// Everything Ferret does, independent of the interface: the host helper,
// window capture and OCR, profiles and limits. Progress messages go to a log
// callback; results come back as values.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::bar::{Gauge, Kind as GaugeKind};
use crate::capture::WindowCapture;
use crate::font::{self, Font};
use crate::i18n::{gettext, n_, ntr, tr};
use crate::ocr::{self, Rect, Shown, Word};
use crate::shapes::{self, Learned, Shape};
use crate::share;

fn flatpak_info(key: &str) -> Option<String> {
    let info = fs::read_to_string("/.flatpak-info").ok()?;
    info.lines().find_map(|l| l.strip_prefix(key)?.strip_prefix('=')).map(str::to_owned)
}

fn flatpak_app_path() -> Option<String> {
    flatpak_info("app-path")
}

const APP_ID: &str = "io.github.Petru101.Ferret";

/// Ferret's version and, in the flatpak, the start of its build's commit ("0.1.0 (3f2a9c1e)").
pub fn version() -> String {
    let version = env!("CARGO_PKG_VERSION");
    match flatpak_info("app-commit") {
        Some(c) => format!("{version} ({})", &c[..c.len().min(8)]),
        None => version.to_owned(),
    }
}

/// Whether a newer build of Ferret was installed since this one started: a running app keeps
/// its build until it's restarted, and an old build drops what it doesn't know when it saves.
/// The running build is `<deploy dir>/<commit>/files`; `<deploy dir>/active` is the installed one.
pub fn newer_install() -> bool {
    let (Some(path), Some(commit)) = (flatpak_app_path(), flatpak_info("app-commit")) else { return false };
    let Some(active) = Path::new(&path).parent().and_then(Path::parent).map(|d| d.join("active")) else { return false };
    let Ok(out) = Command::new("flatpak-spawn").args(["--host", "readlink"]).arg(&active).output() else { return false };
    let installed = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    out.status.success() && !installed.is_empty() && installed != commit
}

/// Starts Ferret again on the host once this instance has left the session bus (a second
/// instance would only hand over to this one); call right before quitting. Detached on the
/// host, so flatpak-spawn returns at once instead of keeping this sandbox alive meanwhile.
pub fn restart_after_quit() -> Result<(), String> {
    let script = format!(
        "for i in $(seq 50); do gdbus call --session --dest org.freedesktop.DBus --object-path /org/freedesktop/DBus \
         --method org.freedesktop.DBus.NameHasOwner {APP_ID} | grep -q true || break; sleep 0.2; done; exec flatpak run {APP_ID}"
    );
    Command::new("flatpak-spawn")
        .args(["--host", "setsid", "-f", "sh", "-c", &script])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn sandbox_report() -> Vec<String> {
    let pids: Vec<String> = fs::read_dir("/proc")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|n| n.chars().all(|c| c.is_ascii_digit()))
                .collect()
        })
        .unwrap_or_default();
    let ns = fs::read_link("/proc/self/ns/pid")
        .map(|p| p.display().to_string())
        .unwrap_or_else(|e| e.to_string());
    vec![
        format!("frontend in flatpak: {}", Path::new("/.flatpak-info").exists()),
        format!("frontend pid namespace: {ns}"),
        format!("processes visible to frontend: {} ({})", pids.len(), pids.join(" ")),
    ]
}

pub fn cache_dir() -> PathBuf {
    let dir = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache/ferret"));
    fs::create_dir_all(&dir).ok();
    dir
}

/// The host helper, started through flatpak-spawn so it can see the game.
struct Helper {
    child: Child,
    input: Option<ChildStdin>,
    output: BufReader<ChildStdout>,
}

impl Helper {
    fn start() -> Result<Self, String> {
        let mut cmd = match flatpak_app_path() {
            Some(app) => {
                let mut c = Command::new("flatpak-spawn");
                c.args(["--host", "--watch-bus", &format!("{app}/bin/ferret-helper")]);
                c
            }
            None => Command::new(std::env::current_exe().map_err(|e| e.to_string())?.with_file_name("ferret-helper")),
        };
        let mut child = cmd
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .map_err(|e| format!("could not start host helper: {e}"))?;
        let input = child.stdin.take();
        let output = BufReader::new(child.stdout.take().unwrap());
        Ok(Self { child, input, output })
    }

    fn call(&mut self, line: &str) -> Vec<String> {
        self.call_with(line, &mut |_| {})
    }

    /// `call`, telling `progress` how far a scan is (0.0..=1.0) as the helper reports it.
    fn call_with(&mut self, line: &str, progress: &mut dyn FnMut(f64)) -> Vec<String> {
        if !self.send(line) {
            return vec!["error: host helper exited".into()];
        }
        self.reply(progress)
    }

    /// `call`, sending the helper a `cancel` line once `cancel` is set meanwhile (it ends a
    /// `sites ... wait`; the helper doesn't answer it).
    fn call_cancellable(&mut self, line: &str, cancel: &Arc<AtomicBool>) -> Vec<String> {
        use std::os::fd::AsFd;
        if !self.send(line) {
            return vec!["error: host helper exited".into()];
        }
        let done = Arc::new(AtomicBool::new(false));
        let watcher = self.input.as_ref().and_then(|i| i.as_fd().try_clone_to_owned().ok()).map(|fd| {
            let (done, cancel) = (done.clone(), cancel.clone());
            std::thread::spawn(move || {
                let mut input = fs::File::from(fd);
                while !done.load(Ordering::Relaxed) {
                    if cancel.load(Ordering::Relaxed) {
                        writeln!(input, "cancel").ok();
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            })
        });
        let reply = self.reply(&mut |_| {});
        done.store(true, Ordering::Relaxed);
        if let Some(w) = watcher {
            w.join().ok();
        }
        reply
    }

    fn send(&mut self, line: &str) -> bool {
        self.input.as_mut().is_some_and(|input| writeln!(input, "{line}").and_then(|_| input.flush()).is_ok())
    }

    /// The reply to the line sent last: its lines up to `end`.
    fn reply(&mut self, progress: &mut dyn FnMut(f64)) -> Vec<String> {
        let mut reply = Vec::new();
        loop {
            let mut l = String::new();
            match self.output.read_line(&mut l) {
                Ok(0) | Err(_) => {
                    reply.push("error: host helper exited".into());
                    return reply;
                }
                Ok(_) if l.trim_end() == "end" => return reply,
                Ok(_) if l.starts_with("progress ") => {
                    let mut n = l.split_whitespace().skip(1).filter_map(|w| w.parse::<f64>().ok());
                    if let (Some(done), Some(total)) = (n.next(), n.next()) {
                        progress(if total > 0.0 { done / total } else { 0.0 });
                    }
                }
                Ok(_) => reply.push(l.trim_end().to_owned()),
            }
        }
    }
}

/// A named path through the engine's own names (Unreal objects, Unity classes, Godot scripts and
/// dictionaries): nothing better to upgrade it to.
fn engine_path(text: &str) -> bool {
    ["ue:", "mono:", "il2cpp:", "gd:", "{"].iter().any(|p| text.starts_with(p))
}

/// A Unity class path: searching for its class and objects takes seconds (9 on Creeper World
/// 4, two passes over its memory), so its code patterns are kept too and tried first (1.2 s
/// there); the name finds it when they stop working (a game update) and gets them traced
/// again.
fn backed_by_code(text: &str) -> bool {
    ["mono:", "il2cpp:"].iter().any(|p| text.starts_with(p))
}

/// Whether a number found through code patterns could be the value: random bytes read as a
/// float are mostly huge or tiny (a pattern that matches in the wrong place after an update).
fn plausible(kind: Kind, v: f64) -> bool {
    match kind {
        Kind::F32 | Kind::F64 => v == 0.0 || (1e-9..1e15).contains(&v.abs()),
        _ => true,
    }
}

struct UpgradeJob {
    pid: u32,
    name: String,
    loc: Loc,
}

/// An engine path that leads to a saved value's place: its text, how many places it leads to
/// and what it means.
struct Upgrade {
    pid: u32,
    name: String,
    loc: Loc,
    text: String,
    places: usize,
    about: String,
}

/// Asks its own helper for engine paths to saved values found some weaker way (a code
/// pattern, pointer paths, a path through a name the game merely holds), away from the
/// worker: a search for a Unity class takes seconds (38 on Particle Fleet).
struct Upgrader {
    jobs: std::sync::mpsc::Sender<UpgradeJob>,
    done: std::sync::mpsc::Receiver<Upgrade>,
}

impl Upgrader {
    fn start() -> Self {
        let (jobs, todo) = std::sync::mpsc::channel::<UpgradeJob>();
        let (found, done) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut helper, mut attached) = (None, 0);
            for job in todo {
                if helper.is_none() {
                    helper = Helper::start().ok();
                }
                let Some(h) = helper.as_mut() else { continue };
                if attached != job.pid {
                    h.call(&format!("attach {}", job.pid));
                    attached = job.pid;
                }
                let reply = h.call(&format!("names {} unreal", job.loc));
                let upgrade = reply.iter().filter_map(|l| l.strip_prefix("named ")).find_map(|line| {
                    let mut f = line.splitn(3, ' ');
                    let (text, _) = (f.next()?, f.next()?);
                    let check = h.call(&format!("named {} {text}", job.loc.kind.name()));
                    let places = parse_exact_values(&check);
                    let fits = !check.iter().any(|l| l.starts_with("doubtful ")) && places.iter().any(|(p, _)| p.addr == job.loc.addr);
                    fits.then(|| Upgrade {
                        pid: job.pid,
                        name: job.name.clone(),
                        loc: job.loc,
                        text: text.to_owned(),
                        places: places.len(),
                        about: f.next().unwrap_or_default().to_owned(),
                    })
                });
                if upgrade.is_some_and(|u| found.send(u).is_err()) {
                    break;
                }
            }
        });
        Upgrader { jobs, done }
    }
}

impl Drop for Helper {
    // Closing its input ends the helper, which also stops any limits.
    fn drop(&mut self) {
        self.input.take();
        self.child.wait().ok();
    }
}

/// Match count from a helper `scan`/`next` reply ("... N matches ...").
fn match_count(reply: &[String]) -> Option<usize> {
    let line = reply.first()?;
    let words: Vec<&str> = line.split_whitespace().collect();
    let i = words.iter().position(|w| *w == "matches")?;
    words.get(i.checked_sub(1)?)?.parse().ok()
}

/// What Ferret is busy with and whether the player has something to do, for the interface
/// (the player watches the game, and took a small spinner for done more than once).
pub enum Phase {
    /// Scanning all memory for the shown number: how far (0.0..=1.0). The number mustn't change.
    Scanning(String, f64),
    /// The search has this many places and follows the number from here on.
    Ready(usize),
    /// A change on screen narrowed the search down to this many places.
    Watching(usize),
    /// Scan Again while Start runs: the places before and after.
    ScannedAgain(usize, usize),
    /// Testing which of this many places is the value: how far. Nothing to do.
    Checking(usize, f64),
    /// The player has to change the number in the game: this many places left, seconds to do it.
    YourTurn(usize, u64),
    /// Saving: nothing touched the value yet, so the player changes it in the game while Ferret
    /// watches which code does. Until it does or the player cancels.
    SaveTurn,
    /// Finding saved values again on attach: the game's program, the value, its number and how
    /// many there are.
    Restoring(String, String, usize, usize),
    /// The watched box stopped showing the number for the third time since the pick.
    BoxChanging,
    /// Copying the game's memory for a bar's search: how far. The bar mustn't change.
    Copying(f64),
    /// The copy is taken: the bar has to change in the game before anything narrows down.
    BarReady,
}

/// One of the places still matching a search, as the matches list shows it.
#[derive(Clone, Debug)]
pub struct Match {
    pub loc: Loc,
    /// Its value as the game holds it.
    pub value: String,
    /// What it is, when Ferret can tell (an entry of a Godot dictionary: its key and ids).
    pub about: Option<String>,
    /// Already saved as a value of this game.
    pub saved: bool,
}

/// How the game stores a value.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Kind {
    I32,
    F32,
    F64,
    /// XOR-encoded 4-byte integer (the next word is the key).
    Xor,
    /// 2-byte unsigned integer (Frostbite's ammo).
    U16,
}

impl Kind {
    pub const ALL: [Kind; 5] = [Kind::I32, Kind::F32, Kind::F64, Kind::Xor, Kind::U16];

    pub fn name(self) -> &'static str {
        match self {
            Kind::I32 => "i32",
            Kind::F32 => "f32",
            Kind::F64 => "f64",
            Kind::Xor => "xor",
            Kind::U16 => "u16",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.name() == s)
    }

    fn with_article(self) -> String {
        let d = self.describe();
        format!("{} {d}", if d.starts_with(['a', 'e', 'i', 'o', 'u']) { "an" } else { "a" })
    }

    /// Whole numbers in memory (a game may still show them with decimals: 12 as "1.2").
    pub fn whole(self) -> bool {
        matches!(self, Kind::I32 | Kind::Xor | Kind::U16)
    }

    pub fn describe(self) -> &'static str {
        match self {
            Kind::I32 => "whole number",
            Kind::F32 => "float",
            Kind::F64 => "double",
            Kind::Xor => "encoded whole number",
            Kind::U16 => "2-byte whole number",
        }
    }
}

/// Where a value is and how it is stored; shown the way the helper takes it: "<hex>:<type>".
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Loc {
    pub addr: u64,
    pub kind: Kind,
}

impl std::fmt::Display for Loc {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{:x}:{}", self.addr, self.kind.name())
    }
}

pub fn parse_loc(a: &str) -> Option<Loc> {
    let (addr, kind) = a.split_once(':').map_or((a, Some(Kind::I32)), |(a, k)| (a, Kind::parse(k)));
    Some(Loc { addr: u64::from_str_radix(addr.trim_start_matches("0x"), 16).ok()?, kind: kind? })
}

/// Parses helper lines of the form "0x00003795366c:f32 = 96.5".
fn parse_exact_values(lines: &[String]) -> Vec<(Loc, Option<f64>)> {
    lines
        .iter()
        .filter_map(|l| {
            let (a, v) = l.split_once(" = ")?;
            Some((parse_loc(a)?, v.trim().parse::<f64>().ok().filter(|v| v.is_finite())))
        })
        .collect()
}

/// The same, with values as whole numbers, cut off the way games usually display them.
/// Whether a value in memory is what the screen shows: within 1 (the screen lags), or a decimal
/// kept as a whole number of tenths (12 for "1.2").
fn shows(v: i64, n: &Shown) -> bool {
    (v as f64 - n.value()).abs() <= 1.0 || n.decimals() > 0 && (v - n.scaled()).abs() <= 1
}

fn parse_values(lines: &[String]) -> Vec<(Loc, Option<i64>)> {
    parse_exact_values(lines).into_iter().map(|(loc, v)| (loc, v.map(|v| (v + 1e-6).floor() as i64))).collect()
}

/// A number the player typed: "1.5", "-3", "1,250" (thousands), "1,5" (a decimal comma).
pub fn parse_number(text: &str) -> Option<f64> {
    let t = text.trim().replace([' ', '_'], "");
    let groups: Vec<&str> = t.split(',').collect();
    let t = if t.contains('.') || groups[1..].iter().all(|g| g.len() == 3) {
        t.replace(',', "")
    } else if groups.len() == 2 {
        t.replace(',', ".")
    } else {
        return None;
    };
    t.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// A value as Ferret shows it: `decimals` for whole numbers the game shows with decimals
/// (none for plain ones), and floats (`None`) with up to 3 decimals ("96.5", "100").
pub fn number_text(v: f64, decimals: Option<u32>) -> String {
    match decimals {
        Some(d) => format!("{v:.*}", d as usize),
        None => {
            let s = format!("{v:.3}");
            let s = s.trim_end_matches('0').trim_end_matches('.');
            if s == "-0" { "0" } else { s }.to_owned()
        }
    }
}

/// Why attaching was blocked by kernel.yama.ptrace_scope and how to allow it. Lines starting
/// with two spaces are commands (the GUI shows them selectable).
fn ptrace_help(scope: &str) -> String {
    let now = format!("{}\n  sudo sysctl kernel.yama.ptrace_scope=0\n", tr!("To allow it until the next restart, run this in a terminal:"));
    let keep = "  echo kernel.yama.ptrace_scope=0 | sudo tee /etc/sysctl.d/60-ptrace.conf";
    let risk = tr!("This lets any program you run read and change your other programs' memory.");
    let keep_it = tr!("To keep it that way:");
    match scope {
        "1" => format!(
            "{}\n{now}{keep_it}\n{keep}\n{risk}",
            tr!(
                "Your system only lets programs change the memory of programs they started themselves \
                 (kernel.yama.ptrace_scope is 1). Windows games running through Proton or Wine still work."
            )
        ),
        "2" => format!(
            "{}\n{now}{keep_it}\n{keep}\n{risk}",
            tr!("Your system only lets administrators change other programs' memory (kernel.yama.ptrace_scope is 2).")
        ),
        _ => format!(
            "{}\n{}\n{keep}\n{risk}",
            tr!(
                "Your system has turned off changing other programs' memory until it restarts (kernel.yama.ptrace_scope is {scope}).",
                scope
            ),
            tr!("To allow it, run this in a terminal and restart:")
        ),
    }
}

/// Tells the shared library whether an upload worked, in the background (the server may be slow
/// or away; nothing waits for it).
fn vote(id: String, worked: bool) {
    std::thread::spawn(move || {
        if let Err(e) = crate::library::vote(&id, worked) {
            eprintln!("vote on {id}: {e}");
        }
    });
}

fn first_error(reply: &[String]) -> Option<String> {
    reply.iter().find_map(|l| l.strip_prefix("error: ")).map(str::to_owned)
}

// --- Profiles: named values that survive game restarts, one file per game.

fn profile_path(exe: &str) -> PathBuf {
    let base = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share/ferret"));
    base.join("profiles").join(format!("{}.profile", exe.to_lowercase()))
}

/// The game's saved values as the profile holds them (feedback.rs sends them with a report).
pub fn saved_text(exe: &str) -> Option<String> {
    fs::read_to_string(profile_path(exe)).ok()
}

/// "one kind of place", "3 kinds of places".
pub fn kinds_of_places(n: usize) -> String {
    if n == 1 { "one kind of place".into() } else { format!("{n} kinds of places") }
}

/// The game's learned digit shapes, next to its profile.
fn digits_path(exe: &str) -> PathBuf {
    profile_path(exe).with_extension("digits")
}

/// The memory around the game's values found so far, next to its profile.
fn shapes_path(exe: &str) -> PathBuf {
    profile_path(exe).with_extension("shapes")
}

#[derive(Clone)]
struct Entry {
    name: String,
    kind: Kind,
    sites: Vec<String>,
    /// Pointer paths that led to the value in more than one run of the game,
    /// "<module>+<offset> <offset>...".
    paths: Vec<String>,
    /// Pointer paths from a scan, best first, not confirmed by another run yet: most of them
    /// go through things that change when the game restarts. Saving the value again in a
    /// later run keeps the ones that still lead to it as `paths`.
    candidates: Vec<String>,
    /// The game process the candidates were found in.
    run: Option<u32>,
    /// Named paths (helper/src/names.rs): from an object the game names to every place the
    /// value is kept, like every wood stack in Valheim's inventory.
    named: Vec<String>,
    /// "<min|-> <max|->" when the value is kept within a range, as the game keeps it (12 for a
    /// "1.2" kept in tenths).
    limit: Option<String>,
    /// How many decimals the game shows a whole number with: 1 = kept in tenths ("1.2" is 12).
    decimals: u32,
    /// The value (in the game's units) last seen in a place Ferret trusted, kept while its
    /// pointer paths are unconfirmed: after a restart, the one place they lead to that still
    /// holds it is the value (games keep it in their save), not what most paths agree on
    /// (Andromeda: 461 of 2951 led to a 1, the 32 right ones to the credits).
    last: Option<f64>,
    /// The game's build its code patterns were traced or last checked in (helper `build`):
    /// after an update they are checked against its name before being trusted.
    build: Option<String>,
    /// The game's build in which tracing a value found by name saw nothing touch it (ULTRAKILL's
    /// health while the player isn't hit): not traced again at restores until the next build
    /// (saving it again traces it).
    untraced: Option<String>,
    /// Came from a `.ferret` file (share.rs) and the player hasn't said yet that the game shows
    /// the number it leads to: never written, no limit, until they do (`confirm_paths`).
    imported: bool,
    /// "<min|-> <max|->": a range the import suggested, only filled into the Values tab's fields.
    suggested: Option<String>,
    /// The shared library's upload it was imported from (library.rs): told whether it worked
    /// when the player confirms it or removes it unconfirmed.
    from: Option<String>,
    /// Lines this build doesn't understand (from a newer one), saved back unchanged.
    other: Vec<String>,
}

/// Names are one lowercase word everywhere (helper commands, CLI): "Max HP" is saved as
/// "max_hp", so saving over a value doesn't depend on remembering how it was capitalised.
/// The longest name a value can have (the shared library's limit too: names show in its list).
pub const MAX_NAME: usize = 32;

pub fn one_word(name: &str) -> String {
    name.split_whitespace().collect::<Vec<_>>().join("_").to_lowercase()
}

/// Profile format: "entry <name>" followed by an optional "type f32|f64|xor" line (i32 when
/// missing), its "site ...", "path ...", "named ..." and "candidate ..." lines, "run <pid>" (where the
/// candidates came from), an optional "limit <min|-> <max|->" line and "decimals <n>" for a
/// whole number shown with decimals, "last <value>" while its pointer paths are unconfirmed,
/// "build <stamp>" with code patterns, "untraced <stamp>", "imported", "suggest <min|-> <max|->"
/// and "from <upload id>" (see Entry).
/// Other lines are kept
/// with their entry, so a build older than the profile doesn't drop what it doesn't know.
fn read_profile(exe: &str) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for line in fs::read_to_string(profile_path(exe)).unwrap_or_default().lines() {
        if let Some(name) = line.strip_prefix("entry ") {
            // Older builds kept capitals ("Gems" and "gems" both saved): the newer one wins.
            let name = one_word(name);
            entries.retain(|e| e.name != name);
            entries.push(Entry {
                name,
                kind: Kind::I32,
                sites: Vec::new(),
                paths: Vec::new(),
                candidates: Vec::new(),
                run: None,
                named: Vec::new(),
                limit: None,
                decimals: 0,
                last: None,
                build: None,
                untraced: None,
                imported: false,
                suggested: None,
                from: None,
                other: Vec::new(),
            });
        } else if let (Some(kind), Some(e)) = (line.strip_prefix("type ").and_then(|k| Kind::parse(k.trim())), entries.last_mut()) {
            e.kind = kind;
        } else if let (Some(site), Some(e)) = (line.strip_prefix("site "), entries.last_mut()) {
            e.sites.push(site.trim().to_owned());
        } else if let (Some(path), Some(e)) = (line.strip_prefix("path "), entries.last_mut()) {
            e.paths.push(path.trim().to_owned());
        } else if let (Some(path), Some(e)) = (line.strip_prefix("candidate "), entries.last_mut()) {
            e.candidates.push(path.trim().to_owned());
        } else if let (Some(path), Some(e)) = (line.strip_prefix("named "), entries.last_mut()) {
            e.named.push(path.trim().to_owned());
        } else if let (Some(pid), Some(e)) = (line.strip_prefix("run "), entries.last_mut()) {
            e.run = pid.trim().parse().ok();
        } else if let (Some(limit), Some(e)) = (line.strip_prefix("limit "), entries.last_mut()) {
            e.limit = Some(limit.trim().to_owned());
        } else if let (Some(d), Some(e)) = (line.strip_prefix("decimals ").and_then(|d| d.trim().parse().ok()), entries.last_mut()) {
            e.decimals = d;
        } else if let (Some(v), Some(e)) = (line.strip_prefix("last ").and_then(|v| v.trim().parse().ok()), entries.last_mut()) {
            e.last = Some(v);
        } else if let (Some(b), Some(e)) = (line.strip_prefix("build "), entries.last_mut()) {
            e.build = Some(b.trim().to_owned());
        } else if let (Some(b), Some(e)) = (line.strip_prefix("untraced "), entries.last_mut()) {
            e.untraced = Some(b.trim().to_owned());
        } else if let ("imported", Some(e)) = (line.trim(), entries.last_mut()) {
            e.imported = true;
        } else if let (Some(l), Some(e)) = (line.strip_prefix("suggest "), entries.last_mut()) {
            e.suggested = Some(l.trim().to_owned());
        } else if let (Some(id), Some(e)) = (line.strip_prefix("from "), entries.last_mut()) {
            e.from = Some(id.trim().to_owned());
        } else if let (false, Some(e)) = (line.trim().is_empty(), entries.last_mut()) {
            e.other.push(line.to_owned());
        }
    }
    entries
}

