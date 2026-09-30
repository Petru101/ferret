// Captures frames of one window chosen by the user through the ScreenCast
// portal, so the tool never sees the rest of the desktop. The choice is kept
// with a restore token, so the picker only shows up the first time.

use std::collections::HashMap;
use std::fs;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{ObjectPath, OwnedFd, OwnedValue, Value};

const DEST: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";
const SOURCE_WINDOW: u32 = 2;
const CURSOR_HIDDEN: u32 = 1;
const PERSIST_UNTIL_REVOKED: u32 = 2;

pub struct WindowCapture {
    conn: Connection,
    session: String,
    node: u32,
    pub size: Option<(i32, i32)>,
}

fn data_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/ferret"))
}

fn token_file() -> PathBuf {
    data_dir().join("screencast-restore-token")
}

fn string_of(v: &Value) -> Option<String> {
    match v {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        _ => None,
    }
}

/// Makes a portal call and waits for its Response signal.
fn request(
    conn: &Connection,
    token: &str,
    call: impl FnOnce(&Proxy) -> zbus::Result<()>,
) -> Result<HashMap<String, OwnedValue>, String> {
    let sender = conn.unique_name().ok_or("no bus name")?.trim_start_matches(':').replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{token}");
    let req = Proxy::new(conn, DEST, path, "org.freedesktop.portal.Request").map_err(|e| e.to_string())?;
    let mut responses = req.receive_signal("Response").map_err(|e| e.to_string())?;
    let portal = Proxy::new(conn, DEST, PATH, "org.freedesktop.portal.ScreenCast").map_err(|e| e.to_string())?;
    call(&portal).map_err(|e| e.to_string())?;
    let msg = responses.next().ok_or("portal closed the request")?;
    let (code, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize().map_err(|e| e.to_string())?;
    match code {
        0 => Ok(results),
        1 => Err("cancelled in the window picker".into()),
        _ => Err(format!("portal request failed (code {code})")),
    }
}

impl WindowCapture {
    pub fn start() -> Result<Self, String> {
        let conn = Connection::session().map_err(|e| e.to_string())?;

        let res = request(&conn, "ferret_create", |p| {
            let opts = HashMap::from([
                ("handle_token", Value::from("ferret_create")),
                ("session_handle_token", Value::from("ferret")),
            ]);
            p.call_method("CreateSession", &(opts,)).map(drop)
        })?;
        let session = res.get("session_handle").and_then(|v| string_of(v)).ok_or("no session handle")?;
        let session_path = ObjectPath::try_from(session.as_str()).map_err(|e| e.to_string())?;

        let saved = fs::read_to_string(token_file()).ok();
        request(&conn, "ferret_select", |p| {
            let mut opts = HashMap::from([
                ("handle_token", Value::from("ferret_select")),
                ("types", Value::from(SOURCE_WINDOW)),
                ("multiple", Value::from(false)),
                ("cursor_mode", Value::from(CURSOR_HIDDEN)),
                ("persist_mode", Value::from(PERSIST_UNTIL_REVOKED)),
            ]);
            if let Some(t) = saved.as_deref() {
                opts.insert("restore_token", Value::from(t.trim()));
            }
            p.call_method("SelectSources", &(&session_path, opts)).map(drop)
        })?;

        let res = request(&conn, "ferret_start", |p| {
            let opts = HashMap::from([("handle_token", Value::from("ferret_start"))]);
            p.call_method("Start", &(&session_path, "", opts)).map(drop)
        })?;
        if let Some(t) = res.get("restore_token").and_then(|v| string_of(v)) {
            let f = token_file();
            fs::create_dir_all(f.parent().unwrap()).ok();
            fs::write(f, t).ok();
        }
        let Some(Value::Array(streams)) = res.get("streams").map(|v| &**v) else {
            return Err("portal returned no stream".into());
        };
        let Some(Value::Structure(stream)) = streams.iter().next() else {
            return Err("portal returned no stream".into());
        };
        let Some(Value::U32(node)) = stream.fields().first() else {
            return Err("stream has no PipeWire node".into());
        };
        let size = match stream.fields().get(1) {
            Some(Value::Dict(props)) => props
                .get::<&str, Value>(&"size")
                .ok()
                .flatten()
                .and_then(|v| match v {
                    Value::Structure(s) => match s.fields() {
                        [Value::I32(w), Value::I32(h)] => Some((*w, *h)),
                        _ => None,
                    },
                    _ => None,
                }),
            _ => None,
        };
        Ok(Self { conn, session, node: *node, size })
    }

    /// Saves the window's current frame as a PNG.
    pub fn grab(&self, out: &Path) -> Result<(), String> {
        let portal = Proxy::new(&self.conn, DEST, PATH, "org.freedesktop.portal.ScreenCast").map_err(|e| e.to_string())?;
        let session = ObjectPath::try_from(self.session.as_str()).map_err(|e| e.to_string())?;
        // Each GStreamer run needs its own PipeWire connection.
        let fd: OwnedFd = portal
            .call("OpenPipeWireRemote", &(&session, HashMap::<&str, Value>::new()))
            .map_err(|e| e.to_string())?;
        let raw = fd.as_raw_fd();
        unsafe { libc::fcntl(raw, libc::F_SETFD, 0) };
        let status = Command::new("timeout")
            .args(["10", "gst-launch-1.0", "-q"])
            .args(["pipewiresrc", &format!("fd={raw}"), &format!("path={}", self.node), "num-buffers=1"])
            .args(["!", "videoconvert", "!", "video/x-raw,format=RGB", "!", "pngenc", "!", "filesink"])
            .arg(format!("location={}", out.display()))
            .status()
            .map_err(|e| e.to_string())?;
        drop(fd);
        if status.success() && out.metadata().map(|m| m.len() > 0).unwrap_or(false) {
            Ok(())
        } else {
            Err(format!("could not grab a frame ({status})"))
        }
    }
}
