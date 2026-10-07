// Feedback and bug reports: what the player writes, plus Ferret's log and the game's saved
// values when they leave them in, sent to Ferret's server (`server/`, a Cloudflare Worker)
// over HTTPS. The player sees exactly what goes before sending (`Report::preview`); the home
// folder's path is taken out of the log first.

use std::fs;
use std::time::Duration;

use crate::core;

/// Where reports go; `FERRET_SERVER` points a test build at a local `wrangler dev`.
const SERVER: &str = "https://ferret.petru101.workers.dev";

/// The server takes at most 400 KB of log and 128 KB of saved values: the end of the log (the
/// newest lines) and the profile without its unconfirmed candidates fit with room to spare.
const LOG_MAX: usize = 300 * 1024;
const PROFILE_MAX: usize = 100 * 1024;

pub struct Report {
    /// A random id for this report (`new_id`), shown to the player once it is sent so they
    /// can point to it; the server refuses a second report with the same one.
    pub id: String,
    pub message: String,
    /// How the player would like an answer, if at all.
    pub contact: String,
    /// The attached game's program.
    pub game: Option<String>,
    pub log: Option<String>,
    pub profile: Option<String>,
}

/// Ferret's log as it would be sent: the newest part, without the home folder's path.
pub fn log_text() -> Option<String> {
    let text = fs::read_to_string(core::cache_dir().join("ferret.log")).ok()?;
    let text = scrub(&text);
    let start = text.len().saturating_sub(LOG_MAX);
    // From the first whole line in the part kept.
    let start = match start {
        0 => 0,
        s => text.bytes().skip(s).position(|b| b == b'\n').map_or(text.len(), |i| s + i + 1),
    };
    Some(text[start..].to_owned()).filter(|t| !t.is_empty())
}

/// The game's saved values as they would be sent: the profile without its unconfirmed pointer
/// paths (thousands of lines, nothing to read in them), with how many there were.
pub fn profile_text(exe: &str) -> Option<String> {
    let text = core::saved_text(exe)?;
    let candidates = text.lines().filter(|l| l.starts_with("candidate ")).count();
    let mut kept: String = text.lines().filter(|l| !l.starts_with("candidate ")).map(|l| format!("{l}\n")).collect();
    if candidates > 0 {
        kept += &format!("({candidates} candidate lines left out)\n");
    }
    if kept.len() > PROFILE_MAX {
        let end = (0..=PROFILE_MAX).rev().find(|&i| kept.is_char_boundary(i)).unwrap_or(0);
        kept.truncate(end);
    }
    Some(scrub(&kept)).filter(|t| !t.trim().is_empty())
}

/// The home folder's path out of a text: it holds the player's user name. Every way it shows
/// up: Bazzite's /home is a link to /var/home (the helper sees the real path), and Wine
/// writes it with backslashes ("Z:\\var\\home\\...").
fn scrub(text: &str) -> String {
    let Some(home) = std::env::var("HOME").ok().filter(|h| h.len() > 1) else { return text.to_owned() };
    let home = home.trim_end_matches('/');
    let real = home.strip_prefix("/var").unwrap_or(home);
    let mut text = text.to_owned();
    for path in [format!("/var{real}"), real.to_owned()] {
        text = text.replace(&path.replace('/', "\\"), "~").replace(&path, "~");
    }
    text
}

/// Letters and digits that can't be mistaken for each other when read out or typed (no I, L,
/// O, U: Crockford's base 32).
const ID_CHARS: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

/// Random bytes from the kernel.
fn random<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if let Ok(mut f) = fs::File::open("/dev/urandom") {
        std::io::Read::read_exact(&mut f, &mut bytes).ok();
    }
    bytes
}

/// A report's id: "K7Q2-9XMB" (40 bits, so two reports never share one in practice). Row
/// numbers were reused after a delete and told how many reports there are.
pub fn new_id() -> String {
    let chars: Vec<char> = random::<8>().iter().map(|b| ID_CHARS[(b % 32) as usize] as char).collect();
    format!("{}-{}", chars[..4].iter().collect::<String>(), chars[4..].iter().collect::<String>())
}