fn write_profile(exe: &str, entries: &[Entry]) -> Result<PathBuf, String> {
    let path = profile_path(exe);
    fs::create_dir_all(path.parent().unwrap()).map_err(|e| e.to_string())?;
    let mut text = String::new();
    for e in entries {
        text.push_str(&format!("entry {}\n", e.name));
        if e.kind != Kind::I32 {
            text.push_str(&format!("type {}\n", e.kind.name()));
        }
        for s in &e.sites {
            text.push_str(&format!("site {s}\n"));
        }
        for p in &e.paths {
            text.push_str(&format!("path {p}\n"));
        }
        for p in &e.named {
            text.push_str(&format!("named {p}\n"));
        }
        if let Some(pid) = e.run {
            text.push_str(&format!("run {pid}\n"));
        }
        for p in &e.candidates {
            text.push_str(&format!("candidate {p}\n"));
        }
        if let Some(l) = &e.limit {
            text.push_str(&format!("limit {l}\n"));
        }
        if e.decimals > 0 {
            text.push_str(&format!("decimals {}\n", e.decimals));
        }
        if let Some(v) = e.last.filter(|_| e.unconfirmed()) {
            text.push_str(&format!("last {v}\n"));
        }
        if let Some(b) = e.build.as_ref().filter(|_| !e.sites.is_empty()) {
            text.push_str(&format!("build {b}\n"));
        }
        if let Some(b) = &e.untraced {
            text.push_str(&format!("untraced {b}\n"));
        }
        if e.imported {
            text.push_str("imported\n");
        }
        if let Some(l) = &e.suggested {
            text.push_str(&format!("suggest {l}\n"));
        }
        if let Some(id) = &e.from {
            text.push_str(&format!("from {id}\n"));
        }
        for l in &e.other {
            text.push_str(&format!("{l}\n"));
        }
    }
    fs::write(&path, text).map_err(|e| e.to_string())?;
    Ok(path)
}

/// How many leading parts (start, then offsets) a path has in common with the closest of `known`.
fn shared_start(path: &str, known: &[&String]) -> usize {
    let common = |k: &String| path.split_whitespace().zip(k.split_whitespace()).take_while(|(a, b)| a == b).count();
    known.iter().map(|k| common(k)).max().unwrap_or(0)
}

/// A game's values mostly hang off the same few structures (in GameMaker games, its variable
/// list): unconfirmed paths starting the way another value's confirmed ones do (the same start
/// and at least two offsets) are much more likely to be the real ones.
const SHARED_START: usize = 3;

impl Entry {
    /// Confirmed paths of the game's other values.
    fn known<'a>(&self, all: &'a [Entry]) -> Vec<&'a String> {
        all.iter().filter(|e| e.name != self.name).flat_map(|e| &e.paths).collect()
    }

    /// Candidates that start like other values' confirmed paths.
    fn likely<'a>(&'a self, all: &[Entry]) -> Vec<&'a String> {
        let known = self.known(all);
        self.candidates.iter().filter(|p| shared_start(p, &known) >= SHARED_START).collect()
    }

    /// The pointer paths to follow, as the helper takes them: the confirmed ones, else the
    /// candidates that start like other values' confirmed paths, else all candidates.
    fn followed(&self, all: &[Entry]) -> Vec<String> {
        if !self.paths.is_empty() {
            return self.paths.iter().map(|p| path_arg(p)).collect();
        }
        let likely = self.likely(all);
        let paths = if likely.is_empty() { self.candidates.iter().collect() } else { likely };
        paths.into_iter().map(|p| path_arg(p)).collect()
    }

    /// Followed through any of its thousands of unconfirmed paths in a later run than the one
    /// it was saved in: several of them agreeing proves nothing (Valheim: 6 of 2998 led to an
    /// unrelated 21, and a write went there).
    fn guessed(&self, all: &[Entry], pid: u32) -> bool {
        self.unconfirmed() && self.run != Some(pid) && self.likely(all).is_empty()
    }

    /// Saved by a newer build with a value type this one doesn't know: reading or writing it
    /// as an i32 would be wrong.
    fn foreign_type(&self) -> Option<&str> {
        self.other.iter().find_map(|l| l.strip_prefix("type ")).map(str::trim)
    }

    fn unconfirmed(&self) -> bool {
        self.paths.is_empty() && !self.candidates.is_empty()
    }

    /// What the game keeps per shown unit: 10 for a value shown as "1.2" and kept as 12.
    fn scale(&self) -> f64 {
        if self.kind.whole() { 10f64.powi(self.decimals as i32) } else { 1.0 }
    }

    /// Decimals the value is shown with; `None` for floats (any).
    fn shown_decimals(&self) -> Option<u32> {
        self.kind.whole().then_some(self.decimals)
    }

    /// A number as the player sees it, in the game's units; refuses decimals it can't keep.
    fn to_memory(&self, v: f64) -> Result<f64, String> {
        let m = v * self.scale();
        if self.kind.whole() && (m - m.round()).abs() > 1e-6 {
            return Err(match self.decimals {
                0 => format!("{} only takes whole numbers", self.name),
                1 => format!("{} takes one decimal at most", self.name),
                d => format!("{} takes {d} decimals at most", self.name),
            });
        }
        Ok(if self.kind.whole() { m.round() } else { m })
    }

    /// The saved range as the player sees it.
    fn shown_range(&self) -> (Option<f64>, Option<f64>) {
        let (min, max) = self.limit.as_deref().map_or((None, None), parse_range);
        (min.map(|v| v / self.scale()), max.map(|v| v / self.scale()))
    }
}

fn parse_range(limit: &str) -> (Option<f64>, Option<f64>) {
    let mut f = limit.split_whitespace().map(|v| v.parse().ok().filter(|v: &f64| v.is_finite()));
    (f.next().flatten(), f.next().flatten())
}

fn range_text(min: Option<f64>, max: Option<f64>) -> String {
    // Rounded so float noise (1.2000000000000002) doesn't end up in the profile.
    let show = |b: Option<f64>| b.map_or("-".to_owned(), |b| ((b * 1e6).round() / 1e6).to_string());
    format!("{} {}", show(min), show(max))
}

pub fn limit_text(min: Option<f64>, max: Option<f64>) -> String {
    let (min, max) = (min.map(|v| number_text(v, None)), max.map(|v| number_text(v, None)));
    match (min, max) {
        (None, Some(max)) => format!("at most {max}"),
        (Some(min), None) => format!("at least {min}"),
        (Some(min), Some(max)) => format!("between {min} and {max}"),
        (None, None) => "unlimited".into(),
    }
}

// --- What the interfaces get back.

pub struct GameProcess {
    pub pid: u32,
    pub exe: String,
    pub app_id: Option<String>,
    /// Why Ferret won't attach to it.
    pub refused: Option<Refusal>,
    /// The name Steam gives the game.
    pub name: Option<String>,
    /// Its store lists it as played online or with others (and as single-player: else it's refused).
    pub multiplayer: bool,
    /// Anti-cheat it ships or is listed with, not running.
    pub ships: Option<String>,
}

/// Why Ferret won't attach to a game (the helper's verdict).
#[derive(Clone, PartialEq, Debug)]
pub enum Refusal {
    /// Loaded in the game or running beside it.
    AntiCheat(String),
    OnlineOnly,
    /// Steam lists Valve Anti-Cheat.
    Vac,
    /// Its characters get flagged online for edits made offline.
    FlagsEdits,
}

impl Refusal {
    fn parse(code: &str) -> Option<Refusal> {
        Some(match code {
            "-" => return None,
            "online" => Refusal::OnlineOnly,
            "vac" => Refusal::Vac,
            "edits" => Refusal::FlagsEdits,
            // An unknown reason from a newer helper still refuses.
            other => Refusal::AntiCheat(other.strip_prefix("anti-cheat ").unwrap_or(other).to_owned()),
        })
    }
}

/// How the attached game went away.
pub enum Gone {
    /// It quit: its program name.
    Quit(String),
    /// Anti-cheat started in it: its program name and the anti-cheat. Ferret let go.
    AntiCheat(String, String),
}

pub struct ValueRow {
    pub name: String,
    pub addr: u64,
    pub kind: Kind,
    /// The value, min and max as the game shows them (1.2 for a value kept as 12).
    pub value: Option<f64>,
    pub min: Option<f64>,
    pub max: Option<f64>,
    /// Decimals the game shows it with (0 = whole number); `None` for floats.
    pub decimals: Option<u32>,
    /// "fixed N times, restored M times, active" while a limit is enforced.
    pub limit_state: Option<String>,
    /// Found through pointer paths no other run of the game has confirmed yet.
    pub unconfirmed: bool,
    /// Why Ferret won't write it right now: its pointer paths don't agree on where it is.
    pub doubtful: Option<String>,
    /// Places it is kept in (a named path to all of an item's stacks); setting it sets each.
    pub places: usize,
    /// Saved through pointer paths in an earlier run that this run hasn't confirmed: the player
    /// can confirm them by saying the game shows `value` (`Core::confirm_paths`).
    pub confirmable: bool,
    /// Shown only, never written (Java games: see `JAVA_WRITE`).
    pub read_only: bool,
    /// Came from a shared file and isn't confirmed by the player yet (`confirmable` asks).
    pub imported: bool,
    /// A range the import suggested (as the game shows it), for the fields only.
    pub suggested: (Option<f64>, Option<f64>),
}

/// A game's saved values as a `.ferret` file.
pub struct Export {
    pub text: String,
    /// "<program>.ferret".
    pub file_name: String,
    pub count: usize,
    /// Values left out, and why.
    pub left_out: Vec<String>,
}

/// What an import added (unconfirmed) and skipped.
pub struct Imported {
    pub added: Vec<String>,
    /// Values not taken, and why.
    pub skipped: Vec<String>,
    /// The file was made with another build of the game: some may not be found.
    pub other_build: bool,
}

/// Why saved values of Java games are only shown.
fn java_write() -> String {
    tr!("Java game: Ferret only shows saved values. Java moves its objects around, so a later write could land in another object and crash the game. Change it from the Find tab while you search instead.")
}

pub enum AutoResult {
    /// One address left: the value.
    Found(Loc),
    /// Several addresses still follow the value and none could be confirmed.
    Several(usize),
    /// One address left that may not be the value, and why: kept for the player to try (it
    /// doesn't hold the number on screen, or the game put a test value back). The player knows
    /// better than a misread or a guess (MEA's ammo: "1/116" on screen, the place held 166).
    Unsure(String),
    /// Several addresses follow the value, and the game put back the test value Ferret wrote
    /// to each of them at once: a number the game works out from others (Creeper World 3's
    /// energy per second, `stats_energyProductionVal`), copies of a value kept in a form Ferret
    /// can't search for, or a value at its cap (only higher test values are written).
    PutBack(usize),
}

/// How `probe` ended.
pub enum Probed {
    Found(Loc),
    /// Every candidate put its test value back at once.
    PutBack,
    /// Nothing told them apart.
    Unclear,
}

struct Game {
    pid: u32,
    exe: String,
    /// A memory scrambler the game uses (CodeStage Anti-Cheat Toolkit): its numbers are hidden.
    scrambler: Option<String>,
    entries: Vec<(String, Loc)>,
    /// Values found through pointer paths (as the helper takes them), followed again on every
    /// look: the game may have moved them.
    paths: Vec<(String, Vec<String>)>,
    /// How those paths agreed when last followed.
    votes: Vec<(String, Votes)>,
    /// How often each value's confirmed paths led to it this run (see `Steady`).
    steady: Vec<(String, Steady)>,
    /// Values found through code patterns whose object the game reads from a static pointer:
    /// that pointer as a path (good for this run only), for the helper to follow.
    via: Vec<(String, String)>,
    /// Values found through a named path: every place it leads to now (all of them are set).
    named: Vec<(String, String, Vec<Loc>)>,
    /// Named values whose places aren't one value now (the helper's "doubtful" line): never
    /// written.
    named_doubt: Vec<(String, String)>,
    /// Values already handed to the upgrader in this run.
    upgrade_asked: Vec<String>,
    /// When `last` values were last written to the profile.
    last_written: Option<Instant>,
    /// The game's build stamp (helper `build`), when it could be read.
    build: Option<String>,
    /// Its Steam app id, when it has one: the shared library tells games with the same program
    /// name apart by it.
    steam: Option<String>,
    /// A Java game (the helper's attach says so): nothing is saved or restored there.
    java: bool,
}

/// How long a restore waits for the code of values saved by code pattern (all of them at once,
/// 4 instructions at a time). Values whose code doesn't run meanwhile (ammo: only when firing;
/// a Mono game compiles code when it first runs it, Particle Fleet's omni isn't there at the
/// main menu) are found by the helper in the background.
const RESTORE_WAIT: Duration = Duration::from_secs(2);

/// How a code pattern's miss starts when what it found can't be the value.
const IMPLAUSIBLE: &str = "it read a number no game keeps";
/// The helper's reason when a value's code didn't run while it waited.
const NOT_RUN: &str = "none of its code ran meanwhile";

/// How often values seen in trusted places are written to the profile as `last`.
const LAST_EVERY: Duration = Duration::from_secs(10);

/// A value whose number has at least this many digits as shown (100 and up) is distinctive
/// enough for the one place holding it to confirm pointer paths without the player.
const DISTINCTIVE: f64 = 100.0;

/// Two values of a type that are the same number (floats: up to rounding).
fn same_value(kind: Kind, a: f64, b: f64) -> bool {
    if kind.whole() { (a - b).abs() < 0.5 } else { (a - b).abs() <= 1e-4 * b.abs().max(1.0) }
}

/// How a value's pointer paths agreed when last followed (the helper's "votes" line).
#[derive(Clone, Copy)]
struct Votes {
    agree: usize,
    /// Paths leading to the most common other address.
    elsewhere: usize,
    total: usize,
    /// Clear enough to write there (see helper/src/pointers.rs `Vote::clear`).
    clear: bool,
}

impl Votes {
    fn parse(reply: &[String]) -> Option<Votes> {
        let line = reply.iter().find_map(|l| l.strip_prefix("votes "))?;
        let f: Vec<&str> = line.split_whitespace().collect();
        let [agree, elsewhere, total, clear] = f[..] else { return None };
        Some(Votes { agree: agree.parse().ok()?, elsewhere: elsewhere.parse().ok()?, total: total.parse().ok()?, clear: clear == "clear" })
    }

    fn doubt(&self) -> String {
        if self.elsewhere > 0 {
            tr!(
                "its pointer paths disagree ({agree} of {total} lead to one place, {elsewhere} to another)",
                agree = self.agree,
                total = self.total,
                elsewhere = self.elsewhere
            )
        } else {
            ntr!(
                "only {agree} of its {n} pointer path leads anywhere",
                "only {agree} of its {n} pointer paths leads anywhere",
                self.total,
                agree = self.agree
            )
        }
    }
}

/// How often a value's confirmed pointer paths led to it during a run. Paths starting in deep
/// stack frames lead there only some of the time (Age of War: 65-94 of 247 on a clear vote,
/// once 28 elsewhere), so they are dropped once the value has been followed long enough: only
/// paths that led to it every time (`STEADY_SHARE`) are kept. Counted only while the paths
/// agree clearly on a place where the game has changed the value (before it exists, at a
/// menu, they may agree on another place).
#[derive(Default)]
struct Steady {
    /// The place they agree on, and the value first seen there until the game changes it.
    place: u64,
    first: Option<f64>,
    changed: bool,
    /// Looks since then, and how many of them each path led there.
    looks: u32,
    led: Vec<u32>,
    /// Already pruned (or found nothing to prune) this run.
    done: bool,
}

/// Looks at a value (the Values tab: one a second) before its unsteady paths are dropped.
const STEADY_LOOKS: u32 = 60;
/// Share of those looks a kept path led to the value (a few misses while the game changes
/// what it shows).
const STEADY_SHARE: f64 = 0.95;

/// A search that ended on what looks like a display copy.
fn copy_only() -> String {
    tr!(
        "The one place left put a test value back at once, like a copy the game redraws its display \
         from; the game may keep the value in a form Ferret can't search for yet (a 2-byte number or an encoded one)."
    )
}

/// Most pointer paths kept from a scan. The real one can rank far down (Forager's gems: 590th
/// of 81235), and only a later run tells which it is.
const MAX_CANDIDATES: usize = 3000;

/// A profile path line as the helper takes it: "Forager.exe+177a64c 44 2c" -> "Forager.exe+177a64c,44,2c".
fn path_arg(p: &str) -> String {
    p.split_whitespace().collect::<Vec<_>>().join(",")
}

/// Test values a probe gives its candidates are this far apart; the game changing one by up to
/// `PROBE_NEAR` still counts as carrying on from it.
const PROBE_STEP: i64 = 100;
const PROBE_NEAR: i64 = 49;

/// A candidate's test write: its exact original and its test value.
struct Probe {
    loc: Loc,
    orig: String,
    test: i64,
    step: i64,
}

pub struct Core {
    helper: Helper,
    /// Looks for better ways to find saved values, in the background (started when needed).
    upgrader: Option<Upgrader>,
    capture: Option<WindowCapture>,
    words: Vec<Word>,
    area: Option<Rect>,
    /// How the watched area looked when it was picked: reads and learning only happen while it
    /// still looks like that. `hidden` = it doesn't now (told to the player once); `hides` =
    /// how often that happened since the pick (often: the box takes in something that moves).
    picked_look: Option<ocr::Look>,
    hidden: bool,
    hides: u32,
    /// The last read of the box found a number: the first frame after it that reads none is
    /// kept as `unread.png`, to replay misses.
    was_read: bool,
    game: Option<Game>,
    /// The attached game's digits, as learned so far.
    font: Font,
    /// The search in progress (Start or typed numbers, which continue each other): matches
    /// left, and how many typed numbers in a row left the count unchanged. None = the next
    /// number starts a new scan.
    search: Option<(usize, usize)>,
    /// Decimals of the number the search last went by: a whole number found from "1.2" is
    /// kept in tenths, and saved that way.
    searched_decimals: u32,
    log: Box<dyn FnMut(&str) + Send>,
    /// Set to stop a running `auto`.
    pub cancel: Arc<AtomicBool>,
    /// Set to have a running `auto` narrow down by the number on screen now, even when it
    /// didn't change (places that changed meanwhile are other things).
    pub scan_now: Arc<AtomicBool>,
    /// Told about every frame the watched number is read from, and where the watched area is
    /// after it (the interface shows the game as the search sees it).
    pub on_frame: Option<Box<dyn FnMut(&Path, Option<Rect>) + Send>>,
    /// Told what each read of the watched area saw: the number, or None (nothing readable, or
    /// the box doesn't show the number now).
    pub on_read: Option<Box<dyn FnMut(Option<&Shown>) + Send>>,
    /// Told when the player has to do something for the search to go on.
    pub on_status: Option<Box<dyn FnMut(&str) + Send>>,
    /// Told about the places still matching while a search narrows them down (20 or fewer).
    pub on_matches: Option<Box<dyn FnMut(Vec<Match>) + Send>>,
    /// Told what a search or a restore is doing, and when the player has to act.
    pub on_phase: Option<Box<dyn FnMut(Phase) + Send>>,
    /// The value types new scans look for (empty: all of them).
    pub scan_kinds: Vec<Kind>,
    /// Jobs the interface needs done while a search runs (a limit, a value set, the limits
    /// hotkey): run between its reads. Before, they waited for the search to end, and a limit
    /// turned off during one kept holding the value (Lumencraft's stamina).
    pub side_jobs: Option<std::sync::mpsc::Receiver<SideJob>>,
    /// The memory around the game's values found so far: new scans try places shaped like them
    /// first.
    shapes: Vec<Shape>,
    /// The search in progress kept only places shaped like an earlier find.
    shaped: bool,
    /// The value wasn't in such a place: scans look everywhere until the next search.
    unshaped: bool,
    /// The picked box is a bar or a row of icons, searched by how full it is.
    bar: Option<Gauge>,
    /// How full the bar was when the matches' values were taken (None: the search has a copy
    /// of the game's memory to compare with, or none at all).
    bar_last: Option<(f64, f64)>,
    /// The last place tried that kept its test number: where, what it held before, the number
    /// written (`put_back_try`).
    last_try: Option<(Loc, f64, f64)>,
    log_name: &'static str,
}

/// A job from the interface run in the middle of a long one (see `Core::side_jobs`).
pub type SideJob = Box<dyn FnOnce(&mut Core) + Send>;

fn cancelled_now(c: &AtomicBool) -> bool {
    c.load(Ordering::Relaxed)
}

/// Logs of earlier runs kept next to the current one.
const OLD_LOGS: usize = 5;

impl Core {
    /// `log_name`: the file in the cache folder the log is also kept in. The last few runs'
    /// logs are kept as `<name>.1` and on (a test run after the player's would lose theirs).
    pub fn new(log: Box<dyn FnMut(&str) + Send>, log_name: &'static str) -> Result<Self, String> {
        let file = |i: usize| cache_dir().join(if i == 0 { log_name.to_owned() } else { format!("{log_name}.{i}") });
        for i in (0..OLD_LOGS).rev() {
            fs::rename(file(i), file(i + 1)).ok();
        }
        fs::write(file(0), "").ok();
        Ok(Self {
            log_name,
            helper: Helper::start()?,
            upgrader: None,
            capture: None,
            words: Vec::new(),
            area: None,
            picked_look: None,
            hidden: false,
            hides: 0,
            was_read: false,
            game: None,
            font: Font::default(),
            search: None,
            searched_decimals: 0,
            log,
            cancel: Arc::new(AtomicBool::new(false)),
            scan_now: Arc::new(AtomicBool::new(false)),
            on_frame: None,
            on_read: None,
            on_status: None,
            on_matches: None,
            on_phase: None,
            scan_kinds: Vec::new(),
            shapes: Vec::new(),
            shaped: false,
            unshaped: false,
            side_jobs: None,
            bar: None,
            bar_last: None,
            last_try: None,
        })
    }

    /// Something the player has to do now (shown in the interface, not only logged).
    fn status(&mut self, msg: &str) {
        if let Some(f) = self.on_status.as_mut() {
            f(msg);
        }
    }

    /// Runs the interface's jobs that came in meanwhile (`side_jobs`).
    pub fn run_side_jobs(&mut self) {
        let Some(rx) = self.side_jobs.take() else { return };
        while let Ok(job) = rx.try_recv() {
            job(self);
        }
        self.side_jobs = Some(rx);
    }

    pub fn say(&mut self, msg: &str) {
        (self.log)(msg);
        // Also kept on disk, for looking into problems after the fact.
        if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(cache_dir().join(self.log_name)) {
            let _ = writeln!(f, "{msg}");
        }
    }

    /// Sends a command straight to the host helper.
    pub fn raw(&mut self, line: &str) -> Vec<String> {
        self.helper.call(line)
    }

    fn game(&mut self) -> Result<&mut Game, String> {
        self.game.as_mut().ok_or_else(|| tr!("attach to a game first"))
    }

    // --- Games