/// A random id made once per install: the server's flood limit counts reports per install,
/// and it tells one player's reports from another's. Nothing else is tied to it.
fn install_id() -> String {
    let path = gtk::glib::user_config_dir().join("install-id");
    if let Some(id) = fs::read_to_string(&path).ok().map(|s| s.trim().to_owned()).filter(|s| s.len() == 32) {
        return id;
    }
    let id: String = random::<16>().iter().map(|b| format!("{b:02x}")).collect();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(&path, &id).ok();
    id
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

impl Report {
    /// What the server gets.
    fn body(&self, install: &str) -> String {
        let field = |name: &str, v: Option<&str>| v.map(|v| format!(",{}:{}", json_string(name), json_string(v))).unwrap_or_default();
        format!(
            "{{\"id\":{},\"install\":{},\"version\":{},\"message\":{}{}{}{}{}}}",
            json_string(&self.id),
            json_string(install),
            json_string(&core::version()),
            json_string(self.message.trim()),
            field("contact", Some(self.contact.trim()).filter(|c| !c.is_empty())),
            field("game", self.game.as_deref()),
            field("log", self.log.as_deref()),
            field("profile", self.profile.as_deref()),
        )
    }

    /// Everything that is sent, as the player reads it.
    pub fn preview(&self) -> String {
        let mut text = format!("Report {}\nFerret {}\n", self.id, core::version());
        if let Some(g) = &self.game {
            text += &format!("Game: {g}\n");
        }
        if !self.contact.trim().is_empty() {
            text += &format!("Contact: {}\n", self.contact.trim());
        }
        text += &format!("\n{}\n", self.message.trim());
        if let Some(p) = &self.profile {
            text += &format!("\n--- Saved values\n{p}");
        }
        if let Some(l) = &self.log {
            text += &format!("\n--- Ferret's log\n{l}");
        }
        text += "\n--- Also sent: a random id Ferret made for this install (it limits floods of reports).";
        text
    }

    /// Sends it; blocks for up to 30 s (not on the GUI's thread). The player is shown its id.
    pub fn send(&self) -> Result<(), String> {
        if self.message.trim().is_empty() {
            return Err("Write something first".into());
        }
        let url = format!("{}/v1/feedback", std::env::var("FERRET_SERVER").unwrap_or(SERVER.into()));
        let mut response = ureq::post(&url)
            .config()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(30)))
            .build()
            .header("content-type", "application/json")
            .send(&self.body(&install_id()))
            .map_err(|e| format!("Could not reach Ferret's server ({e}). Check your connection and try again."))?;
        match response.status().as_u16() {
            200 => Ok(()),
            429 => Err("Too many reports from here in the last hour: try again later.".into()),
            413 => Err("This report is too big to send: leave out the log and try again.".into()),
            409 => Err("A report with this id was sent already: close this window and write it again.".into()),
            s => {
                let why = response.body_mut().read_to_string().unwrap_or_default();
                Err(format!("Ferret's server refused it ({s} {why})"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn makes_readable_ids() {
        let id = new_id();
        assert_eq!(id.len(), 9, "{id}");
        assert_eq!(&id[4..5], "-");
        assert!(id.chars().filter(|&c| c != '-').all(|c| ID_CHARS.contains(&(c as u8))));
        assert_ne!(new_id(), new_id());
    }

    #[test]
    fn escapes_json() {
        assert_eq!(json_string("a \"b\" \\ c\n\u{1}é"), "\"a \\\"b\\\" \\\\ c\\n\\u0001é\"");
    }

    #[test]
    fn takes_the_home_folder_out() {
        let home = std::env::var("HOME").unwrap();
        let real = home.strip_prefix("/var").unwrap_or(&home).to_owned();
        let wine = format!("Z:/var{real}/Steam").replace('/', "\\");
        let text = format!("profile {real}/.var/app/x and /var{real}/Games/y, {wine}");
        assert_eq!(scrub(&text), "profile ~/.var/app/x and ~/Games/y, Z:~\\Steam");
    }

    #[test]
    fn leaves_out_what_the_player_left_empty() {
        let r = Report { id: new_id(), message: " hi ".into(), contact: " ".into(), game: None, log: None, profile: None };
        let body = r.body("0123456789abcdef0123456789abcdef");
        assert!(body.contains("\"message\":\"hi\""));
        assert!(body.starts_with(&format!("{{\"id\":\"{}\"", r.id)));
        assert!(!body.contains("contact") && !body.contains("log"));
    }
}