    pub fn games(&mut self) -> Vec<GameProcess> {
        self.helper
            .call("games")
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                let opt = |s: &str| (s != "-").then(|| s.to_owned());
                Some(GameProcess {
                    pid: f.first()?.parse().ok()?,
                    exe: f.get(1)?.to_string(),
                    app_id: opt(f.get(2)?),
                    refused: Refusal::parse(f.get(3)?),
                    name: f.get(4).and_then(|s| opt(s)),
                    multiplayer: matches!(f.get(5), Some(&"multiplayer" | &"online")),
                    ships: f.get(6).and_then(|s| opt(s)),
                })
            })
            .collect()
    }

    /// The attached game's program and the memory scrambler it uses.
    pub fn scrambler(&self) -> Option<(String, String)> {
        let g = self.game.as_ref()?;
        Some((g.exe.clone(), g.scrambler.clone()?))
    }

    /// Lets go of the attached game when it quit, or when anti-cheat started in it (the helper
    /// already let go then).
    pub fn check_game(&mut self) -> Option<Gone> {
        self.game.as_ref()?;
        let reply = self.helper.call("alive");
        let anti_cheat = reply.first().and_then(|l| l.strip_prefix("alive anti-cheat ")).map(str::to_owned);
        if anti_cheat.is_none() && reply.first().map(String::as_str) != Some("alive no") {
            return None;
        }
        let exe = self.game.take()?.exe;
        self.search = None;
        self.capture = None;
        Some(match anti_cheat {
            Some(ac) => {
                self.say(&format!("{ac} started in {exe}: Ferret let go of the game and changes nothing in it"));
                Gone::AntiCheat(exe, ac)
            }
            None => {
                self.say(&format!("{exe} quit"));
                Gone::Quit(exe)
            }
        })
    }

    /// Attaches and, when the game has a profile, finds its saved values again.
    pub fn attach(&mut self, pid: u32) -> Result<String, String> {
        let reply = self.helper.call(&format!("attach {pid}"));
        for l in &reply {
            self.say(l);
        }
        if let Some(e) = first_error(&reply) {
            return Err(e.strip_prefix("blocked by ptrace_scope ").map_or_else(|| e.clone(), ptrace_help));
        }
        let exe = reply.iter().find_map(|l| l.strip_prefix("exe: ")).ok_or("no program name")?.to_owned();
        // Another game (or the same one restarted) has another window: the old capture shows
        // nothing anymore.
        if self.game.as_ref().map(|g| g.pid) != Some(pid) {
            self.capture = None;
        }
        let build = self.helper.call("build").iter().find_map(|l| l.strip_prefix("build ")).map(str::to_owned);
        let steam = self
            .games()
            .into_iter()
            .find(|g| g.pid == pid)
            .and_then(|g| g.app_id)
            .filter(|id| id != "0" && id.bytes().all(|b| b.is_ascii_digit()));
        let scrambler = reply.iter().find_map(|l| l.strip_prefix("memory scrambler: ")).map(str::to_owned);
        self.game = Some(Game {
            pid,
            exe: exe.clone(),
            scrambler,
            entries: Vec::new(),
            paths: Vec::new(),
            votes: Vec::new(),
            steady: Vec::new(),
            via: Vec::new(),
            named: Vec::new(),
            named_doubt: Vec::new(),
            last_written: None,
            upgrade_asked: Vec::new(),
            build,
            steam,
            java: reply.iter().any(|l| l.starts_with("Java game: ")),
        });
        self.search = None;
        // A bar's copy of memory was the old process's.
        self.bar_last = None;
        self.font = Font::load(&digits_path(&exe));
        if !self.font.is_empty() {
            self.say(&format!("knows how {exe} draws the digits {}", self.font.known()));
        }
        self.shapes = shapes::load(&shapes_path(&exe));
        self.unshaped = false;
        self.send_shapes();
        if !self.shapes.is_empty() {
            let kinds = match self.shapes.len() {
                1 => "one kind of value".to_owned(),
                n => format!("{n} kinds of values"),
            };
            self.say(&format!("knows how {exe} keeps {kinds}: searches look there first"));
        }
        if !read_profile(&exe).is_empty() && self.game()?.java {
            // Listed, not looked for: nothing finds them again in Java yet.
            let names: Vec<(String, Loc)> = read_profile(&exe).into_iter().map(|e| (e.name, Loc { addr: 0, kind: e.kind })).collect();
            self.say(&format!("saved values for {exe} aren't looked for: Java games can't be found again yet (find them again and save them with the same name)"));
            self.game.as_mut().ok_or("no game")?.entries = names;
        } else if !read_profile(&exe).is_empty() {
            self.say(&format!("found saved values for {exe}, restoring:"));
            self.restore()?;
        }
        Ok(exe)
    }

    // --- Saved values

    fn peek(&mut self, addrs: &[Loc]) -> Vec<Option<i64>> {
        self.peek_exact(addrs).into_iter().map(|v| v.map(|v| (v + 1e-6).floor() as i64)).collect()
    }

    fn peek_exact(&mut self, addrs: &[Loc]) -> Vec<Option<f64>> {
        let arg: Vec<String> = addrs.iter().map(|a| a.to_string()).collect();
        let values = parse_exact_values(&self.helper.call(&format!("peek {}", arg.join(" "))));
        addrs
            .iter()
            .map(|a| values.iter().find(|(b, _)| b == a).and_then(|(_, v)| *v))
            .collect()
    }

    /// The "survive restarts" button: finds the code that accesses the current
    /// value and saves it under `name`, so the value can be found again next time.
    /// Returns false when that still needs confirming in a later run (unconfirmed pointer paths).
    pub fn save(&mut self, name: &str) -> Result<bool, String> {
        let name: String = one_word(name).chars().take(MAX_NAME).collect();
        let name = name.as_str();
        if name.is_empty() {
            return Err(tr!("the value needs a name"));
        }
        let listed = self.helper.call("list");
        let [(loc, _)] = parse_values(&listed)[..] else {
            return Err(tr!("narrow down to exactly one address first"));
        };
        if self.game()?.java {
            return self.save_java(name, loc);
        }
        // Unreal and Unity games name their objects' classes and fields: a name holds up
        // across restarts and updates (code patterns break when the game replaces objects), and
        // needs no tracing. Unity's are slow to search for: their code patterns are kept too.
        let mut named = self.named_path(loc, true);
        let reply = match &named {
            Some((text, _, _)) if !backed_by_code(text) => Vec::new(),
            _ => self.helper.call(&format!("sites {loc}")),
        };
        for l in reply.iter().filter(|l| !l.starts_with("site ")) {
            self.say(l);
        }
        let mut sites: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect();
        let (mut paths, mut candidates) = (Vec::new(), Vec::new());
        let pid = self.game()?.pid;
        if sites.is_empty() && named.is_none() {
            let accessed = reply.iter().find_map(|l| l.strip_suffix(" instructions accessed it")).and_then(|n| n.parse::<usize>().ok());
            let shared = reply.iter().any(|l| l.contains(": shared code,"));
            let why = match accessed {
                Some(0) => "nothing in the game touched the value while Ferret watched",
                _ if shared => "the game only reads this value through code it shares with other values (GameMaker games do this)",
                _ => "the game uses the value in a way Ferret can't save by code",
            };
            self.say(&format!("{why}; looking for objects the game names that lead to it"));
            named = self.named_path(loc, false);
            if named.is_none() && accessed == Some(0) {
                sites = self.sites_when_changed(loc);
            }
        }
        if sites.is_empty() && named.is_none() {
            self.say("no names lead to it; looking for pointers that lead to it instead");
            let exe = self.game()?.exe.clone();
            if let Some(saved) = read_profile(&exe).into_iter().find(|e| e.name == name) {
                // Candidates from this same run all still lead here: that proves nothing.
                let mut old = saved.paths;
                if saved.run != Some(pid) {
                    old.extend(saved.candidates);
                }
                paths = self.proven_paths(loc, &old);
            }
            if paths.is_empty() {
                candidates = self.pointer_paths(loc).map_err(|e| tr!("Ferret can't find it again by code or by name, and {why}", why = e))?;
            }
        }
        let how = if let (false, Some((_, places, about))) = (sites.is_empty(), &named) {
            format!("by name: {about}, {places} now, and {} code patterns, tried first", sites.len())
        } else if !sites.is_empty() {
            format!("{} code patterns", sites.len())
        } else if let Some((_, places, about)) = &named {
            format!("by name: {about}, {places} now")
        } else if !paths.is_empty() {
            format!("{} pointer paths that held up since the last run", paths.len())
        } else {
            format!(
                "{} pointer paths; after the next restart, if it's wrong, find it again and save it as {name}: that picks the right path",
                candidates.len()
            )
        };
        let decimals = if loc.kind.whole() { self.searched_decimals } else { 0 };
        let last = self.peek_exact(&[loc]).pop().flatten();
        let game = self.game()?;
        let build = game.build.clone();
        let mut entries = read_profile(&game.exe);
        // Saving a value again (to find it a better way) keeps its limit.
        let limit = entries.iter().find(|e| e.name == name).and_then(|e| e.limit.clone());
        entries.retain(|e| e.name != name);
        let named_text: Vec<String> = named.iter().map(|(t, _, _)| t.clone()).collect();
        let mut entry = Entry {
            name: name.to_owned(),
            kind: loc.kind,
            sites,
            paths,
            candidates,
            run: Some(pid),
            named: named_text.clone(),
            limit,
            decimals,
            last,
            build,
            untraced: None,
            imported: false,
            suggested: None,
            from: None,
            other: Vec::new(),
        };
        let known = entry.known(&entries);
        entry.candidates.sort_by_key(|p| std::cmp::Reverse(shared_start(p, &known)));
        let likely = entry.followed(&entries).len();
        let (followed, confirmed) = (entry.followed(&entries), !entry.unconfirmed());
        entries.push(entry);
        let path = write_profile(&game.exe, &entries)?;
        game.entries.retain(|(n, _)| n != name);
        game.entries.push((name.to_owned(), loc));
        game.paths.retain(|(n, _)| n != name);
        game.votes.retain(|(n, _)| n != name);
        game.via.retain(|(n, _)| n != name);
        game.named.retain(|(n, _, _)| n != name);
        if !followed.is_empty() {
            game.paths.push((name.to_owned(), followed));
        }
        if let Some(text) = named_text.first() {
            game.named.push((name.to_owned(), text.clone(), vec![loc]));
        }
        if let Some(entry) = entries.last().filter(|e| e.limit.is_some()) {
            let (min, max) = entry.shown_range();
            match self.apply_limit(entry) {
                Ok(()) => self.say(&format!("{name} is kept {}", limit_text(min, max))),
                Err(e) => self.say(&format!("{name}: limit not applied: {e}")),
            }
        } else if let Some(entry) = entries.last().filter(|e| !e.sites.is_empty()) {
            if let Err(e) = self.apply_limit(entry) {
                self.say(&format!("{name}: Ferret can't keep track of it if the game moves it: {e}"));
            }
        }
        if !confirmed && likely < entries.last().map_or(0, |e| e.candidates.len()) {
            self.say(&format!("{likely} of them start like other saved values' confirmed paths: Ferret follows those"));
        }
        self.say(&format!("saved {name} ({how}) to {}", path.display()));
        Ok(confirmed)
    }

    /// Whether the attached game runs on Java (saved values are only shown).
    pub fn java(&self) -> bool {
        self.game.as_ref().is_some_and(|g| g.java)
    }

    /// Java games: kept for this run only, and only shown. Nothing finds the value again (its
    /// code is shared by every object, references are compressed), and Java moves its objects:
    /// a write to the old address lands in another one (a limit's write crashed Shattered Pixel
    /// Dungeon).
    fn save_java(&mut self, name: &str, loc: Loc) -> Result<bool, String> {
        let decimals = if loc.kind.whole() { self.searched_decimals } else { 0 };
        let last = self.peek_exact(&[loc]).pop().flatten();
        let game = self.game()?;
        let (pid, build) = (game.pid, game.build.clone());
        let mut entries = read_profile(&game.exe);
        entries.retain(|e| e.name != name);
        entries.push(Entry {
            name: name.to_owned(),
            kind: loc.kind,
            sites: Vec::new(),
            paths: Vec::new(),
            candidates: Vec::new(),
            run: Some(pid),
            named: Vec::new(),
            limit: None,
            decimals,
            last,
            build,
            untraced: None,
            imported: false,
            suggested: None,
            from: None,
            other: Vec::new(),
        });
        let path = write_profile(&game.exe, &entries)?;
        self.helper.call(&format!("unlimit {name}"));
        let game = self.game.as_mut().ok_or("no game")?;
        game.entries.retain(|(n, _)| n != name);
        game.entries.push((name.to_owned(), loc));
        self.say(&format!("saved {name} to {} (Java game: shown only, for this run)", path.display()));
        Ok(true)
    }

    /// Pointer paths from the game's static memory to the value (see helper/src/pointers.rs),
    /// in profile form, best first. Paths through memory that changes within a few seconds
    /// would not survive a restart either, so they are dropped.
    fn pointer_paths(&mut self, loc: Loc) -> Result<Vec<String>, String> {
        let reply = self.helper.call(&format!("ptrscan {loc} 5 1000 {MAX_CANDIDATES}"));
        if let Some(e) = first_error(&reply) {
            return Err(tr!("the pointer scan failed: {e}", e));
        }
        for l in reply.iter().filter(|l| !l.starts_with("path ")) {
            self.say(l);
        }
        let mut paths: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("path ")).map(str::to_owned).collect();
        if paths.is_empty() {
            return Err(tr!("no pointer from the game's own memory leads to it"));
        }
        std::thread::sleep(Duration::from_secs(3));
        let (ends, _) = self.follow(loc.kind, &paths);
        let before = paths.len();
        paths = paths.into_iter().zip(ends).filter(|(_, e)| *e == Some(loc.addr)).map(|(p, _)| p).collect();
        if paths.is_empty() {
            return Err(tr!("every pointer path to it changed within seconds"));
        }
        self.say(&format!("{} of {before} pointer paths still lead to it after 3 s", paths.len()));
        Ok(paths.iter().map(|p| p.replace(',', " ")).collect())
    }

    /// The best named path to the value (see helper/src/names.rs): its text, how many places it
    /// leads to now and what it means. Checked the way a restart finds it: by name. `unreal`:
    /// only through an Unreal game's objects or a Mono class's live object (fast; nothing for other games).
    fn named_path(&mut self, loc: Loc, unreal: bool) -> Option<(String, usize, String)> {
        let reply = self.helper.call(&format!("names {loc}{}", if unreal { " unreal" } else { "" }));
        for l in reply.iter().filter(|l| !l.starts_with("named ") && !(unreal && l.starts_with("0 named paths"))) {
            self.say(l);
        }
        for line in reply.iter().filter_map(|l| l.strip_prefix("named ")) {
            let mut f = line.splitn(3, ' ');
            let (Some(text), Some(_)) = (f.next(), f.next()) else { continue };
            let (places, doubt) = self.follow_named(loc.kind, text);
            if let Some(why) = doubt {
                self.say(&format!("named path {text} left out: {why}"));
                continue;
            }
            if places.iter().any(|(p, _)| p.addr == loc.addr) {
                return Some((text.to_owned(), places.len(), f.next().unwrap_or_default().to_owned()));
            }
        }
        None
    }

    /// Every place a named path leads to now, with its value, and why they aren't one value
    /// if they aren't.
    fn follow_named(&mut self, kind: Kind, text: &str) -> (Vec<(Loc, Option<f64>)>, Option<String>) {
        let reply = self.helper.call(&format!("named {} {text}", kind.name()));
        let doubt = reply.iter().find_map(|l| l.strip_prefix("doubtful ")).map(str::to_owned);
        (parse_exact_values(&reply), doubt)
    }

    /// Remembers why a named value's places aren't one value now (None: they are).
    fn set_named_doubt(&mut self, name: &str, doubt: Option<String>) {
        if let Some(game) = self.game.as_mut() {
            game.named_doubt.retain(|(n, _)| n != name);
            game.named_doubt.extend(doubt.map(|d| (name.to_owned(), d)));
        }
    }

    /// Follows the named paths of values found through them again: the game adds and removes
    /// the things they lead to (stacks of an item).
    fn refresh_named(&mut self) {
        let Some(game) = self.game.as_ref() else { return };
        let followed: Vec<(String, String, Kind)> = game
            .named
            .iter()
            .filter_map(|(n, t, _)| Some((n.clone(), t.clone(), game.entries.iter().find(|(e, _)| e == n)?.1.kind)))
            .collect();
        for (name, text, kind) in followed {
            let (places, doubt) = self.follow_named(kind, &text);
            self.set_named_doubt(&name, doubt);
            let places: Vec<Loc> = places.into_iter().map(|(l, _)| l).collect();
            if let Some(game) = self.game.as_mut() {
                if let Some(e) = game.entries.iter_mut().find(|(n, _)| *n == name) {
                    e.1.addr = places.first().map_or(0, |l| l.addr);
                }
                if let Some(e) = game.named.iter_mut().find(|(n, _, _)| *n == name) {
                    e.2 = places;
                }
            }
        }
    }

    /// The player says the game shows the number a value saved through unconfirmed pointer paths
    /// has now, in a later run: the paths leading there are kept as confirmed, as saving it again
    /// would (finding it again only to save it wore the player out: Prey starts slowly), and its
    /// limit applies. Only when the paths clearly agree on one place: a few junk paths can agree
    /// on a place that happens to hold the same number (Valheim: 6 of 2998).
    pub fn confirm_paths(&mut self, name: &str) -> Result<String, String> {
        let name = one_word(name);
        let (exe, pid) = {
            let game = self.game()?;
            (game.exe.clone(), game.pid)
        };
        let mut entries = read_profile(&exe);
        let entry = entries.iter().find(|e| e.name == name).ok_or_else(|| tr!("{name} isn't saved", name))?;
        if entry.imported {
            return self.confirm_imported(&exe, entries, &name);
        }
        if !entry.unconfirmed() {
            return Err(tr!("{name} is confirmed already", name));
        }
        if entry.run == Some(pid) {
            return Err(tr!(
                "{name} was saved in this run of the game, where all its pointer paths lead to it: that proves nothing yet. Confirm it after restarting the game",
                name
            ));
        }
        let (kind, mut candidates) = (entry.kind, entry.candidates.clone());
        // Only the ones followed now: the number from last time may have picked them.
        if let Some((_, following)) = self.game()?.paths.iter().find(|(n, _)| *n == name) {
            let narrowed: Vec<String> = candidates.iter().filter(|c| following.contains(&path_arg(c))).cloned().collect();
            if !narrowed.is_empty() {
                candidates = narrowed;
            }
        }
        let args: Vec<String> = candidates.iter().map(|p| path_arg(p)).collect();
        let (ends, best, votes) = self.follow_votes(kind, &args);
        let Some((loc, _)) = best else {
            return Err(tr!("none of {name}'s pointer paths lead anywhere now", name));
        };
        if let Some(v) = votes.filter(|v| !v.clear) {
            return Err(tr!("{name} can't be confirmed: {why}", name, why = v.doubt()));
        }
        let kept: Vec<String> = candidates.iter().zip(&ends).filter(|(_, e)| **e == Some(loc.addr)).map(|(p, _)| p.clone()).collect();
        let entry = entries.iter_mut().find(|e| e.name == name).expect("found above");
        entry.paths = kept.clone();
        entry.candidates.clear();
        entry.run = Some(pid);
        let entry = entry.clone();
        write_profile(&exe, &entries)?;
        let followed = entry.followed(&entries);
        let game = self.game()?;
        game.entries.retain(|(n, _)| *n != name);
        game.entries.push((name.clone(), loc));
        game.paths.retain(|(n, _)| *n != name);
        game.paths.push((name.clone(), followed));
        self.set_votes(&name, votes);
        self.say(&format!("{name}: confirmed by the player; {} of {} pointer paths lead to it, kept as confirmed", kept.len(), candidates.len()));
        if let Err(e) = self.apply_limit(&entry) {
            self.say(&format!("{name}: limit not applied: {e}"));
        }
        Ok(tr!("{name} confirmed: Ferret finds it this way from now on", name))
    }

    /// The player says the game shows the number an imported value leads to: from now on it is
    /// theirs, written and kept in range like any other. Not while its ways of finding it
    /// disagree (another build of the game, a name leading to several values).
    fn confirm_imported(&mut self, exe: &str, mut entries: Vec<Entry>, name: &str) -> Result<String, String> {
        let game = self.game()?;
        if !game.entries.iter().any(|(n, l)| n == name && l.addr != 0) {
            return Err(tr!("{name} isn't in the game right now: load a save, then check it again", name));
        }
        if let Some((_, why)) = game.named_doubt.iter().find(|(n, _)| n == name) {
            return Err(tr!("{name} can't be confirmed: its name doesn't lead to one value ({why})", name, why));
        }
        if let Some((_, v)) = game.votes.iter().find(|(n, v)| n == name && !v.clear) {
            return Err(tr!("{name} can't be confirmed: {why}", name, why = v.doubt()));
        }
        let entry = entries.iter_mut().find(|e| e.name == name).ok_or_else(|| tr!("{name} isn't saved", name))?;
        entry.imported = false;
        let entry = entry.clone();
        write_profile(exe, &entries)?;
        self.say(&format!("{name}: confirmed by the player, imported value kept as their own"));
        if let Some(id) = entry.from.clone() {
            vote(id, true);
        }
        // Followed by the helper from now on (its code patterns, if the game moves it).
        if !entry.sites.is_empty() {
            if let Err(e) = self.apply_limit(&entry) {
                self.say(&format!("{name}: Ferret can't keep track of it if the game moves it: {e}"));
            }
        }
        Ok(format!("{name} confirmed: Ferret can change it now"))
    }

    /// Saving a value again after it moved (a restart, a new level) keeps the saved pointer
    /// paths that lead to where it is now: they held up through the move, which fresh ones
    /// haven't yet.
    fn proven_paths(&mut self, loc: Loc, saved: &[String]) -> Vec<String> {
        if saved.is_empty() {
            return Vec::new();
        }
        let args: Vec<String> = saved.iter().map(|p| path_arg(p)).collect();
        let (ends, _) = self.follow(loc.kind, &args);
        let kept: Vec<String> = saved.iter().zip(ends).filter(|(_, e)| *e == Some(loc.addr)).map(|(p, _)| p.clone()).collect();
        let then = if kept.is_empty() { ", scanning again" } else { ", keeping those" };
        self.say(&format!("{} of {} saved pointer paths lead to it here{then}", kept.len(), saved.len()));
        kept
    }

    /// Where each pointer path (helper form) leads now, and the address most of them agree on.
    fn follow(&mut self, kind: Kind, paths: &[String]) -> (Vec<Option<u64>>, Option<(Loc, Option<i64>)>) {
        let (ends, best, _) = self.follow_votes(kind, paths);
        (ends, best)
    }

    /// `follow`, plus how strongly the paths agree.
    fn follow_votes(&mut self, kind: Kind, paths: &[String]) -> (Vec<Option<u64>>, Option<(Loc, Option<i64>)>, Option<Votes>) {
        let reply = self.helper.call(&format!("follow {} {}", kind.name(), paths.join(" ")));
        let ends = (0..paths.len())
            .map(|i| {
                let line = reply.iter().find_map(|l| l.strip_prefix(&format!("{i}: ")))?;
                Some(parse_values(&[line.to_owned()]).first()?.0.addr)
            })
            .collect();
        let best = reply.iter().find_map(|l| l.strip_prefix("best ")).and_then(|l| parse_values(&[l.to_owned()]).first().copied());
        (ends, best, Votes::parse(&reply))
    }

    fn set_votes(&mut self, name: &str, votes: Option<Votes>) {
        if let Some(game) = self.game.as_mut() {
            game.votes.retain(|(n, _)| n != name);
            game.votes.extend(votes.map(|v| (name.to_owned(), v)));
        }
    }

    /// Why a value found through pointer paths isn't written now: its paths are unconfirmed
    /// guesses, or they didn't agree clearly when last followed.
    fn doubtful(&self, name: &str, saved: &[Entry]) -> Option<String> {
        let game = self.game.as_ref()?;
        if saved.iter().any(|e| e.name == name && e.imported) {
            return Some(tr!("it was imported and isn't checked yet. If the game shows the number Ferret reads, say so in the Values tab"));
        }
        if let Some((_, why)) = game.named_doubt.iter().find(|(n, _)| n == name) {
            return Some(tr!("its name doesn't lead to one value ({why})", why));
        }
        game.paths.iter().any(|(n, _)| n == name).then_some(())?;
        if saved.iter().any(|e| e.name == name && e.guessed(saved, game.pid)) {
            return Some(tr!("its pointer paths are guesses until a later run of the game confirms them"));
        }
        game.votes.iter().find(|(n, _)| n == name).map(|(_, v)| *v).filter(|v| !v.clear).map(|v| v.doubt())
    }

    /// Follows the pointer paths of values found through them again: the game may have moved them.
    fn refresh_paths(&mut self) {
        let Some(game) = self.game.as_ref() else { return };
        let followed = game.paths.clone();
        for (name, paths) in followed {
            let Some(kind) = self.game.as_ref().and_then(|g| g.entries.iter().find(|(n, _)| *n == name)).map(|(_, l)| l.kind) else {
                continue;
            };
            let (ends, best, votes) = self.follow_votes(kind, &paths);
            let clear = votes.as_ref().is_some_and(|v| v.clear);
            self.set_votes(&name, votes);
            if let Some(e) = self.game.as_mut().and_then(|g| g.entries.iter_mut().find(|(n, _)| *n == name)) {
                e.1.addr = best.map_or(0, |(loc, _)| loc.addr);
            }
            if let (true, Some((loc, _))) = (clear, best) {
                self.count_steady(&name, loc, &paths, &ends);
            }
        }
    }

    /// Counts which of a value's confirmed paths led to it (see `Steady`), and after
    /// `STEADY_LOOKS` drops the others from the profile and from this run's following.
    fn count_steady(&mut self, name: &str, loc: Loc, paths: &[String], ends: &[Option<u64>]) {
        let Some(game) = self.game.as_ref() else { return };
        let exe = game.exe.clone();
        if game.steady.iter().any(|(n, s)| n == name && s.done) {
            return;
        }
        let value = self.peek_exact(&[loc])[0];
        let Some(game) = self.game.as_mut() else { return };
        if !game.steady.iter().any(|(n, _)| n == name) {
            game.steady.push((name.to_owned(), Steady::default()));
        }
        let s = &mut game.steady.iter_mut().find(|(n, _)| n == name).unwrap().1;
        if s.place != loc.addr || s.led.len() != paths.len() {
            *s = Steady { place: loc.addr, led: vec![0; paths.len()], ..Steady::default() };
        }
        if !s.changed {
            match (s.first, value) {
                (Some(first), Some(v)) if first != v => s.changed = true,
                (_, v) => {
                    s.first = s.first.or(v);
                    return;
                }
            }
        }
        s.looks += 1;
        for (n, end) in s.led.iter_mut().zip(ends) {
            *n += (*end == Some(loc.addr)) as u32;
        }
        if s.looks < STEADY_LOOKS {
            return;
        }
        s.done = true;
        let need = (s.looks as f64 * STEADY_SHARE).ceil() as u32;
        let keep: Vec<String> = paths.iter().zip(&s.led).filter(|(_, n)| **n >= need).map(|(p, _)| p.clone()).collect();
        // Only confirmed paths, and only when enough are left to vote.
        let mut entries = read_profile(&exe);
        let Some(e) = entries.iter_mut().find(|e| e.name == name) else { return };
        let confirmed: Vec<String> = e.paths.iter().map(|p| path_arg(p)).collect();
        if confirmed != paths || keep.len() < 2 || keep.len() == paths.len() {
            return;
        }
        e.paths.retain(|p| keep.contains(&path_arg(p)));
        if write_profile(&exe, &entries).is_err() {
            return;
        }
        if let Some(f) = game.paths.iter_mut().find(|(n, _)| n == name) {
            f.1 = keep.clone();
        }
        self.say(&format!(
            "{name}: kept the {} of {} pointer paths that always led to it; the others lead elsewhere at times",
            keep.len(),
            paths.len()
        ));
    }

    /// Hands a saved value to the helper, which keeps it in its limit (if it has one) in the
    /// background and finds it again through its code patterns if the game moves it.
    fn apply_limit(&mut self, entry: &Entry) -> Result<(), String> {
        let limit = entry.limit.as_deref().unwrap_or("- -");
        let game = self.game()?;
        let (_, loc) = *game.entries.iter().find(|(n, _)| *n == entry.name).ok_or("value not found in this run")?;
        let saved = read_profile(&game.exe);
        let followed = entry.followed(&saved);
        if entry.sites.is_empty() && followed.is_empty() && entry.named.is_empty() {
            return Err("no saved code pattern, pointer path or name".into());
        }
        if entry.limit.is_some() && entry.imported {
            return Err(tr!(
                "{name}: it was imported and isn't checked yet. If the game shows the number Ferret reads, say so in the Values tab",
                name = entry.name
            ));
        }
        if entry.limit.is_some() && entry.sites.is_empty() && entry.guessed(&saved, game.pid) {
            return Err(tr!(
                "its pointer paths aren't confirmed yet: confirm it in the Values tab if the game shows its number, or find it again and save it as {name}",
                name = entry.name
            ));
        }
        let via = game.via.iter().filter(|(n, _)| *n == entry.name).map(|(_, p)| p.clone());
        let sites: Vec<String> = entry
            .sites
            .iter()
            .map(|s| s.split_whitespace().collect::<Vec<_>>().join(":"))
            .chain(via)
            .chain(followed)
            .chain(entry.named.iter().cloned())
            .collect();
        let reply = self.helper.call(&format!("limit {} {loc} {limit} {}", entry.name, sites.join(" ")));
        first_error(&reply).map_or(Ok(()), Err)
    }

    /// The saved values kept in range in the attached game (none when no game is attached).
    pub fn limited(&mut self) -> Vec<String> {
        if self.game().is_err() {
            return Vec::new();
        }
        // Not the values registered with no bounds ("- -") so their address stays current, nor
        // limits the player turned off.
        self.limits()
            .into_iter()
            .filter(|(_, _, rest)| !rest.starts_with("- - ") && !rest.ends_with("turned off"))
            .map(|(name, _, _)| name)
            .collect()
    }

    /// Turns all the game's limits off (followed, never written) when any is on, else on again
    /// (a hotkey: a cutscene or a fair fight). Until the next attach; not saved. Whether they're
    /// on now and their names.
    pub fn switch_limits(&mut self) -> Result<(bool, Vec<String>), String> {
        self.game()?;
        let reply = self.helper.call("switch * toggle");
        if reply.iter().any(|l| l.starts_with("error: no limit")) {
            return Err(tr!("Nothing is kept in range in this game"));
        }
        if let Some(e) = first_error(&reply) {
            return Err(e);
        }
        let on = reply.iter().any(|l| l.ends_with(" on"));
        let names: Vec<String> = reply.iter().filter_map(|l| l.rsplit_once(' ').map(|(n, _)| n.to_owned())).collect();
        self.say(&format!("Limits {}: {}", if on { "on" } else { "off" }, names.join(", ")));
        Ok((on, names))
    }

    /// Limits the helper enforces: name, current address, and the rest of its line.
    fn limits(&mut self) -> Vec<(String, u64, String)> {
        self.helper
            .call("limits")
            .iter()
            .filter_map(|l| {
                let mut f = l.splitn(3, ' ');
                let name = f.next()?.to_owned();
                let addr = u64::from_str_radix(f.next()?.trim_start_matches("0x"), 16).ok()?;
                Some((name, addr, f.next()?.to_owned()))
            })
            .collect()
    }

    /// The helper may have found a limited value at a new address; use that one.
    fn sync_addresses(&mut self) {
        let limits = self.limits();
        if let Some(game) = self.game.as_mut() {
            for (name, addr, _) in limits {
                if let Some(e) = game.entries.iter_mut().find(|(n, _)| *n == name) {
                    e.1.addr = addr;
                }
            }
        }
    }

    /// Forgets a saved value: out of the profile, no longer followed or kept in range. The
    /// game's number stays as it is.
    pub fn remove(&mut self, name: &str) -> Result<(), String> {
        let name = one_word(name);
        let name = name.as_str();
        let exe = self.game()?.exe.clone();
        let mut entries = read_profile(&exe);
        // An imported value removed before it was confirmed: it didn't work for this player.
        if let Some(id) = entries.iter().find(|e| e.name == name && e.imported).and_then(|e| e.from.clone()) {
            vote(id, false);
        }
        let before = entries.len();
        entries.retain(|e| e.name != name);
        if entries.len() == before {
            return Err(tr!("no saved value called {name}", name));
        }
        write_profile(&exe, &entries)?;
        self.helper.call(&format!("unlimit {name}"));
        if let Some(game) = self.game.as_mut() {
            game.entries.retain(|(n, _)| n != name);
            game.paths.retain(|(n, _)| n != name);
            game.votes.retain(|(n, _)| n != name);
            game.via.retain(|(n, _)| n != name);
            game.named.retain(|(n, _, _)| n != name);
        }
        Ok(())
    }

    /// The attached game's saved values as a `.ferret` file (share.rs): only the ways of finding
    /// them that work on other computers. Values with none are left out, with the reason.
    pub fn export(&mut self) -> Result<Export, String> {
        let game = self.game()?;
        let (exe, build, steam) = (game.exe.clone(), game.build.clone(), game.steam.clone());
        let (mut values, mut left_out) = (Vec::new(), Vec::new());
        for e in read_profile(&exe) {
            if e.imported {
                // Says what to do: the player re-imported their own export and couldn't tell
                // why it stayed out (Particle Fleet's omni).
                left_out.push(tr!(
                    "{name}: imported and not checked yet. On its card in the Values tab, click “Yes, It Shows…” if the game shows that number",
                    name = e.name
                ));
                continue;
            }
            // Names longer than the library takes (saved before the limit).
            if e.name.chars().count() > MAX_NAME {
                left_out.push(tr!("{name}: its name is longer than 32 letters (save it again with a shorter one)", name = e.name));
                continue;
            }
            let paths: Vec<String> = e.paths.iter().filter(|p| share::shareable_path(p)).cloned().collect();
            if e.sites.is_empty() && paths.is_empty() && e.named.is_empty() {
                left_out.push(match () {
                    _ if !e.paths.is_empty() => tr!(
                        "{name}: its pointer paths start in files that differ between computers (graphics driver, Wine, Steam)",
                        name = e.name
                    ),
                    _ if !e.candidates.is_empty() => {
                        tr!("{name}: its pointer paths aren't confirmed yet (restart the game and check it)", name = e.name)
                    }
                    _ => tr!("{name}: Ferret has no way to find it that works on other computers", name = e.name),
                });
                continue;
            }
            values.push(share::Shared {
                name: e.name,
                kind: e.kind,
                decimals: e.decimals,
                sites: e.sites,
                build: e.build,
                paths,
                named: e.named,
                limit: e.limit,
            });
        }
        if values.is_empty() {
            return Err(match left_out.is_empty() {
                true => tr!("Nothing is saved for this game yet"),
                false => format!("{} {}", tr!("Nothing to export."), left_out.join("; ")),
            });
        }
        let text = share::write(&exe, steam.as_deref(), build.as_deref(), &values);
        let names: Vec<&str> = values.iter().map(|v| v.name.as_str()).collect();
        self.say(&format!("exported {}", names.join(", ")));
        for l in &left_out {
            self.say(&format!("left out {l}"));
        }
        let stem = Path::new(&exe).file_stem().map_or(exe.clone(), |s| s.to_string_lossy().into_owned());
        Ok(Export { text, file_name: format!("{stem}.ferret"), count: values.len(), left_out })
    }

    /// What the shared library knows the attached game by: its program, Steam app id and build.
    pub fn library_key(&mut self) -> Result<(String, Option<String>, Option<String>), String> {
        let game = self.game()?;
        Ok((game.exe.clone(), game.steam.clone(), game.build.clone()))
    }

    /// Adds the values in a `.ferret` file's text to the attached game's, and finds them. They
    /// stay unconfirmed (never written, no limit) until the player says the game shows their
    /// number: a wrong shared value would write into random memory. A value with the name of
    /// one already saved is skipped (the player's own stays).
    /// `from`: the shared library's upload it came from.
    pub fn import(&mut self, text: &str, from: Option<&str>) -> Result<Imported, String> {
        let file = share::read(text)?;
        let game = self.game()?;
        let (exe, build) = (game.exe.clone(), game.build.clone());
        if !file.game.eq_ignore_ascii_case(&exe) {
            return Err(tr!("These values are for {theirs}, not {exe}", theirs = file.game, exe));
        }
        if let (Some(theirs), Some(ours)) = (&file.steam, &game.steam) {
            if theirs != ours {
                return Err(tr!(
                    "These values are for another game with a program called {exe} (Steam app {theirs}, this one is {ours})",
                    exe,
                    theirs,
                    ours
                ));
            }
        }
        let mut entries = read_profile(&exe);
        let (mut skipped, mut added) = (file.skipped, Vec::new());
        for v in file.values {
            if entries.iter().any(|e| e.name == v.name) {
                skipped.push(tr!("{name}: you have a value with this name already (remove yours to import it)", name = v.name));
                continue;
            }
            added.push(v.name.clone());
            entries.push(Entry {
                name: v.name,
                kind: v.kind,
                sites: v.sites,
                paths: v.paths,
                candidates: Vec::new(),
                run: None,
                named: v.named,
                limit: None,
                decimals: v.decimals,
                last: None,
                build: v.build,
                untraced: None,
                imported: true,
                suggested: v.limit,
                from: from.map(str::to_owned),
                other: Vec::new(),
            });
        }
        for s in &skipped {
            self.say(&format!("import skipped {s}"));
        }
        if added.is_empty() {
            return Err(format!("{} {}", tr!("Nothing imported."), skipped.join("; ")));
        }
        write_profile(&exe, &entries)?;
        self.say(&format!("imported {} (unconfirmed until the player checks them)", added.join(", ")));
        let other_build = file.build.is_some() && build.is_some() && file.build != build;
        if other_build {
            self.say("import: made with another build of the game");
        }
        self.restore_some(Some(&added))?;
        Ok(Imported { added, skipped, other_build })
    }

    /// Keeps a saved value within a range (as the game shows it); `None` on both sides turns
    /// the limit off.
    pub fn limit(&mut self, name: &str, min: Option<f64>, max: Option<f64>) -> Result<(), String> {
        let name = one_word(name);
        let name = name.as_str();
        if self.game()?.java && (min.is_some() || max.is_some()) {
            return Err(java_write());
        }
        if let (Some(lo), Some(hi)) = (min, max) {
            if hi < lo {
                return Err(tr!(
                    "At most ({max}) is lower than at least ({min}): nothing changed",
                    max = number_text(hi, None),
                    min = number_text(lo, None)
                ));
            }
        }
        let exe = self.game()?.exe.clone();
        let mut entries = read_profile(&exe);
        let i = entries.iter().position(|e| e.name == name).ok_or_else(|| tr!("no saved value called {name}", name))?;
        let (lo, hi) = (min.map(|v| entries[i].to_memory(v)).transpose()?, max.map(|v| entries[i].to_memory(v)).transpose()?);
        entries[i].limit = (lo.is_some() || hi.is_some()).then(|| range_text(lo, hi));
        if entries[i].limit.is_some() {
            self.apply_limit(&entries[i])?;
            self.say(&format!("{name} is kept {} (written only when the game goes past it)", limit_text(min, max)));
        } else if !entries[i].sites.is_empty() && self.apply_limit(&entries[i]).is_ok() {
            // Still found again when the game moves it.
            self.say(&format!("{name} is no longer limited"));
        } else {
            self.helper.call(&format!("unlimit {name}"));
            self.say(&format!("{name} is no longer limited"));
        }
        write_profile(&exe, &entries)?;
        Ok(())
    }

    /// Finds every saved value of this game again, and re-applies saved limits.
    pub fn restore(&mut self) -> Result<(), String> {
        self.restore_some(None)
    }

    /// `restore` for some of the values only (`None`: all of them).
    fn restore_some(&mut self, only: Option<&[String]>) -> Result<(), String> {
        let (exe, pid) = (self.game()?.exe.clone(), self.game()?.pid);
        let all = read_profile(&exe);
        let entries: Vec<Entry> = all.iter().filter(|e| only.is_none_or(|o| o.contains(&e.name))).cloned().collect();
        if entries.is_empty() {
            return Err(format!("nothing saved for {exe}"));
        }
        let t = Instant::now();
        let by_code: Vec<&Entry> = entries
            .iter()
            .filter(|e| e.foreign_type().is_none() && !e.sites.is_empty() && e.named.first().is_none_or(|t| backed_by_code(t)))
            .collect();
        if let Some(first) = by_code.first() {
            self.phase(Phase::Restoring(exe.clone(), first.name.clone(), 0, entries.len()));
        }
        let mut by_code = self.resolve_all(&by_code, RESTORE_WAIT)?;
        for (i, entry) in entries.iter().enumerate() {
            let name = &entry.name;
            self.phase(Phase::Restoring(exe.clone(), name.clone(), i, entries.len()));
            if let Some(kind) = entry.foreign_type() {
                self.say(&format!("{name}: saved by a newer Ferret (value type {kind}), which this one can't read; restart Ferret"));
                continue;
            }
            if let Some(text) = entry.named.first() {
                let by_site = by_code.iter().position(|(n, _)| n == name).map(|i| by_code.remove(i).1);
                let same_build = entry.build.is_some() && entry.build == self.game()?.build;
                // Its code patterns found it (fast), in the build they were checked in.
                if let (Some(Ok((loc, v))), true) = (&by_site, same_build) {
                    self.found_by_site(entry, *loc, *v)?;
                    continue;
                }
                let (places, doubt) = self.follow_named(entry.kind, text);
                if let Some(why) = &doubt {
                    self.say(&format!("{name}: its name doesn't lead to one value ({why}): never written; find it again and save it as {name}"));
                }
                let one_value = doubt.is_none();
                self.set_named_doubt(name, doubt);
                let locs: Vec<Loc> = places.iter().map(|(l, _)| *l).collect();
                let game = self.game()?;
                game.named.retain(|(n, _, _)| n != name);
                game.named.push((name.clone(), text.clone(), locs.clone()));
                game.entries.retain(|(n, _)| n != name);
                game.entries.push((name.clone(), locs.first().copied().unwrap_or(Loc { addr: 0, kind: entry.kind })));
                match places.first() {
                    None => self.say(&format!("{name}: not in the game right now (load a save?), Ferret keeps looking for it by name")),
                    Some((loc, v)) => {
                        let v = v.map_or("??".into(), |v| number_text(v / entry.scale(), entry.shown_decimals()));
                        let more = if places.len() > 1 { format!(", and {} more places it is kept in", places.len() - 1) } else { String::new() };
                        self.say(&format!("{name} = {v} (found by name at 0x{:x}{more})", loc.addr));
                    }
                }
                if let (true, Some((loc, _)), true) = (backed_by_code(text), places.first(), one_value) {
                    self.check_sites(entry, by_site, *loc, &places)?;
                }
                if entry.limit.is_some() {
                    let (min, max) = entry.shown_range();
                    match self.apply_limit(entry) {
                        Ok(()) => self.say(&format!("{name} is kept {}", limit_text(min, max))),
                        Err(e) => self.say(&format!("{name}: limit not applied: {e}")),
                    }
                }
                continue;
            }
            let mut why_not = None;
            // Set when the number from last time confirms its pointer paths here.
            let mut confirmed: Option<Entry> = None;
            let mut resolved = match by_code.iter().position(|(n, _)| n == name).map(|i| by_code.remove(i).1) {
                Some(Ok(found)) => Some(found),
                Some(Err(why)) => {
                    why_not = Some(why);
                    None
                }
                None => None,
            };
            let paths = entry.followed(&all);
            if resolved.is_none() && !paths.is_empty() {
                let (ends, mut best, mut votes) = self.follow_votes(entry.kind, &paths);
                let mut paths = paths;
                // Whether the number from last time picked the place (and said so).
                let mut by_last = false;
                if let Some((loc, v, kept)) = entry.run.filter(|&r| r != pid).and_then(|_| self.held_last(entry, &paths, &ends)) {
                    let shown = number_text(entry.last.unwrap_or_default() / entry.scale(), entry.shown_decimals());
                    if (entry.last.unwrap_or_default() / entry.scale()).abs() >= DISTINCTIVE {
                        let mut saved = read_profile(&exe);
                        if let Some(e) = saved.iter_mut().find(|e| e.name == *name) {
                            e.paths = e.candidates.iter().filter(|c| kept.contains(&path_arg(c))).cloned().collect();
                            e.candidates.clear();
                            e.run = Some(pid);
                            e.last = None;
                            confirmed = Some(e.clone());
                        }
                        write_profile(&exe, &saved)?;
                        self.say(&format!(
                            "{name}: {} of {} pointer paths lead to the one place holding {shown}, its number last time: kept as confirmed",
                            kept.len(),
                            paths.len()
                        ));
                    } else {
                        self.say(&format!(
                            "{name}: the one place holding {shown}, its number last time, is Ferret's best guess ({} of {} pointer paths lead there): if the game shows this number, confirm it in the Values tab",
                            kept.len(),
                            paths.len()
                        ));
                    }
                    // Followed only through those from now on.
                    let (_, b, v2) = self.follow_votes(entry.kind, &kept);
                    best = b.or(Some((loc, Some(v))));
                    votes = v2;
                    paths = kept;
                    by_last = true;
                }
                self.set_votes(name, votes);
                // Keep following them: before a save is loaded they may lead nowhere yet.
                let game = self.game()?;
                game.paths.retain(|(n, _)| n != name);
                game.paths.push((name.clone(), paths.clone()));
                match best {
                    Some((loc, Some(v))) => {
                        if !by_last {
                            let agree = ends.iter().filter(|e| **e == Some(loc.addr)).count();
                            self.say(&format!("{name}: {agree} of {} pointer paths lead to it", paths.len()));
                        }
                        if let Some(v) = votes.filter(|v| !v.clear) {
                            self.say(&format!("{name}: {}: Ferret won't write it until they agree", v.doubt()));
                        }
                        if confirmed.is_none() && entry.unconfirmed() {
                            if !by_last {
                                if paths.len() < entry.candidates.len() {
                                    self.say(&format!("{name}: following the {} unconfirmed paths that start like other values' confirmed ones", paths.len()));
                                }
                                self.say(&format!("{name}: its pointer paths aren't confirmed yet: if the game shows this number, confirm it in the Values tab; if not, find it again and save it as {name}"));
                            }
                            // Read again: the number from last time may have confirmed another
                            // value's paths, which this one's may start like.
                            if entry.guessed(&read_profile(&exe), pid) {
                                self.say(&format!("{name}: Ferret won't write it until a path is confirmed"));
                            }
                        }
                        resolved = Some((loc, v));
                    }
                    _ => {
                        self.say(&format!("{name}: its pointer paths lead nowhere yet (load a save?), Ferret keeps following them"));
                        let game = self.game()?;
                        game.entries.retain(|(n, _)| n != name);
                        game.entries.push((name.clone(), Loc { addr: 0, kind: entry.kind }));
                        if entry.limit.is_some() {
                            if let Err(e) = self.apply_limit(entry) {
                                self.say(&format!("{name}: limit not applied: {e}"));
                            }
                        }
                        continue;
                    }
                }
            }
            let entry = confirmed.as_ref().unwrap_or(entry);
            match resolved {
                Some((loc, v)) => self.found_by_site(entry, loc, v)?,
                None if why_not.as_ref().is_some_and(|w| w.starts_with(IMPLAUSIBLE)) => {
                    // Looking again would find the same wrong place (and a limit write there).
                    let why = why_not.unwrap_or_default();
                    self.say(&format!("{name}: its code patterns lead to something else ({why}; the game was updated?): find it again and save it as {name}"));
                    let game = self.game()?;
                    game.entries.retain(|(n, _)| n != name);
                    game.entries.push((name.clone(), Loc { addr: 0, kind: entry.kind }));
                }
                None => {
                    let why = why_not.unwrap_or_else(|| "the game hasn't run the code that uses it".into());
                    self.say(&format!("{name}: not found yet ({why}), Ferret keeps looking for it in the background"));
                    let game = self.game()?;
                    game.entries.retain(|(n, _)| n != name);
                    game.entries.push((name.clone(), Loc { addr: 0, kind: entry.kind }));
                    // The helper finds it when the code runs (and applies its limit then).
                    if let Err(e) = self.apply_limit(entry) {
                        self.say(&format!("{name}: can't look for it in the background: {e}"));
                    }
                }
            }
        }
        self.say(&format!("restored in {} ms", t.elapsed().as_millis()));
        Ok(())
    }

    /// Finds values by their code patterns, all at once: per value where it is and its value,
    /// or why not.
    fn resolve_all(&mut self, entries: &[&Entry], wait: Duration) -> Result<Vec<(String, Result<(Loc, i64), String>)>, String> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }
        let mut cmd = format!("resolve-all {}", wait.as_secs_f64());
        for e in entries {
            cmd += &format!(" | {} {} {}", e.name, e.kind.name(), e.sites.join(" "));
        }
        let reply = self.helper.call(&cmd);
        if let Some(e) = reply.first().filter(|l| l.starts_with("error:")) {
            return Err(e.clone());
        }
        let mut found = Vec::new();
        for e in entries {
            let name = &e.name;
            self.game()?.via.retain(|(n, _)| n != name);
            let mine: Vec<&str> = reply.iter().filter_map(|l| l.strip_prefix(name.as_str())?.strip_prefix(' ')).collect();
            let result = match mine.iter().find_map(|l| l.strip_prefix("error: ")) {
                Some(why) => Err(why.to_owned()),
                None => match parse_exact_values(&mine.iter().map(|l| l.to_string()).collect::<Vec<_>>()).first() {
                    Some((loc, Some(v))) if !plausible(loc.kind, *v) => Err(format!("{IMPLAUSIBLE}: {v:e}")),
                    Some((loc, Some(v))) => {
                        let v = (v + 1e-6).floor() as i64;
                        if let Some(via) = mine.iter().find_map(|l| l.strip_prefix("via ")) {
                            self.game()?.via.push((name.clone(), via.to_owned()));
                        }
                        Ok((*loc, v))
                    }
                    _ => Err(format!("no answer from the helper ({})", mine.join(" "))),
                },
            };
            found.push((name.clone(), result));
        }
        Ok(found)
    }

    /// A value found by name whose code patterns were tried first: when they found it too, they
    /// hold up in this build; when they found something else (or nothing) in another build,
    /// they are traced again where the name leads, so the next restart is fast again.
    /// `by_site`: None when it has none yet (saved by name before they were kept too).
    fn check_sites(&mut self, entry: &Entry, by_site: Option<Result<(Loc, i64), String>>, loc: Loc, places: &[(Loc, Option<f64>)]) -> Result<(), String> {
        let name = &entry.name;
        let build = self.game()?.build.clone();
        let tried = build.is_some() && entry.untraced == build;
        let Some(by_site) = by_site else {
            if tried {
                return Ok(());
            }
            self.say(&format!("{name}: tracing the code that uses it, so the next restart finds it faster"));
            return self.retrace(name, loc, false);
        };
        let why = match by_site {
            Ok((l, _)) if places.iter().any(|(p, _)| p.addr == l.addr) => {
                if entry.build != build {
                    self.say(&format!("{name}: its code patterns still find it in this version of the game"));
                    self.update_entry(name, |e| e.build = build.clone())?;
                }
                return Ok(());
            }
            Ok((l, _)) => format!("they led to 0x{:x}, not where its name leads", l.addr),
            // Its code didn't run meanwhile: nothing tells whether they still work.
            Err(why) if why.starts_with(NOT_RUN) || why.starts_with("cannot trace") => return Ok(()),
            Err(why) => why,
        };
        let wrong = why.starts_with("they led") || why.starts_with(IMPLAUSIBLE);
        if tried && !wrong {
            return Ok(());
        }
        self.say(&format!("{name}: its code patterns no longer find it ({why}): tracing it again"));
        self.retrace(name, loc, wrong)
    }

    /// Traces the code using a value found by name and saves its patterns; `wrong`: the old
    /// ones found something else, dropped when no new ones come.
    fn retrace(&mut self, name: &str, loc: Loc, wrong: bool) -> Result<(), String> {
        let build = self.game()?.build.clone();
        let reply = self.helper.call(&format!("sites {loc}"));
        let sites: Vec<String> = reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect();
        if !sites.is_empty() {
            self.say(&format!("{name}: {} new code patterns saved: the next restart finds it fast again", sites.len()));
            return self.update_entry(name, |e| {
                e.sites = sites.clone();
                e.build = build.clone();
                e.untraced = None;
            });
        }
        let dropped = if wrong { "; its old code patterns are dropped" } else { "" };
        self.say(&format!(
            "{name}: the game didn't touch it meanwhile{dropped}; it's found by name (tried again after a game update, or when saved again)"
        ));
        self.update_entry(name, |e| {
            if wrong {
                e.sites.clear();
            }
            e.untraced = build.clone();
        })
    }

    /// Changes a saved value in the profile.
    fn update_entry(&mut self, name: &str, change: impl Fn(&mut Entry)) -> Result<(), String> {
        let exe = self.game()?.exe.clone();
        let mut saved = read_profile(&exe);
        saved.iter_mut().filter(|e| e.name == name).for_each(change);
        write_profile(&exe, &saved).map(|_| ())
    }

    /// A value found by its code patterns (or its pointer paths): shown, and its limit applied.
    fn found_by_site(&mut self, entry: &Entry, loc: Loc, v: i64) -> Result<(), String> {
        let name = &entry.name;
        self.say(&format!("{name} = {v} (at 0x{:x}, {})", loc.addr, loc.kind.describe()));
        let game = self.game()?;
        game.entries.retain(|(n, _)| n != name);
        game.entries.push((name.clone(), loc));
        if entry.limit.is_some() {
            let (min, max) = entry.shown_range();
            match self.apply_limit(entry) {
                Ok(()) => self.say(&format!("{name} is kept {}", limit_text(min, max))),
                Err(e) => self.say(&format!("{name}: limit not applied: {e}")),
            }
        } else if !entry.sites.is_empty() {
            // The game may replace the object holding it (Particle Fleet does on a new map): the
            // helper finds it again.
            if let Err(e) = self.apply_limit(entry) {
                self.say(&format!("{name}: Ferret can't keep track of it if the game moves it: {e}"));
            }
        }
        Ok(())
    }

    /// Hands values found this run some weaker way to the upgrader (once a run each), and saves
    /// the engine paths it found for them.
    fn upgrades(&mut self) {
        let done: Vec<Upgrade> = self.upgrader.as_ref().map(|u| u.done.try_iter().collect()).unwrap_or_default();
        for u in done {
            if let Err(e) = self.upgrade(&u) {
                self.say(&format!("{}: could not save it by name: {e}", u.name));
            }
        }
        let Some(game) = self.game.as_ref() else { return };
        let fresh: Vec<(String, Loc)> = game.entries.iter().filter(|(n, l)| l.addr != 0 && !game.upgrade_asked.contains(n)).cloned().collect();
        if fresh.is_empty() {
            return;
        }
        let saved = read_profile(&game.exe);
        let mut jobs = Vec::new();
        for (name, loc) in &fresh {
            let Some(e) = saved.iter().find(|e| e.name == *name) else { continue };
            let engine = e.named.first().is_some_and(|t| engine_path(t));
            // A name leading to every stack of an item: an engine path leads to fewer places.
            let stacks = game.named.iter().any(|(n, _, l)| n == name && l.len() > 1);
            // Pointer paths no run confirmed may lead to the wrong place: never written, and
            // never made into a name that would be.
            let unsure = e.named.is_empty() && e.sites.is_empty() && e.unconfirmed();
            // An imported value may lead somewhere else in this player's game: not made into a
            // name before the player confirms it.
            if !engine && !stacks && !unsure && !e.imported && e.foreign_type().is_none() {
                jobs.push(UpgradeJob { pid: game.pid, name: name.clone(), loc: *loc });
            }
        }
        if let Some(game) = self.game.as_mut() {
            game.upgrade_asked.extend(fresh.into_iter().map(|(n, _)| n));
        }
        if jobs.is_empty() {
            return;
        }
        let upgrader = self.upgrader.get_or_insert_with(Upgrader::start);
        for job in jobs {
            upgrader.jobs.send(job).ok();
        }
    }

    /// Saves a value by the engine path found for it, in place of the weaker way it was saved
    /// (its limit stays). Nothing when it moved or was saved again meanwhile.
    fn upgrade(&mut self, u: &Upgrade) -> Result<(), String> {
        let Some(game) = self.game.as_mut().filter(|g| g.pid == u.pid) else { return Ok(()) };
        if !game.entries.iter().any(|(n, l)| *n == u.name && l.addr == u.loc.addr) {
            return Ok(());
        }
        let mut entries = read_profile(&game.exe);
        let Some(e) = entries.iter_mut().find(|e| e.name == u.name && e.kind == u.loc.kind) else { return Ok(()) };
        if e.named.first().is_some_and(|t| engine_path(t)) {
            return Ok(());
        }
        let was = match (e.sites.is_empty(), e.named.is_empty()) {
            (false, _) => "code patterns",
            (true, false) => "a name the game merely holds",
            (true, true) => "pointer paths",
        };
        // Code patterns that found it this run stay, tried first (a Unity class search is slow);
        // this run goes on following it by them.
        if backed_by_code(&u.text) && !e.sites.is_empty() {
            e.named = vec![u.text.clone()];
            e.build = game.build.clone();
            write_profile(&game.exe, &entries)?;
            self.say(&format!(
                "{}: also saved by name ({}, {} now): Ferret finds it that way when its code patterns stop working (after a game update)",
                u.name, u.about, u.places
            ));
            return Ok(());
        }
        e.sites.clear();
        e.paths.clear();
        e.candidates.clear();
        e.named = vec![u.text.clone()];
        e.run = Some(u.pid);
        let limited = e.limit.is_some();
        write_profile(&game.exe, &entries)?;
        game.paths.retain(|(n, _)| *n != u.name);
        game.votes.retain(|(n, _)| *n != u.name);
        game.via.retain(|(n, _)| *n != u.name);
        game.named.retain(|(n, _, _)| *n != u.name);
        game.named.push((u.name.clone(), u.text.clone(), vec![u.loc]));
        self.set_named_doubt(&u.name, None);
        // The helper followed it the old way (code patterns, paths): by name from now on.
        self.helper.call(&format!("unlimit {}", u.name));
        if limited {
            if let Some(entry) = entries.iter().find(|e| e.name == u.name) {
                self.apply_limit(entry)?;
            }
        }
        self.say(&format!(
            "{}: now saved by name ({}, {} now) instead of by {was}: Ferret finds it that way after restarts and game updates",
            u.name, u.about, u.places
        ));
        Ok(())
    }

    /// Keeps the number of each value with unconfirmed pointer paths while it is somewhere
    /// Ferret trusts (the run it was saved in, agreeing paths), for `held_last` after a restart.
    fn remember_last(&mut self, shown: &[(String, Loc)], values: &[Option<f64>], saved: &[Entry]) {
        let Some(game) = self.game.as_ref() else { return };
        if game.last_written.is_some_and(|t| t.elapsed() < LAST_EVERY) {
            return;
        }
        let seen: Vec<(String, f64)> = shown
            .iter()
            .zip(values)
            .filter(|((name, loc), _)| loc.addr != 0 && self.doubtful(name, saved).is_none())
            .filter_map(|((name, _), v)| Some((name.clone(), (*v)?)))
            .collect();
        let mut entries = saved.to_vec();
        let mut changed = false;
        for (name, v) in seen {
            if let Some(e) = entries.iter_mut().find(|e| e.name == name && e.unconfirmed() && !e.last.is_some_and(|l| same_value(e.kind, l, v))) {
                e.last = Some(v);
                changed = true;
            }
        }
        let exe = game.exe.clone();
        if changed && write_profile(&exe, &entries).is_err() {
            return;
        }
        if let Some(game) = self.game.as_mut() {
            game.last_written = Some(Instant::now());
        }
    }

    /// The one place `ends` lead to that holds the value's number from last time (`last`), its
    /// value, and the paths (helper form) that lead there. `None` when no place or several do.
    fn held_last(&mut self, entry: &Entry, paths: &[String], ends: &[Option<u64>]) -> Option<(Loc, i64, Vec<String>)> {
        let last = entry.last?;
        let mut places: Vec<Loc> = ends.iter().flatten().map(|&addr| Loc { addr, kind: entry.kind }).collect();
        places.sort_by_key(|l| l.addr);
        places.dedup();
        let values = self.peek_exact(&places);
        let holding: Vec<(Loc, f64)> =
            places.into_iter().zip(values).filter_map(|(l, v)| Some((l, v?))).filter(|(_, v)| same_value(entry.kind, *v, last)).collect();
        let [(loc, v)] = holding.as_slice() else { return None };
        let kept = paths.iter().zip(ends).filter(|(_, e)| **e == Some(loc.addr)).map(|(p, _)| p.clone()).collect();
        Some((*loc, (v + 1e-6).floor() as i64, kept))
    }

    pub fn values(&mut self) -> Result<Vec<ValueRow>, String> {
        let exe = self.game()?.exe.clone();
        self.upgrades();
        self.refresh_paths();
        self.refresh_named();
        self.sync_addresses();
        let entries: Vec<(String, Loc)> = self.game()?.entries.clone();
        let named = self.game()?.named.clone();
        let addrs: Vec<Loc> = entries.iter().map(|(_, a)| *a).collect();
        let values = self.peek_exact(&addrs);
        let limits = self.limits();
        let saved = read_profile(&exe);
        let (pid, java) = (self.game()?.pid, self.game()?.java);
        self.remember_last(&entries, &values, &saved);
        Ok(entries
            .into_iter()
            .zip(values)
            .map(|((name, loc), value)| {
                let entry = saved.iter().find(|e| e.name == name);
                let (min, max) = entry.map_or((None, None), Entry::shown_range);
                let value = value.map(|v| v / entry.map_or(1.0, Entry::scale));
                let decimals = entry.map_or(loc.kind.whole().then_some(0), Entry::shown_decimals);
                let limit_state = limits
                    .iter()
                    .filter(|_| min.is_some() || max.is_some())
                    .find(|(n, _, _)| *n == name)
                    .and_then(|(_, _, rest)| rest.split_once(" fixed ").map(|(_, s)| format!("fixed {s}")));
                let unconfirmed = saved.iter().any(|e| e.name == name && e.unconfirmed());
                let doubtful = self.doubtful(&name, &saved);
                let places = match named.iter().find(|(n, _, _)| *n == name) {
                    Some((_, _, p)) => p.len(),
                    // A Java value saved in an earlier run: not looked for.
                    None if loc.addr == 0 => 0,
                    None => 1,
                };
                // A named value that leads nowhere now (no stack of the item): the last address
                // holds something else by now (iron showed 1118760170, a float of another object).
                let value = value.filter(|_| places > 0);
                let imported = entry.is_some_and(|e| e.imported);
                let confirmable = value.is_some() && (imported || saved.iter().any(|e| e.name == name && e.unconfirmed() && e.run != Some(pid)));
                let suggested = entry.and_then(|e| Some((e, e.suggested.as_deref()?))).map_or((None, None), |(e, l)| {
                    let (lo, hi) = parse_range(l);
                    (lo.map(|v| v / e.scale()), hi.map(|v| v / e.scale()))
                });
                ValueRow {
                    name,
                    addr: loc.addr,
                    kind: loc.kind,
                    value,
                    min,
                    max,
                    decimals,
                    limit_state,
                    unconfirmed,
                    doubtful,
                    places,
                    confirmable,
                    read_only: java,
                    imported,
                    suggested,
                }
            })
            .collect())
    }

    /// Writes a saved value, as the game shows it ("1.5").
    pub fn set(&mut self, name: &str, value: f64) -> Result<(), String> {
        let name = one_word(name);
        let name = name.as_str();
        if self.game()?.java {
            return Err(java_write());
        }
        let exe = self.game()?.exe.clone();
        let saved = read_profile(&exe);
        if saved.iter().any(|e| e.name == name && e.imported) {
            return Err(tr!(
                "{name} not written: it was imported and isn't checked yet. If the game shows the number Ferret reads, say so in the Values tab",
                name
            ));
        }
        // Kept at least some number and nothing more (Keep It): the minimum moves to the new
        // number, first, so the limit doesn't put the old one back (the player set Age of War's
        // base health to 1000 and 1100 while it was kept at least 992.45).
        if let Some((min, None)) = saved.iter().find(|e| e.name == name).map(|e| e.shown_range()).filter(|r| r.0.is_some()) {
            if min != Some(value) {
                self.limit(name, Some(value), None)?;
            }
        }
        let saved = read_profile(&exe);
        let value = match saved.iter().find(|e| e.name == name) {
            Some(entry) => entry.to_memory(value)?,
            None => value,
        };
        self.refresh_paths();
        self.refresh_named();
        self.sync_addresses();
        if let Some((_, _, places)) = self.game()?.named.iter().find(|(n, _, _)| n == name).cloned() {
            if places.is_empty() {
                return Err(tr!("{name} isn't in the game right now (is a save loaded?)", name));
            }
            if let Some((_, why)) = self.game()?.named_doubt.iter().find(|(n, _)| n == name) {
                return Err(tr!("{name} not written: its name doesn't lead to one value ({why}). Find it again and save it as {name}.", name, why));
            }
            for p in &places {
                let reply = self.helper.call(&format!("write {p} {value}"));
                if let Some(e) = first_error(&reply) {
                    return Err(e);
                }
            }
            self.say(&format!("wrote {value} to {name} in {} places", places.len()));
            return Ok(());
        }
        let (_, loc) = *self
            .game()?
            .entries
            .iter()
            .find(|(n, _)| n == name)
            .ok_or_else(|| tr!("no saved value called {name}", name))?;
        if loc.addr == 0 {
            return Err(tr!("{name} can't be found right now (is a save loaded?)", name));
        }
        if let Some(why) = self.doubtful(name, &saved) {
            return Err(tr!(
                "not written: {why}, so it could land in the wrong place. Find it again and save it as {name}: that confirms the right path",
                why,
                name
            ));
        }
        let reply = self.helper.call(&format!("write {loc} {value}"));
        for l in &reply {
            self.say(l);
        }
        first_error(&reply).map_or(Ok(()), Err)
    }

    // --- Reading numbers off the game window

    /// Starts window capture; the first time, the desktop asks which window.
    pub fn start_capture(&mut self) -> Result<(), String> {
        if self.capture.is_none() {
            self.capture = Some(WindowCapture::start()?);
        }
        Ok(())
    }

    /// The player picked the wrong window: forget it (and the box picked in it) and ask the
    /// desktop again, then find the numbers in the new one. Closing Ferret didn't help: the
    /// choice is kept with a restore token so the picker only shows up once.
    pub fn change_window(&mut self) -> Result<(PathBuf, Vec<Word>), String> {
        self.capture = None;
        WindowCapture::forget_window();
        self.clear_area();
        self.say("forgot the window picked before: the desktop asks which window to watch");
        self.numbers()
    }

    pub fn frame(&mut self) -> Result<PathBuf, String> {
        self.start_capture()?;
        let out = cache_dir().join("frame.png");
        // The window may be gone (the game closed): the next try asks the desktop again.
        if let Err(e) = self.capture.as_ref().unwrap().grab(&out) {
            self.capture = None;
            return Err(e);
        }
        Ok(out)
    }

    /// Captures a frame and finds every number in it.
    pub fn numbers(&mut self) -> Result<(PathBuf, Vec<Word>), String> {
        let frame = self.frame()?;
        self.words = ocr::numbers(&frame, Some(&self.font))?;
        Ok((frame, self.words.clone()))
    }

    pub fn word_area(&self, i: usize) -> Option<Rect> {
        self.words.get(i).map(|w| ocr::watch_area(w.rect))
    }

    /// Picks the number to watch. A different number (not just a tighter box around the same
    /// one) while a search runs keeps the search: it may show the same value elsewhere (a
    /// total and a stack). Returns the matches kept then, for the player to decide.
    pub fn set_area(&mut self, area: Rect) -> Option<usize> {
        let kept = self.search.filter(|_| self.area.is_some_and(|a| !ocr::overlaps(a, area))).map(|(n, _)| n);
        if let Some(n) = kept {
            self.say(&format!("another number picked: keeping the {n} matches (Start Over clears them)"));
        }
        self.area = Some(area);
        self.bar = None;
        self.picked_look = None;
        self.hidden = false;
        self.hides = 0;
        kept
    }

    /// Takes the box just picked (`read_picked`, which read no number in it) as a bar or a row
    /// of icons: how full it is (how many are full) from now on is what the search goes by.
    pub fn pick_bar(&mut self) -> Result<GaugeKind, String> {
        let area = self.area.ok_or("no area picked yet")?;
        let img = image::open(cache_dir().join("picked.png")).map_err(|e| e.to_string())?.to_rgb8();
        let gauge = Gauge::pick(&img, area)?;
        let (kind, shown) = (gauge.kind(), gauge.shown(gauge.fill(&img).0));
        self.say(&format!("{}; {shown} now", gauge.describe()));
        self.bar = Some(gauge);
        // Matches go on from the new box's reading (`bar_step` marks them); a copy of memory
        // was taken against the old box.
        self.bar_last = None;
        Ok(kind)
    }

    /// The picked bar's or icons' share of the whole as the player sees it, for the log.
    fn shown(&self, share: f64) -> String {
        self.bar.as_ref().map_or(String::new(), |g| g.shown(share))
    }

    fn icons(&self) -> bool {
        self.bar.as_ref().is_some_and(|g| g.kind() == GaugeKind::Icons)
    }


    /// Forgets the picked box: searches go on with typed numbers only (a total the game never
    /// shows, such as loaded + reserve ammo), and checks don't read the screen.
    pub fn clear_area(&mut self) {
        self.area = None;
        self.bar = None;
        self.picked_look = None;
        self.hidden = false;
        self.hides = 0;
        self.say("no box picked now: type the numbers the game holds");
    }

    /// Forgets the search in progress, so the next number starts from scratch.
    pub fn reset(&mut self) {
        // Clearing a search that kept only places shaped like earlier finds: they weren't it, and
        // the next scan would land on the same kind of place again (Prey: the shotgun's shells
        // gave one place shaped like the pistol's ammo, three Start Overs in a row). Scans look
        // everywhere until something is found.
        if self.search.is_some() && self.shaped {
            self.unshaped = true;
            self.say("searches look everywhere now, not first where earlier finds were");
        }
        self.shaped = false;
        self.search = None;
        self.bar_last = None;
        self.helper.call("track");
        self.say("search cleared");
    }

    /// Takes back the last step of the search (a number, ruling a place out, Start Over, a
    /// pick): how many places match again, and what was undone.
    pub fn undo(&mut self) -> Result<(usize, String), String> {
        self.history_step("undo", "undid")
    }

    /// Takes the last step Undo took back again: the matches count and the step in words.
    pub fn redo(&mut self) -> Result<(usize, String), String> {
        self.history_step("redo", "redid")
    }

    fn history_step(&mut self, command: &str, done: &str) -> Result<(usize, String), String> {
        let reply = self.helper.call(command);
        let count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or(format!("{command} failed")))?;
        let step = reply.get(1).and_then(|l| l.strip_prefix(done)).unwrap_or_default();
        let place = |a: &str| parse_loc(a).map_or(a.to_owned(), |l| format!("0x{:x}", l.addr));
        let mut words = step.split_whitespace();
        // In English for the log, and in the player's language for the window.
        let (what, shown) = match (words.next(), words.next()) {
            (Some("next"), Some("bar")) => ("a move of the bar".to_owned(), tr!("a move of the bar")),
            (Some("snap"), _) => ("copying the game's memory".to_owned(), tr!("copying the game's memory")),
            (Some("scan" | "next"), Some(n)) => (format!("the search for {n}"), tr!("the search for {n}", n)),
            (Some("drop"), Some(at)) => (format!("ruling out {}", place(at)), tr!("ruling out {place}", place = place(at))),
            (Some("keep"), Some(at)) => (format!("picking {}", place(at)), tr!("picking {place}", place = place(at))),
            _ => ("starting over".to_owned(), tr!("starting over")),
        };
        self.search = (count > 0).then_some((count, 0));
        self.say(&format!("{done} {what}: {count} matches"));
        Ok((count, shown))
    }

    /// Reads the number inside the watched area of a fresh frame.
    pub fn read(&mut self) -> Result<Option<Shown>, String> {
        Ok(self.read_learned()?.map(|(n, _)| n))
    }

    /// Reads the number inside the watched area, and whether the learned digits read it.
    fn read_learned(&mut self) -> Result<Option<(Shown, bool)>, String> {
        let frame = self.frame()?;
        self.read_frame(&frame)
    }

    /// Reads the watched number in `frame`, strictly inside the watched area. Nothing while the
    /// area doesn't look like it did when picked (the inventory closed): what's there instead
    /// isn't the number, and the readers would make one up.
    fn read_frame(&mut self, frame: &Path) -> Result<Option<(Shown, bool)>, String> {
        let area = self.area.ok_or("no area picked yet")?;
        if let Some(f) = self.on_frame.as_mut() {
            f(frame, self.area);
        }
        let shows = self.shows_picked(frame);
        let read = if shows {
            ocr::read_number_at(frame, area, &cache_dir().join("area.png"), Some(&self.font))?
        } else {
            None
        };
        if shows && read.is_none() && self.was_read {
            fs::copy(frame, cache_dir().join("unread.png")).ok();
            self.say(&format!("no number read in the box now ({},{} {}x{}): frame kept as unread.png", area.x, area.y, area.w, area.h));
        }
        self.was_read = read.is_some();
        if let Some(f) = self.on_read.as_mut() {
            f(read.as_ref().map(|(n, _)| n));
        }
        Ok(read)
    }

    /// Whether the watched area of `frame` still looks like when it was picked. Tells the
    /// player when that changes.
    fn shows_picked(&mut self, frame: &Path) -> bool {
        let (Some(area), Some(picked)) = (self.area, self.picked_look.as_ref()) else { return true };
        let shows = ocr::Look::of(frame, area).is_ok_and(|now| now.like(picked));
        if shows == self.hidden {
            self.hidden = !shows;
            if !shows {
                self.hides += 1;
            }
            let msg = if shows {
                n_!("The number is back in the box.")
            } else if self.hides >= 3 {
                n_!(
                    "The box keeps changing: it may take in something that moves or blinks around the \
                     number. Pick the number again with a box around its digits only."
                )
            } else {
                n_!(
                    "The box doesn't show the number now (it looks different from when you picked it: \
                     a menu or the inventory closed?). Ferret waits until it's back."
                )
            };
            self.say(msg);
            self.status(&gettext(msg));
            if !shows && self.hides == 3 {
                self.phase(Phase::BoxChanging);
            }
        }
        shows
    }

    /// The area being watched.
    pub fn watched(&self) -> Option<Rect> {
        self.area
    }

    /// Reads the number just picked, and whether the game's learned digits read it. A read
    /// they didn't make is a guess to confirm; its frame is kept for `confirm`.
    pub fn read_picked(&mut self) -> Result<Option<(Shown, bool)>, String> {
        let area = self.area.ok_or("no area picked yet")?;
        let frame = self.frame()?;
        // The window got smaller since the capture the box was drawn on (or the game moved).
        if let Ok((w, h)) = image::image_dimensions(&frame) {
            if area.x >= w || area.y >= h {
                return Err(tr!("The box is outside the game picture now (the window got smaller?): capture again and pick it again."));
            }
        }
        self.picked_look = ocr::Look::of(&frame, area).ok();
        self.hidden = false;
        self.hides = 0;
        let read = self.read_frame(&frame)?;
        fs::rename(&frame, cache_dir().join("picked.png")).map_err(|e| e.to_string())?;
        Ok(read)
    }

    /// The player says the picked number read right: learn the game's digits from it.
    pub fn confirm(&mut self, n: &Shown) {
        self.learn(&cache_dir().join("picked.png"), n, false);
    }

    /// Learns the game's digits from the watched area of `frame`, which shows `n`. Failing to
    /// learn only gets logged.
    fn learn(&mut self, frame: &Path, n: &Shown, trusted: bool) {
        let (Some(area), Some(game)) = (self.area, self.game.as_ref()) else { return };
        if self.bar.is_some() {
            return;
        }
        let path = digits_path(&game.exe);
        // Scenery learned as digits gets read as numbers everywhere.
        if self.picked_look.as_ref().is_some_and(|picked| !ocr::Look::of(frame, area).is_ok_and(|now| now.like(picked))) {
            self.say(&format!("digits not learned: the box doesn't show {n} now (it looks different from when you picked it)"));
            return;
        }
        let msg = match ocr::learn(frame, area, n, &mut self.font, trusted) {
            Ok(msg) => self.font.save(&path).map(|_| msg).unwrap_or_else(|e| format!("could not save the digits: {e}")),
            Err(e) => format!("digits not learned: {e}"),
        };
        self.say(&msg);
    }

    /// The attached game's learned digits: the shapes of 0 to 9 (empty = not learned yet).
    pub fn digits(&self) -> Vec<Vec<font::DigitShape>> {
        (0..10).map(|d| self.font.shapes(d)).collect()
    }

    /// Forgets the shapes learned for one digit (a wrongly learned one reads numbers wrong).
    pub fn forget_digit(&mut self, d: u8) -> Result<(), String> {
        let game = self.game.as_ref().ok_or("attach to a game first")?;
        let path = digits_path(&game.exe);
        let n = self.font.forget(d);
        self.font.save(&path)?;
        self.say(&format!("forgot the {n} learned shapes of {d}"));
        Ok(())
    }

    /// With the value's address known, memory tells what the screen shows: learn from that.
    /// Only for whole numbers: a game may round or cut off a decimal it shows, and keep a timer
    /// in ticks rather than seconds.
    fn learn_from_memory(&mut self, loc: Loc, shown: &Shown) {
        if self.area.is_none() || shown.decimals() > 0 || shown.to_string().contains(':') {
            return;
        }
        let before = self.peek(&[loc])[0];
        let Ok(frame) = self.frame() else { return };
        // A value that changed while the frame was taken could show either.
        match (before, self.peek(&[loc])[0]) {
            (Some(a), Some(b)) if a == b => self.learn(&frame, &Shown::whole(a), true),
            _ => {}
        }
    }

    /// The places still matching when there are 20 or fewer (none otherwise), with their values
    /// as the game holds them.
    pub fn matches(&mut self) -> Vec<Match> {
        let reply = self.helper.call("list");
        if reply.iter().any(|l| l.starts_with("... ")) {
            return Vec::new();
        }
        let mut list: Vec<Match> = reply
            .iter()
            .filter_map(|l| {
                let (a, v) = l.split_once(" = ")?;
                Some(Match { loc: parse_loc(a)?, value: v.trim().to_owned(), about: None, saved: false })
            })
            .collect();
        let addrs: Vec<String> = list.iter().map(|m| format!("{:x}", m.loc.addr)).collect();
        for l in self.helper.call(&format!("about {}", addrs.join(" "))) {
            let Some((a, text)) = l.split_once(' ') else { continue };
            let addr = a.strip_prefix("0x").and_then(|a| u64::from_str_radix(a, 16).ok());
            for m in list.iter_mut().filter(|m| Some(m.loc.addr) == addr) {
                m.about = Some(text.to_owned());
            }
        }
        // A place already saved: the player knows whether that value works (Brotato: the
        // saved live health and a run-data copy kept following each other).
        if let Some(game) = self.game.as_ref() {
            for m in &mut list {
                let mut names: Vec<&str> = game.entries.iter().filter(|(_, l)| l.addr == m.loc.addr).map(|(n, _)| n.as_str()).collect();
                names.extend(game.named.iter().filter(|(_, _, p)| p.iter().any(|l| l.addr == m.loc.addr)).map(|(n, _, _)| n.as_str()));
                names.sort_unstable();
                names.dedup();
                if !names.is_empty() {
                    let saved = tr!("saved as {names}", names = names.join(", "));
                    m.about = Some(m.about.take().map_or(saved.clone(), |a| format!("{saved}: {a}")));
                    m.saved = true;
                }
            }
        }
        list
    }

    /// Shows the places still matching while a search runs, once they're few enough to list
    /// (more again after a rescan: none).
    fn tell_matches(&mut self, count: usize) {
        if self.on_matches.is_some() {
            let list = if count <= 20 { self.matches() } else { Vec::new() };
            if let Some(f) = self.on_matches.as_mut() {
                f(list);
            }
        }
    }

    /// Writes `value` to one of the places still matching, so the player can see whether the
    /// game shows it (when Ferret couldn't tell from the screen); the places a second later
    /// (a copy the game keeps rewriting is back to the old value by then).
    pub fn try_match(&mut self, loc: Loc, value: &str) -> Result<(Vec<Match>, String), String> {
        let v = Shown::parse(value).map(|s| s.value()).ok_or_else(|| tr!("not a number: {value}", value))?;
        let before = self.peek_exact(&[loc])[0];
        let reply = self.helper.call(&format!("write {loc} {v}"));
        if let Some(e) = first_error(&reply) {
            return Err(e);
        }
        self.say(&format!("wrote {v} to 0x{:x} ({}): does the game show it?", loc.addr, loc.kind.describe()));
        // As long as `sticks` waits: the stand-in refreshes its display copy about once a second.
        std::thread::sleep(Duration::from_millis(1500));
        // Ferret sees whether the write stayed: a place the game puts back at once (a copy it
        // refreshes from the real value) can't be set, so it isn't the one to save. A change
        // nearer the written value than the old one is the game carrying on from it.
        let now = self.peek_exact(&[loc])[0];
        let shown = |x: f64| number_text(x, None);
        if let (Some(before), Some(now)) = (before, now) {
            if (now - v).abs() > 1e-3 && (now - before).abs() <= (now - v).abs() {
                self.say(&format!("the game put {} back at once: ruling 0x{:x} out", shown(now), loc.addr));
                let left = self.drop_match(loc)?;
                let mut msg = tr!(
                    "The game put {n} back at once, so that place isn't where it keeps the value: removed it.",
                    n = shown(now)
                );
                if left.is_empty() {
                    msg.push(' ');
                    msg.push_str(&tr!("No places are left: press Start Over and search again."));
                }
                return Ok((left, msg));
            }
        }
        self.last_try = before.map(|b| (loc, b, v));
        let msg = tr!(
            "It kept {n}. If the game shows it too, press Use This One (some games only redraw a number when they change it themselves).",
            n = shown(now.unwrap_or(v))
        );
        Ok((self.matches(), msg))
    }

    /// The player says the game doesn't show the number tried last: what the place held before
    /// goes back (only the test's difference, when the game changed it meanwhile). It stays among
    /// the matches: some games only redraw a number when they change it themselves.
    pub fn put_back_try(&mut self) -> Vec<Match> {
        if let Some((loc, before, tried)) = self.last_try.take() {
            if let Some(now) = self.peek_exact(&[loc])[0] {
                let back = if (now - tried).abs() < 1e-3 { before } else { now - (tried - before) };
                if (now - tried).abs() < 1e-3 || (now - tried).abs() < (now - before).abs() {
                    self.helper.call(&format!("write {loc} {back}"));
                    self.say(&format!("put {} back at 0x{:x}", number_text(back, None), loc.addr));
                }
            }
        }
        self.matches()
    }

    fn phase(&mut self, p: Phase) {
        if let Some(f) = self.on_phase.as_mut() {
            f(p);
        }
    }

    fn ready(&mut self, count: usize) {
        self.phase(Phase::Ready(count));
    }

    /// A new scan for `n`, of the value types the player picked, in places shaped like earlier
    /// finds when there are any (unless the value turned out not to be in one).
    fn scan(&mut self, n: &Shown) -> Vec<String> {
        let kinds: Vec<&str> = self.scan_kinds.iter().map(|k| k.name()).collect();
        let mut cmd = format!("scan {}", n.search());
        if !kinds.is_empty() {
            cmd += &format!(" {}", kinds.join(","));
        }
        if self.unshaped {
            cmd += " all";
        }
        self.bar_last = None;
        let shown = n.to_string();
        let on_phase = &mut self.on_phase;
        let mut tell = |done| {
            if let Some(f) = on_phase.as_mut() {
                f(Phase::Scanning(shown.clone(), done));
            }
        };
        tell(0.0);
        let reply = self.helper.call_with(&cmd, &mut tell);
        self.shaped = reply.first().is_some_and(|l| l.contains("shaped like"));
        reply
    }

    /// The value wasn't where earlier finds were: the next scans look everywhere.
    fn lost_shape(&mut self) {
        if self.shaped {
            self.say("not in the places shaped like earlier finds after all: searching everywhere");
            self.shaped = false;
            self.unshaped = true;
        }
    }

    /// How many kinds of places earlier finds of this game were in: scans look there first.
    pub fn shape_count(&self) -> usize {
        self.shapes.iter().filter(|s| s.distinct()).count()
    }

    /// Forgets them (a shape can keep pointing searches at the wrong kind of place): scans look
    /// everywhere until the next find teaches one again.
    pub fn forget_shapes(&mut self) -> Result<usize, String> {
        let n = self.shape_count();
        let exe = self.game()?.exe.clone();
        self.shapes.clear();
        match fs::remove_file(shapes_path(&exe)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(format!("could not delete the shapes: {e}")),
            _ => {}
        }
        self.send_shapes();
        self.shaped = false;
        self.unshaped = false;
        self.say(&format!("forgot where earlier finds were ({}): searches look everywhere", kinds_of_places(n)));
        Ok(n)
    }

    fn send_shapes(&mut self) {
        let shapes: Vec<String> = self.shapes.iter().filter(|s| s.distinct()).map(Shape::text).collect();
        self.helper.call(&format!("shapes {}", shapes.join("; ")));
    }

    /// Learns the memory around a value just found, for finding the game's next values faster.
    pub fn learn_shape(&mut self, loc: Loc) {
        let Some(exe) = self.game.as_ref().map(|g| g.exe.clone()) else { return };
        self.unshaped = false;
        let reply = self.helper.call(&format!("shape {loc}"));
        let Some(found) = reply.first().and_then(|l| l.strip_prefix("shape ")).and_then(Shape::parse) else {
            return;
        };
        let what = match shapes::learn(&mut self.shapes, found) {
            Learned::Known => return,
            Learned::Plain => {
                self.say("nothing distinctive around it to look for the next value by");
                return;
            }
            Learned::Narrowed => "narrowed the shape it shares with earlier finds",
            Learned::New => "learned how the game keeps values like this one",
        };
        match shapes::save(&shapes_path(&exe), &self.shapes) {
            Ok(()) => self.say(&format!("{what}: new searches look in places shaped like it first")),
            Err(e) => self.say(&format!("could not save the shapes: {e}")),
        }
        self.send_shapes();
    }

    /// The order to try the places left one at a time: whole numbers before decimals (games keep
    /// most counts as whole numbers), places Ferret can name before others, places already saved
    /// as another value last.
    pub fn try_order(list: &mut [Match]) {
        list.sort_by_key(|m| {
            (m.saved, matches!(m.loc.kind, Kind::F32 | Kind::F64), m.about.is_none())
        });
    }

    /// The player ruled out one of the places still matching; the places left.
    pub fn drop_match(&mut self, loc: Loc) -> Result<Vec<Match>, String> {
        let reply = self.helper.call(&format!("drop {loc}"));
        let count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("drop failed".into()))?;
        self.say(&format!("ruled out 0x{:x} ({}): {count} left", loc.addr, loc.kind.describe()));
        self.search = self.search.map(|(_, unchanged)| (count, unchanged)).filter(|_| count > 0);
        Ok(self.matches())
    }

    /// The player picked the value among the places still matching.
    pub fn choose(&mut self, loc: Loc) -> Result<Loc, String> {
        let reply = self.helper.call(&format!("keep {loc}"));
        if match_count(&reply) != Some(1) {
            return Err(tr!("{place} isn't among the matches any more", place = format!("0x{:x}", loc.addr)));
        }
        self.search = None;
        self.say(&format!("picked 0x{:x} ({}) as the value", loc.addr, loc.kind.describe()));
        self.learn_shape(loc);
        Ok(loc)
    }

    /// Current scan candidates (at most 20 are listed by the helper).
    pub fn candidates(&mut self) -> Vec<(Loc, Option<i64>)> {
        parse_values(&self.helper.call("list"))
    }

    /// Tells the real value apart from copies of it, all candidates at once: each gets its own
    /// test value (+100, +200, ...). The screen showing one of them, or other candidates taking
    /// one on (a copy the game refreshes from the real value), tells which it is. Else one at a
    /// time, +10 each (a copy that only follows a rise won't leave a higher test value of its
    /// own). When neither does (some games only redraw a number when they change it
    /// themselves: Lumencraft), the player changes the number in the game and the real one
    /// carries on from its test value; copies don't. Test writes are undone, keeping what the
    /// game changed meanwhile. Only places holding `n` (the number on screen) get one: a place
    /// that only swept past it between reads can be anything (a stand-in's malloc chunk header
    /// went 0 -> 69 -> 117 across 74 and 85; its test value crashed the game).
    pub fn probe(&mut self, n: &Shown) -> Result<Probed, String> {
        let listed = self.helper.call("list");
        if listed.iter().any(|l| l.starts_with("...")) {
            return Err("too many candidates to probe; narrow down first".into());
        }
        let addrs: Vec<Loc> = parse_values(&listed).into_iter().map(|(a, _)| a).collect();
        if addrs.is_empty() {
            return Err("no candidates to probe".into());
        }
        let mut tests: Vec<Probe> = Vec::new();
        for &loc in &addrs {
            // The exact original, fraction included, to put back afterwards.
            let listed = self.helper.call(&format!("peek {loc}"));
            let Some(orig) = listed.first().and_then(|l| l.split_once(" = ")).map(|(_, v)| v.trim().to_owned()) else {
                continue;
            };
            let Some(Some(now)) = self.peek(&[loc]).first().copied() else { continue };
            if !shows(now, n) {
                self.say(&format!("0x{:012x}: holds {now}, not {n}: left alone", loc.addr));
                continue;
            }
            let step = PROBE_STEP * (tests.len() as i64 + 1);
            tests.push(Probe { loc, orig, test: now + step, step });
        }
        if tests.is_empty() {
            self.say(&format!("none of them holds {n} now"));
            return Ok(Probed::Unclear);
        }
        self.phase(Phase::Checking(tests.len(), 0.0));
        for t in &tests {
            self.helper.call(&format!("write {} {} test", t.loc, t.test));
        }
        std::thread::sleep(Duration::from_millis(1500));
        let locs: Vec<Loc> = tests.iter().map(|t| t.loc).collect();
        let after = self.peek(&locs);
        let screen = if self.area.is_some() { self.read().ok().flatten() } else { None };
        // A display that slides toward the value is still on its way (SuperTux's coin counter
        // counts to the real number: from its test value 305 toward the real one's 205, it read
        // 270 and passed as kept, with the screen showing it). A second look tells it from a
        // place the game changed once: it moved on toward another place's test value, which
        // that place still holds.
        std::thread::sleep(Duration::from_millis(500));
        let later = self.peek(&locs);
        let chases: Vec<Option<usize>> = (0..tests.len())
            .map(|k| {
                let (Some(a), Some(b), t) = (after[k], later[k], tests[k].test) else { return None };
                (0..tests.len()).filter(|&j| j != k && later[j] == Some(tests[j].test)).find(|&j| {
                    let goal = tests[j].test;
                    let toward = |v: i64| (v - goal).abs() < (t - goal).abs() && (v - t).signum() == (goal - t).signum();
                    toward(a) && toward(b) && (b - goal).abs() < (a - goal).abs()
                })
            })
            .collect();
        // Each place kept its test value (or carried on from it), took another's on, slides
        // toward another's, or went back.
        let kept: Vec<bool> = (0..tests.len())
            .map(|k| chases[k].is_none() && after[k].is_some_and(|v| (v - tests[k].test).abs() <= PROBE_NEAR))
            .collect();
        let source = |v: Option<i64>, k: usize| tests.iter().enumerate().position(|(j, t)| j != k && v == Some(t.test));
        let followers: Vec<usize> = (0..tests.len())
            .map(|j| (0..tests.len()).filter(|&k| source(after[k], k) == Some(j) || chases[k] == Some(j)).count())
            .collect();
        for (k, (t, v)) in tests.iter().zip(&after).enumerate() {
            let what = match (kept[k], source(*v, k), chases[k]) {
                (true, _, _) => "kept".to_owned(),
                (false, Some(j), _) => format!("took 0x{:012x}'s test value", tests[j].loc.addr),
                (false, None, Some(j)) => format!(
                    "slides toward 0x{:012x}'s test value ({} -> {}): a display counting to it",
                    tests[j].loc.addr,
                    v.unwrap_or_default(),
                    later[k].unwrap_or_default()
                ),
                (false, None, None) => format!("the game put back {}", v.map_or("something else".into(), |v| v.to_string())),
            };
            self.say(&format!("0x{:012x}: wrote {}: {what}", t.loc.addr, t.test));
        }
        if let Some(s) = &screen {
            self.say(&format!("screen shows {s}"));
        }
        let on_screen = |s: &Shown, t: &Probe| [s.value() as i64, s.scaled()].iter().any(|v| (v - t.test).abs() <= PROBE_NEAR);
        let mut real = screen
            .as_ref()
            .and_then(|s| (0..tests.len()).find(|&k| kept[k] && on_screen(s, &tests[k])))
            .map(|k| (k, format!("held {} and the screen showed it", tests[k].test)));
        if real.is_none() {
            // The one the others copy: alone in having followers.
            let most = (0..tests.len()).filter(|&k| kept[k]).max_by_key(|&k| followers[k]).filter(|&k| followers[k] > 0);
            real = most
                .filter(|&k| (0..tests.len()).filter(|&j| kept[j] && followers[j] == followers[k]).count() == 1)
                .map(|k| (k, format!("held {} and {} other places copied it", tests[k].test, followers[k])));
        }
        self.undo_probes(&tests);
        // Copies that only follow some changes (a highest-so-far that only follows a rise) may
        // have got a test value they won't leave: one place at a time, each only +10.
        if real.is_none() && kept.iter().any(|&k| k) {
            real = self.probe_one_by_one(&tests, &kept);
        }
        if real.is_none() && kept.iter().any(|&k| k) {
            let now = self.peek(&locs);
            for (k, t) in tests.iter_mut().enumerate() {
                match now[k] {
                    Some(v) if kept[k] => {
                        t.test = v + t.step;
                        self.helper.call(&format!("write {} {} test", t.loc, t.test));
                    }
                    _ => {}
                }
            }
            let found = self.probe_in_game(&tests, &kept);
            self.undo_probes(&tests);
            self.tell_matches(tests.len());
            match found {
                Ok(found) => real = found,
                Err(why) => {
                    self.say(&format!("no candidate behaved like the real value: {why}"));
                    return Ok(Probed::Unclear);
                }
            }
        }
        if let Some((k, how)) = real {
            let loc = tests[k].loc;
            self.helper.call(&format!("keep {:x}", loc.addr));
            self.say(&format!("0x{:012x} {how}: the real value (test writes undone)", loc.addr));
            return Ok(Probed::Found(loc));
        }
        let why = match (kept.iter().any(|&k| k), self.cancel.load(Ordering::Relaxed)) {
            (false, _) => "the game put every test value back",
            (true, true) => "stopped before the game changed the number",
            (true, false) => "the number didn't change in the game while Ferret waited (is the game paused?)",
        };
        self.say(&format!("no candidate behaved like the real value: {why}"));
        Ok(if kept.iter().any(|&k| k) { Probed::Unclear } else { Probed::PutBack })
    }

    /// Puts back what the test writes changed: the exact original where the test value is still
    /// there, only the step where the game changed it since; a copy the game rewrote is left
    /// alone.
    fn undo_probes(&mut self, tests: &[Probe]) {
        let locs: Vec<Loc> = tests.iter().map(|t| t.loc).collect();
        let now = self.peek(&locs);
        for (t, v) in tests.iter().zip(now) {
            match v {
                Some(v) if v == t.test => {
                    self.helper.call(&format!("write {} {}", t.loc, t.orig));
                }
                Some(v) if (v - t.test).abs() <= PROBE_NEAR => {
                    self.helper.call(&format!("write {} {}", t.loc, (v - t.step).max((t.test - t.step).min(0))));
                }
                _ => {}
            }
        }
    }

    /// One candidate at a time (of those that kept their test value): +10 for 1.5 s, then
    /// whether it kept it and other candidates followed it or the screen shows it. Undone
    /// before the next.
    fn probe_one_by_one(&mut self, tests: &[Probe], kept: &[bool]) -> Option<(usize, String)> {
        self.say("checking them one at a time");
        let locs: Vec<Loc> = tests.iter().map(|t| t.loc).collect();
        let checked: Vec<usize> = (0..tests.len()).filter(|&k| kept[k]).collect();
        for (i, &k) in checked.iter().enumerate() {
            self.phase(Phase::Checking(tests.len(), (i + 1) as f64 / (checked.len() + 1) as f64));
            let loc = tests[k].loc;
            let listed = self.helper.call(&format!("peek {loc}"));
            let Some(orig) = listed.first().and_then(|l| l.split_once(" = ")).map(|(_, v)| v.trim().to_owned()) else { continue };
            let Some(Some(shown)) = self.peek(&[loc]).first().copied() else { continue };
            let test = shown + 10;
            self.helper.call(&format!("write {loc} {test} test"));
            std::thread::sleep(Duration::from_millis(1500));
            let after = self.peek(&locs);
            let stuck = after[k] == Some(test);
            let followers = (0..tests.len()).filter(|&j| j != k && after[j] == Some(test)).count();
            let screen = if self.area.is_some() { self.read().ok().flatten() } else { None };
            let on_screen = screen.as_ref().is_some_and(|s| s.value() as i64 == test || s.scaled() == test);
            self.say(&format!(
                "0x{:012x}: wrote {test}: {}, {followers} of {} others followed, screen shows {}",
                loc.addr,
                if stuck { "kept" } else { "the game changed it" },
                tests.len() - 1,
                screen.as_ref().map_or("?".into(), |n| n.to_string()),
            ));
            match after[k] {
                Some(v) if v == test => {
                    self.helper.call(&format!("write {loc} {orig}"));
                }
                // The game changed it meanwhile: only the test's +10 comes off.
                Some(v) if (v - test).abs() < (v - shown).abs() => {
                    self.helper.call(&format!("write {loc} {}", v - 10));
                }
                _ => {}
            }
            if stuck && (followers > 0 || on_screen) {
                let how = match on_screen {
                    true => format!("held {test} and the screen showed it"),
                    false => format!("held {test} and {followers} other places followed it"),
                };
                return Some((k, how));
            }
        }
        None
    }

    /// Code patterns for a value nothing touched while Ferret watched: many only change when the
    /// player acts (ammo when firing), and the code that changes them then is the lasting way to
    /// find them (pointer paths to Wolfenstein's ammo were luck and broke at a restart). The
    /// player changes the number in the game; Ferret watches until the game touches it or the
    /// player cancels. Only with a window to ask in (the CLI's scripts expect the old way).
    fn sites_when_changed(&mut self, loc: Loc) -> Vec<String> {
        if self.on_phase.is_none() {
            return Vec::new();
        }
        self.cancel.store(false, Ordering::Relaxed);
        self.phase(Phase::SaveTurn);
        self.status(&tr!(
            "Now change the number in the game once (use some or pick some up): Ferret watches which of the game's code \
             does it, the surest way to find it again. Cancel saves it another way."
        ));
        let cancel = self.cancel.clone();
        let reply = self.helper.call_cancellable(&format!("sites {loc} 1 wait"), &cancel);
        let cancelled = self.cancel.swap(false, Ordering::Relaxed);
        self.phase(Phase::Checking(1, 0.0));
        for l in reply.iter().filter(|l| !l.starts_with("site ")) {
            self.say(l);
        }
        if cancelled && !reply.iter().any(|l| l.starts_with("site ")) {
            self.say("stopped waiting for the game to change it");
        }
        reply.iter().filter_map(|l| l.strip_prefix("site ")).map(str::to_owned).collect()
    }

    /// The player changes the number in the game while every candidate that kept its test
    /// value still holds it: the game's own value carries on from its test value (in memory,
    /// and on screen once redrawn), copies don't. The index of the real one, and how it showed.
    /// Err: the game changed them all at once, so none can be told apart this way.
    fn probe_in_game(&mut self, tests: &[Probe], kept: &[bool]) -> Result<Option<(usize, String)>, String> {
        // The player has to go to the game and gather or use some (Astro Colony's copper: 45 s
        // ran out first); Stop gives up sooner.
        const WAIT: Duration = Duration::from_secs(180);
        let watched: Vec<usize> = (0..tests.len()).filter(|&k| kept[k]).collect();
        let locs: Vec<Loc> = watched.iter().map(|&k| tests[k].loc).collect();
        self.say(&format!(
            "neither the screen nor the other places told which one it is: waiting up to {} s for the game to change it",
            WAIT.as_secs()
        ));
        self.phase(Phase::YourTurn(watched.len(), WAIT.as_secs()));
        self.status(&ntr!(
            "Now change the number in the game once (pick some up or use some): Ferret waits and watches which \
             of the {n} place the game carries on from. Stop Waiting lists them to try instead.",
            "Now change the number in the game once (pick some up or use some): Ferret waits and watches which \
             of the {n} places the game carries on from. Stop Waiting lists them to try instead.",
            watched.len()
        ));
        self.tell_matches(tests.len());
        // Watchpoints: the first one the game writes is the real value, even when it changes the
        // copies in the same frame. Polling below when the game can't be traced.
        let arg: Vec<String> = locs.iter().map(|l| l.to_string()).collect();
        let cancel = self.cancel.clone();
        let reply = self.helper.call_cancellable(&format!("first {} {}", arg.join(" "), WAIT.as_secs()), &cancel);
        if let Some((loc, v)) = reply.iter().find_map(|l| l.strip_prefix("first ")).and_then(|l| {
            let (a, v) = l.split_once(' ')?;
            Some((parse_loc(a)?, v.parse::<f64>().ok()?))
        }) {
            let Some(&i) = watched.iter().find(|&&k| tests[k].loc.addr == loc.addr) else { return Ok(None) };
            let v = v.round() as i64;
            // It took another's test value: that one is the source.
            if let Some(&k) = watched.iter().find(|&&k| k != i && tests[k].test == v) {
                return Ok(Some((k, format!("held {} and 0x{:012x} took it from there", tests[k].test, loc.addr))));
            }
            return Ok(Some((i, format!("the game changed it first, from {} to {v}", tests[i].test))));
        }
        if reply.iter().any(|l| l == "none") {
            return Ok(None);
        }
        let start = Instant::now();
        let mut last = None;
        let mut seen = Vec::new();
        while start.elapsed() < WAIT && !self.cancel.load(Ordering::Relaxed) {
            self.run_side_jobs();
            // Memory: the one the game changed, starting from its test value.
            let now = self.peek(&locs);
            if let Some((&k, v)) = watched.iter().zip(&now).find(|(&k, v)| v.is_some_and(|v| v != tests[k].test && (v - tests[k].test).abs() <= PROBE_NEAR)) {
                return Ok(Some((k, format!("went from {} to {}", tests[k].test, v.unwrap()))));
            }
            // A game may not carry on from the test value (Brotato's health: capped at the most
            // first, 230 -> 18). The real one changes first and copies follow it: one changed
            // while the others still hold their test values is the real one, unless it took
            // another's test value (then that one is).
            let moved: Vec<usize> = (0..watched.len()).filter(|&i| now[i].is_some_and(|v| v != tests[watched[i]].test)).collect();
            if watched.len() > 1 {
                if let [i] = moved[..] {
                    let v = now[i].unwrap_or_default();
                    let k = watched.iter().copied().find(|&k| k != watched[i] && tests[k].test == v).unwrap_or(watched[i]);
                    let how = match k == watched[i] {
                        true => format!("the game changed it from {} to {v} first", tests[k].test),
                        false => format!("held {} and another place took it", tests[k].test),
                    };
                    return Ok(Some((k, how)));
                }
                if moved.len() == watched.len() {
                    let values: Vec<String> = now.iter().map(|v| v.map_or("?".into(), |v| v.to_string())).collect();
                    return Err(format!(
                        "the game changed all {} at once (to {}): copies of one value, too quick to tell which came first",
                        watched.len(),
                        values.join(", ")
                    ));
                }
            }
            // The matches list shows what the places hold (the worker can't re-list it meanwhile).
            if now != seen {
                self.tell_matches(tests.len());
                seen = now;
            }
            if self.area.is_none() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
            // Screen: a game that does redraw shows the test value (or what followed it).
            let Ok(Some(s)) = self.read() else { continue };
            if let Some(&k) = watched.iter().find(|&&k| [s.value() as i64, s.scaled()].iter().any(|v| (v - tests[k].test).abs() <= PROBE_NEAR)) {
                return Ok(Some((k, format!("held {} and the screen showed {s}", tests[k].test))));
            }
            if last.as_ref() != Some(&s) {
                self.say(&format!("screen shows {s}"));
                last = Some(s);
            }
        }
        Ok(None)
    }

    /// Two reads in a row that agree.
    fn read_stable(&mut self) -> Result<Option<Shown>, String> {
        let a = self.read_learned()?.map(|(n, _)| n);
        let b = self.read_learned()?.map(|(n, _)| n);
        Ok(if a == b { a } else { None })
    }

    /// Whether the one match left is the value itself: a test write (undone right after) has to
    /// stick. A game that keeps its own copy of a value for the HUD rewrites that copy every
    /// frame, and a search for a value stored some way Ferret doesn't look for (a 2-byte int,
    /// another encoding) ends on that copy; writing it changes nothing and saving it fails.
    fn sticks(&mut self, loc: Loc) -> bool {
        let listed = self.helper.call(&format!("peek {loc}"));
        let (Some(orig), Some(Some(shown))) =
            (listed.first().and_then(|l| l.split_once(" = ")).map(|(_, v)| v.trim().to_owned()), self.peek(&[loc]).first().copied())
        else {
            return true;
        };
        let test = shown + 10;
        self.phase(Phase::Checking(1, 0.0));
        self.helper.call(&format!("write {loc} {test} test"));
        std::thread::sleep(Duration::from_millis(1500));
        let now = self.peek(&[loc])[0];
        if now == Some(test) {
            self.helper.call(&format!("write {loc} {orig}"));
            return true;
        }
        // The game changed it meanwhile (the player gathered something): still the value, nearer
        // the test value than where a copy would be put back to. Undo only the test's +10.
        if let Some(v) = now.filter(|v| (v - test).abs() < (v - shown).abs()) {
            self.helper.call(&format!("write {loc} {}", v - 10));
            return true;
        }
        self.say(&format!(
            "wrote {test} to the last match as a test and the game put back {}: it's a copy the game keeps \
             refreshing (for its display), not where it keeps the value",
            now.map_or("something else".into(), |v| v.to_string())
        ));
        false
    }

    /// Why the one match left may not be the value: it doesn't hold what the screen shows (`n`;
    /// a match reached through misreads can be anything), or a test write doesn't stick (a
    /// display copy). The match stays for the player to try either way.
    fn doubt(&mut self, loc: Loc, n: &Shown) -> Option<String> {
        let v = self.peek(&[loc])[0];
        if !v.is_some_and(|v| shows(v, n)) {
            let held = v.map_or("nothing readable".into(), |v| v.to_string());
            self.say(&format!("the last match holds {held} but the screen shows {n}: kept for the player to try"));
            return Some(tr!("The one place left holds {held}, not the {n} on screen.", held, n));
        }
        if !self.sticks(loc) {
            return Some(copy_only());
        }
        None
    }

    /// The automated scan loop: read the number off the window, scan for it, and
    /// keep narrowing down whenever it changes on screen. Stops early when
    /// `cancel` is set. Continues a search that was stopped or typed into.
    pub fn auto(&mut self, limit: Duration) -> Result<AutoResult, String> {
        self.cancel.store(false, Ordering::Relaxed);
        self.scan_now.store(false, Ordering::Relaxed);
        if self.bar.is_some() {
            return self.auto_bar(limit);
        }
        let cancelled = |c: &Arc<AtomicBool>| c.load(Ordering::Relaxed);
        let start = Instant::now();
        let first = loop {
            if let Some(n) = self.read_stable()? {
                break n;
            }
            if start.elapsed() > limit || cancelled(&self.cancel) {
                return Err(tr!("could not read the number"));
            }
        };
        let continuing = self.search.is_some();
        let mut count = match self.search {
            Some((before, _)) => {
                let reply = self.helper.call(&format!("next {}", first.search()));
                self.say(&format!("screen shows {first}, continuing from {before} matches: {}", reply.join(" ")));
                match_count(&reply).unwrap_or(0)
            }
            None => 0,
        };
        // One place shaped like an earlier find may be another item that happens to hold the
        // same number: it has to follow the number once before it counts as found.
        let mut confirm = false;
        if count == 0 {
            if continuing {
                self.lost_shape();
            }
            let reply = self.scan(&first);
            self.say(&format!("screen shows {first}: {}", reply.join(" ")));
            count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()))?;
            confirm = self.shaped && count == 1;
            if confirm {
                self.say("one place shaped like earlier finds holds it: waiting for the number to change once to be sure");
            }
        }
        self.search = Some((count, 0));
        self.tell_matches(count);
        self.ready(count);
        let mut last = first.clone();
        let mut unchanged_rounds = 0;
        // A number nothing fits: a misread or something covering the number, unless it's read
        // again right after.
        let mut unfit = None;
        while (count > 1 || confirm) && start.elapsed() < limit && !cancelled(&self.cancel) {
            std::thread::sleep(Duration::from_millis(300));
            self.run_side_jobs();
            // Values that change while the screen is read may show either end of that change.
            self.helper.call("mark");
            let Some(now) = self.read_stable()? else { continue };
            let still = now == last;
            let again = self.scan_now.swap(false, Ordering::Relaxed);
            if still && !again {
                continue;
            }
            let reply = self.helper.call(&format!("next {}", now.search()));
            let new_count = match_count(&reply).unwrap_or(0);
            self.say(&format!("screen {} {now}: {}", if still { "still shows" } else { "shows" }, reply.join(" ")));
            if new_count == 0 {
                if unfit.as_ref() != Some(&now) {
                    self.say(&format!("nothing fits {now} (a misread?), keeping the {count} matches"));
                    unfit = Some(now);
                    continue;
                }
                self.lost_shape();
                let reply = self.scan(&now);
                self.say(&format!("{now} again: lost it, rescanning: {}", reply.join(" ")));
                count = match_count(&reply).unwrap_or(0);
                confirm = false;
                unchanged_rounds = 0;
                self.ready(count);
            } else {
                unchanged_rounds = if new_count == count { unchanged_rounds + 1 } else { 0 };
                // Every change: the listed values follow the game.
                self.tell_matches(new_count);
                self.phase(if again { Phase::ScannedAgain(count, new_count) } else { Phase::Watching(new_count) });
                count = new_count;
                confirm = false;
            }
            unfit = None;
            self.search = Some((count, 0));
            last = now;
            // Few enough to test one by one: once they stop shrinking, more changes rarely
            // tell them apart (Astro Colony: 5 places followed 19, 20 and 21).
            if unchanged_rounds >= 3 || (count <= 20 && unchanged_rounds >= 1) {
                self.say(&format!("{count} addresses keep following the value (likely the value plus copies of it)"));
                break;
            }
        }
        self.searched_decimals = last.decimals();
        if cancelled(&self.cancel) {
            self.say("stopped");
        } else if count > 1 && start.elapsed() >= limit {
            self.say("time limit reached; the value never changed enough to narrow down to one address");
        }
        for l in self.helper.call("list") {
            self.say(&l);
        }
        if count == 0 {
            self.search = None;
        }
        if count == 1 {
            let loc = self.candidates()[0].0;
            if let Some(why) = self.doubt(loc, &last) {
                // Only places shaped like earlier finds were searched: the value is elsewhere.
                if self.shaped && !cancelled(&self.cancel) && start.elapsed() < limit {
                    self.lost_shape();
                    self.search = None;
                    return self.auto(limit - start.elapsed());
                }
                return Ok(AutoResult::Unsure(why));
            }
            self.search = None;
            self.say(&format!("stored as {}", loc.kind.with_article()));
            self.learn_from_memory(loc, &last);
            self.learn_shape(loc);
            return Ok(AutoResult::Found(loc));
        }
        if (2..=20).contains(&count) && !cancelled(&self.cancel) {
            self.say("checking which one is the real value:");
            match self.probe(&last)? {
                Probed::Found(loc) => {
                    self.search = None;
                    self.say(&format!("stored as {}", loc.kind.with_article()));
                    self.learn_from_memory(loc, &last);
                    self.learn_shape(loc);
                    return Ok(AutoResult::Found(loc));
                }
                Probed::PutBack => return Ok(AutoResult::PutBack(count)),
                Probed::Unclear => {}
            }
        }
        Ok(AutoResult::Several(count))
    }

    /// How full the bar is in a fresh frame, and how far off that may be (shares of the whole).
    fn read_bar(&mut self) -> Result<(f64, f64), String> {
        let frame = self.frame()?;
        if let Some(f) = self.on_frame.as_mut() {
            f(&frame, self.area);
        }
        let img = image::open(&frame).map_err(|e| e.to_string())?.to_rgb8();
        Ok(self.bar.as_ref().ok_or("no bar picked")?.fill(&img))
    }

    /// `read_bar`, when two reads in a row agree (the bar isn't sliding or flashing) and it isn't
    /// empty: an empty bar is no share of anything, and a menu over the bar reads as one.
    fn read_bar_stable(&mut self) -> Result<Option<(f64, f64)>, String> {
        let (a, b) = (self.read_bar()?, self.read_bar()?);
        Ok(((a.0 - b.0).abs() <= a.1.max(b.1) && b.0 > b.1).then_some(b))
    }

    /// Takes a new copy of the game's memory, the bar `now` full: a bar's search starts there.
    fn bar_copy(&mut self, now: (f64, f64)) -> Result<(), String> {
        let mut cmd = format!("snap {}", cache_dir().join("memory.copy").display());
        let kinds: Vec<&str> = self.scan_kinds.iter().map(|k| k.name()).collect();
        if !kinds.is_empty() {
            cmd += &format!(" {}", kinds.join(","));
        }
        let on_phase = &mut self.on_phase;
        let mut tell = |done| {
            if let Some(f) = on_phase.as_mut() {
                f(Phase::Copying(done));
            }
        };
        tell(0.0);
        let reply = self.helper.call_with(&cmd, &mut tell);
        self.say(&format!("{} full: {}", self.shown(now.0), reply.join(" ")));
        if let Some(e) = first_error(&reply) {
            return Err(e);
        }
        self.search = None;
        self.bar_last = Some(now);
        self.phase(Phase::BarReady);
        Ok(())
    }

    /// One step of a bar's search: keeps the places whose value moved as the bar did since the
    /// last step (`bar_last`; first, since the copy of memory). The places left, or None when
    /// none moved so (kept as they were: a misread, something over the bar, or the value was
    /// lost). Some(0) while the copy waits for the bar to move more.
    fn bar_step(&mut self, now: (f64, f64)) -> Option<usize> {
        let last = match self.bar_last {
            Some(last) => last,
            // Matches from typed numbers: their values now go with the bar now.
            None if self.search.is_some() => {
                self.helper.call("mark");
                self.bar_last = Some(now);
                return self.search.map(|(n, _)| n);
            }
            None => return None,
        };
        let err = last.1.max(now.1);
        let reply = self.helper.call(&format!("next bar {} {} {err}", last.0, now.0));
        if let Some(e) = first_error(&reply) {
            self.say(&e);
            // Nothing to compare with (the copy went with a restart of the game): lost.
            return match e.contains("no copy") {
                true => None,
                false => Some(0).filter(|_| self.search.is_none()),
            };
        }
        self.say(&format!("{} -> {}: {}", self.shown(last.0), self.shown(now.0), reply.join(" ")));
        let count = match_count(&reply).unwrap_or(0);
        if count == 0 {
            return Some(0).filter(|_| self.search.is_none());
        }
        self.bar_last = Some(now);
        self.search = Some((count, 0));
        Some(count)
    }

    /// The bar's search, as `auto` for numbers: a copy of the game's memory while the bar
    /// stays, then every move of the bar keeps the places whose value moved alike (73% of what
    /// it was when the bar went from full to 73%).
    fn auto_bar(&mut self, limit: Duration) -> Result<AutoResult, String> {
        let cancelled = |c: &Arc<AtomicBool>| c.load(Ordering::Relaxed);
        let start = Instant::now();
        let first = loop {
            if let Some(f) = self.read_bar_stable()? {
                break f;
            }
            if start.elapsed() > limit || cancelled(&self.cancel) {
                return Err(match self.icons() {
                    true => tr!("The icons are all empty, hidden or keep changing."),
                    false => tr!("The bar is empty, hidden or keeps moving."),
                });
            }
        };
        let mut count = 0;
        match (self.search, self.bar_last) {
            (Some((n, _)), _) => {
                self.bar_step(first);
                count = n;
                self.say(&format!("{} full, continuing from {n} matches", self.shown(first.0)));
                self.ready(count);
            }
            // The copy is there from an earlier Start: it still waits for the bar to move.
            (None, Some(_)) => self.phase(Phase::BarReady),
            (None, None) => self.bar_copy(first)?,
        }
        let mut last = first;
        let mut unchanged_rounds = 0;
        // A share nothing fits: a misread or something over the bar, unless it's read again
        // after the bar moved off it and back.
        let mut unfit: Option<f64> = None;
        while count != 1 && start.elapsed() < limit && !cancelled(&self.cancel) {
            std::thread::sleep(Duration::from_millis(300));
            self.run_side_jobs();
            // Values that change again while the screen is read fit what it showed then.
            if self.search.is_some() {
                self.helper.call("mark bar");
            }
            let Some(now) = self.read_bar_stable()? else { continue };
            let again = self.scan_now.swap(false, Ordering::Relaxed);
            let near = |a: f64| (a - now.0).abs() <= now.1;
            if (near(last.0) || unfit.is_some_and(near)) && !again {
                continue;
            }
            let Some(new_count) = self.bar_step(now) else {
                match unfit {
                    // Twice: the value was lost (or never kept). Start again from here.
                    Some(_) => {
                        self.say("nothing moved like the screen twice: lost it, copying the game's memory again");
                        self.bar_copy(now)?;
                        count = 0;
                        unfit = None;
                        last = now;
                    }
                    None => {
                        self.say("nothing moved like the screen (a misread?), keeping the matches");
                        unfit = Some(now.0);
                    }
                }
                continue;
            };
            // The copy still waits for a bigger move.
            if new_count == 0 {
                continue;
            }
            unfit = None;
            unchanged_rounds = if new_count == count { unchanged_rounds + 1 } else { 0 };
            self.tell_matches(new_count);
            match count {
                0 => self.ready(new_count),
                _ if again => self.phase(Phase::ScannedAgain(count, new_count)),
                _ => self.phase(Phase::Watching(new_count)),
            }
            count = new_count;
            last = now;
            if unchanged_rounds >= 3 || (count <= 20 && unchanged_rounds >= 1) {
                self.say(&format!("{count} addresses keep following the screen (likely the value plus copies of it)"));
                break;
            }
        }
        if cancelled(&self.cancel) {
            self.say("stopped");
        } else if count != 1 && start.elapsed() >= limit {
            self.say("time limit reached; the screen never changed enough to narrow down to one address");
        }
        let end = self.bar_end(count, last);
        // Only a copy the game redraws the screen from was left: the value itself was lost on the
        // way (Isaac's hearts). Start again rather than stop with nothing.
        if matches!(&end, Ok(AutoResult::Unsure(why)) if *why == copy_only()) && !cancelled(&self.cancel) && start.elapsed() < limit {
            self.say("the one place left is a copy the game redraws the screen from: lost the value, copying the game's memory again");
            self.helper.call("track");
            self.search = None;
            self.bar_last = None;
            return self.auto_bar(limit - start.elapsed());
        }
        end
    }

    /// How a bar's search ends with `count` places left and the bar `now` full: up to 20 are
    /// checked by writing half their value, or twice it when the bar is half full or less
    /// (undone after), which the bar on screen shows.
    fn bar_end(&mut self, count: usize, now: (f64, f64)) -> Result<AutoResult, String> {
        self.searched_decimals = 0;
        if count == 0 {
            return Err(match self.icons() {
                true => tr!("Stopped before the icons changed. Press Start again and let them change in the game."),
                false => tr!("Stopped before the bar moved. Press Start again and let the bar change in the game."),
            });
        }
        for l in self.helper.call("list") {
            self.say(&l);
        }
        if count > 20 || cancelled_now(&self.cancel) {
            return Ok(AutoResult::Several(count));
        }
        let cands: Vec<Loc> = self.candidates().into_iter().map(|(l, _)| l).collect();
        let mut held = 0;
        for (i, &loc) in cands.iter().enumerate() {
            self.phase(Phase::Checking(cands.len(), i as f64 / cands.len() as f64));
            match self.bar_moves(loc, now) {
                Some(true) => {
                    // The others aren't it: Save takes the one place left.
                    self.helper.call(&format!("keep {loc}"));
                    self.search = None;
                    self.bar_last = None;
                    self.say(&format!("stored as {}", loc.kind.with_article()));
                    self.learn_shape(loc);
                    return Ok(AutoResult::Found(loc));
                }
                Some(false) => held += 1,
                None => {}
            }
        }
        match (count, held) {
            // Kept its test value but the bar didn't show it: maybe a game that redraws only on
            // its own changes, maybe something else that moved alike (SPD: a junk number of a
            // billion passed every step). The player tries it.
            (1, 1) => Ok(AutoResult::Unsure(match self.icons() {
                true => tr!("The one place left kept a test value, but the icons didn't show it."),
                false => tr!("The one place left kept a test value, but the bar didn't show it."),
            })),
            (1, _) => Ok(AutoResult::Unsure(copy_only())),
            _ => Ok(AutoResult::Several(count)),
        }
    }

    /// Whether `loc` is the bar's value: half of it as a test value halves the bar (`now` full),
    /// twice it doubles a bar half full or less. Halving there could kill: Isaac with one heart
    /// left had half of one for the test's 1.5 s, and a fly took it. Some(true) = the bar showed
    /// it, Some(false) = it kept the test value (or what the game made of it, no further from it
    /// than the test moved it) but the bar didn't show it, None = the game put something else
    /// there (a copy) or it can't be halved.
    fn bar_moves(&mut self, loc: Loc, now: (f64, f64)) -> Option<bool> {
        let v = self.peek_exact(&[loc])[0].filter(|v| *v > 0.0)?;
        let low = now.0 <= 0.5;
        let test = match (low, loc.kind.whole()) {
            (true, _) => v * 2.0,
            (false, true) => (v / 2.0).floor(),
            (false, false) => v / 2.0,
        };
        if test <= 0.0 {
            return None;
        }
        self.helper.call(&format!("write {loc} {test} test"));
        std::thread::sleep(Duration::from_millis(1500));
        let shown = self.read_bar_stable().ok().flatten();
        let after = self.peek_exact(&[loc])[0];
        let kept = after.filter(|a| (a - test).abs() < (a - v).abs() && (a - test).abs() <= (v - test).abs());
        // Put back what the test took, keeping a change the game made meanwhile.
        match kept {
            Some(a) => self.helper.call(&format!("write {loc} {}", a + (v - test))),
            None => self.helper.call(&format!("write {loc} {v}")),
        };
        // The bar shows the value it holds then, the game's own change during the test included.
        let expect = now.0 * kept.unwrap_or(test) / v;
        let moved = shown.is_some_and(|g| (g.0 - expect).abs() <= 2.0 * now.1.max(g.1) + 0.02);
        self.say(&format!(
            "0x{:x}: wrote {test} ({} {v}) as a test, it held {}; the screen showed {} ({} expected)",
            loc.addr,
            if low { "twice" } else { "half of" },
            after.map_or("??".into(), |a| a.to_string()),
            shown.map_or("nothing steady".into(), |g| self.shown(g.0)),
            self.shown(expect)
        ));
        kept.map(|_| moved)
    }

    /// The same search driven by numbers the player types, for when the screen can't be read:
    /// the first number starts it, each one after narrows it down. With a watched area, the
    /// number also teaches Ferret how the game draws its digits.
    pub fn typed(&mut self, n: Shown) -> Result<AutoResult, String> {
        self.game()?;
        // A Stop pressed after the last search ended would end this one's wait for the game.
        self.cancel.store(false, Ordering::Relaxed);
        if self.area.is_some() {
            let frame = self.frame()?;
            self.learn(&frame, &n, false);
            let area = self.area;
            if let Some(f) = self.on_frame.as_mut() {
                f(&frame, area);
            }
        } else if self.capture.is_some() {
            // Only to show the game as it is now (without a session, a grab would open the
            // desktop's window picker).
            if let (Ok(frame), Some(f)) = (self.frame(), self.on_frame.as_mut()) {
                f(&frame, None);
            }
        }
        self.typed_search(n)
    }

    /// Narrows the search down by the number the game shows now, changed or not: the places
    /// that changed meanwhile are other things. `typed`: the last number the player typed,
    /// searched again when the box can't be read (they typed it because it couldn't).
    pub fn scan_again(&mut self, typed: Option<Shown>) -> Result<AutoResult, String> {
        self.game()?;
        self.cancel.store(false, Ordering::Relaxed);
        if self.search.is_none() {
            return Err(tr!("Nothing to narrow down yet: press Start or type the number the game shows."));
        }
        if self.area.is_none() {
            return Err(tr!("No number picked: type the number the game shows instead."));
        }
        if self.bar.is_some() {
            if self.search.is_some() {
                self.helper.call("mark bar");
            }
            let Some(f) = self.read_bar_stable()? else {
                return Err(match self.icons() {
                    true => tr!("The icons are all empty, hidden or changing: try again while they show and stay still."),
                    false => tr!("The bar is empty, hidden or moving: try again while it shows and stays still."),
                });
            };
            let missed = match self.icons() {
                true => tr!("Nothing in the game's memory changed like the icons did. Press Start Over and search again."),
                false => tr!("Nothing in the game's memory moved like the bar did. Press Start Over and search again."),
            };
            let count = self.bar_step(f).ok_or(missed)?;
            return self.bar_end(count, f);
        }
        match self.read_stable()? {
            Some(n) => {
                self.say(&format!("scanning again for {n}"));
                self.typed_search(n)
            }
            None => match typed {
                Some(n) => {
                    self.say(&format!("can't read the box: scanning again for the typed {n}"));
                    self.typed_search(n)
                }
                None => Err(tr!("Can't read the number now (is it on screen?). Type it below instead.")),
            },
        }
    }

    fn typed_search(&mut self, n: Shown) -> Result<AutoResult, String> {
        self.searched_decimals = n.decimals();
        let fresh = self.search.is_none();
        let (count, unchanged) = match self.search {
            Some((before, unchanged)) => {
                // `next` keeps values that went past the number since their last snapshot (the
                // screen lags memory). The player types what the game shows now: a snapshot
                // now, or a float that drifted 1 -> 3.87 since the last number stays for "2".
                self.helper.call("mark");
                let reply = self.helper.call(&format!("next {}", n.search()));
                self.say(&format!("typed {n}: {}", reply.join(" ")));
                let count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()))?;
                if count == 0 {
                    self.say("nothing went from the last number to this one, starting over");
                    self.lost_shape();
                    self.search = None;
                    return self.typed_search(n);
                }
                (count, if count == before { unchanged + 1 } else { 0 })
            }
            None => {
                let reply = self.scan(&n);
                self.say(&format!("typed {n}: {}", reply.join(" ")));
                let count = match_count(&reply).ok_or_else(|| first_error(&reply).unwrap_or("scan failed".into()));
                self.ready(*count.as_ref().unwrap_or(&0));
                (count?, 0)
            }
        };
        self.search = Some((count, unchanged));
        self.tell_matches(count);
        let found = match count {
            0 => {
                self.search = None;
                return Err(tr!("{n} is nowhere in the game's memory", n));
            }
            // It may be another item holding the same number: it has to follow the next one.
            1 if fresh && self.shaped => {
                self.say("one place shaped like earlier finds holds it: change the number once to be sure");
                None
            }
            1 => {
                let loc = self.candidates()[0].0;
                if let Some(why) = self.doubt(loc, &n) {
                    // Only places shaped like earlier finds were searched: the value is elsewhere.
                    if self.shaped {
                        self.lost_shape();
                        self.search = None;
                        return self.typed_search(n);
                    }
                    return Ok(AutoResult::Unsure(why));
                }
                Some(loc)
            }
            // The same few keep following the value: copies of it. Find the real one.
            2..=20 if unchanged >= 1 => {
                self.say("checking which one is the real value:");
                match self.probe(&n)? {
                    Probed::Found(loc) => Some(loc),
                    Probed::PutBack => return Ok(AutoResult::PutBack(count)),
                    Probed::Unclear => None,
                }
            }
            _ => None,
        };
        match found {
            Some(loc) => {
                self.search = None;
                self.say(&format!("stored as {}", loc.kind.with_article()));
                self.learn_from_memory(loc, &n);
                self.learn_shape(loc);
                Ok(AutoResult::Found(loc))
            }
            None => Ok(AutoResult::Several(count)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: Kind, decimals: u32) -> Entry {
        Entry {
            name: "gems".into(),
            kind,
            sites: Vec::new(),
            paths: Vec::new(),
            candidates: Vec::new(),
            run: None,
            named: Vec::new(),
            limit: None,
            decimals,
            last: None,
            build: None,
            untraced: None,
            imported: false,
            suggested: None,
            from: None,
            other: Vec::new(),
        }
    }

    #[test]
    fn same_numbers() {
        assert!(same_value(Kind::I32, 145156.0, 145156.0));
        assert!(!same_value(Kind::I32, 145157.0, 145156.0));
        assert!(same_value(Kind::F32, 127.00001, 127.0));
        assert!(!same_value(Kind::F64, 127.5, 127.0));
    }

    #[test]
    fn plausible_numbers() {
        assert!(plausible(Kind::F32, 36.333) && plausible(Kind::F32, 0.0) && plausible(Kind::F64, -0.25));
        assert!(!plausible(Kind::F32, 3.4e38) && !plausible(Kind::F32, 1.2e-40));
        assert!(plausible(Kind::I32, 4e9));
    }

    #[test]
    fn typed_numbers() {
        assert_eq!(parse_number("1.5"), Some(1.5));
        assert_eq!(parse_number(" -3 "), Some(-3.0));
        assert_eq!(parse_number("1,250"), Some(1250.0));
        assert_eq!(parse_number("1,250,000.5"), Some(1250000.5));
        assert_eq!(parse_number("1,5"), Some(1.5));
        assert_eq!(parse_number("1,2,3"), None);
        assert_eq!(parse_number("abc"), None);
    }

    #[test]
    fn shown_numbers() {
        assert_eq!(number_text(96.5, None), "96.5");
        assert_eq!(number_text(100.0, None), "100");
        assert_eq!(number_text(0.1f32 as f64, None), "0.1");
        assert_eq!(number_text(-0.0001, None), "0");
        assert_eq!(number_text(1.2, Some(1)), "1.2");
        assert_eq!(number_text(40.0, Some(0)), "40");
    }

    #[test]
    fn game_units() {
        let tenths = entry(Kind::I32, 1);
        assert_eq!(tenths.to_memory(1.5), Ok(15.0));
        assert!(tenths.to_memory(1.25).is_err());
        assert!(entry(Kind::Xor, 0).to_memory(1.5).is_err());
        assert_eq!(entry(Kind::F32, 0).to_memory(1.25), Ok(1.25));
        let mut limited = entry(Kind::I32, 1);
        limited.limit = Some(range_text(Some(15.0), None));
        assert_eq!(limited.limit.as_deref(), Some("15 -"));
        assert_eq!(limited.shown_range(), (Some(1.5), None));
        assert_eq!(range_text(Some(1.2000000000000002), Some(3.0)), "1.2 3");
    }
}
