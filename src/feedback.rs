// Feedback and bug reports: what the player writes, plus Ferret's log and the game's saved
// values when they leave them in, sent to Ferret's server (`server/`, a Cloudflare Worker)
// over HTTPS. The player sees exactly what goes before sending (`Report::preview`); the home
// folder's path is taken out of the log first.

use std::fs;

use crate::core;
use crate::online::{self, json_string};
use crate::i18n::tr;

/// The server takes at most 400 KB of log and 128 KB of saved values: the end of the log (the
/// newest lines) and the profile without its unconfirmed candidates fit with room to spare.
const LOG_MAX: usize = 300 * 1024;
const PROFILE_MAX: usize = 100 * 1024;

pub struct Report {
    /// A random id for this report (`online::new_id`), shown to the player once it is sent so they
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

/// The home folder's path out of a text: it holds the player's user name. On Windows that's
/// USERPROFILE (Windows sets no HOME, and the log went out with C:\Users\<name> in it).
fn scrub(text: &str) -> String {
    #[cfg(windows)]
    let (home, scrub_home) = (std::env::var("USERPROFILE"), scrub_windows as fn(&str, &str) -> String);
    #[cfg(not(windows))]
    let (home, scrub_home) = (std::env::var("HOME"), scrub_unix as fn(&str, &str) -> String);
    // Not a bare "/" or "C:\": that would take out every path.
    match home.ok().filter(|h| h.trim_end_matches(['/', '\\']).len() > 2) {
        Some(home) => scrub_home(text, &home),
        None => text.to_owned(),
    }
}

/// Every way a Linux home shows up: Bazzite's /home is a link to /var/home (the helper sees the
/// real path), and Wine writes it with backslashes ("Z:\var\home\...").
#[cfg_attr(windows, allow(dead_code))]
fn scrub_unix(text: &str, home: &str) -> String {
    let home = home.trim_end_matches('/');
    let real = home.strip_prefix("/var").unwrap_or(home);
    let mut text = text.to_owned();
    for path in [format!("/var{real}"), real.to_owned()] {
        text = text.replace(&path.replace('/', "\\"), "~").replace(&path, "~");
    }
    text
}

/// Every way Windows writes a profile folder (C:\Users\<name>): any case, with \, / or the \\ of
/// an escaped path ("C:\\games\\..." in the log). Only the whole name: C:\Users\Pat leaves
/// C:\Users\Patricia alone.
#[cfg_attr(not(windows), allow(dead_code))]
fn scrub_windows(text: &str, home: &str) -> String {
    let home = home.trim_end_matches(['\\', '/']);
    let mut text = text.to_owned();
    for sep in ["\\\\", "\\", "/"] {
        text = replace_path(&text, &home.replace('\\', sep), "~");
    }
    text
}

/// `text` with each `path` in it (ASCII letters in any case, not followed by more of a name)
/// replaced by `with`.
fn replace_path(text: &str, path: &str, with: &str) -> String {
    // ASCII lowercasing keeps every byte where it was: indices in `low` are indices in `text`.
    let (low, path) = (text.to_ascii_lowercase(), path.to_ascii_lowercase());
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    while let Some(i) = low[at..].find(&path) {
        let end = at + i + path.len();
        let more_name = low.as_bytes().get(end).is_some_and(|&b| b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80);
        out.push_str(&text[at..at + i]);
        out.push_str(if more_name { &text[at + i..end] } else { with });
        at = end;
    }
    out.push_str(&text[at..]);
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
            return Err(tr!("Write something first"));
        }
        let reply = online::post("/v1/feedback", &self.body(&online::install_id()))?;
        match reply.status {
            200 => Ok(()),
            429 => Err(tr!("Too many reports from here in the last hour: try again later.")),
            413 => Err(tr!("This report is too big to send: leave out the log and try again.")),
            409 => Err(tr!("A report with this id was sent already: close this window and write it again.")),
            _ => Err(online::server_error(&reply)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_the_home_folder_out() {
        let text = "profile /home/pat/.var/app/x and /var/home/pat/Games/y, Z:\\var\\home\\pat\\Steam";
        assert_eq!(scrub_unix(text, "/home/pat"), "profile ~/.var/app/x and ~/Games/y, Z:~\\Steam");
        assert_eq!(scrub_unix(text, "/var/home/pat/"), "profile ~/.var/app/x and ~/Games/y, Z:~\\Steam");
    }

    #[test]
    fn takes_the_windows_profile_out() {
        let text = "attached to 7: C:\\Users\\Pat\\Games\\x.exe, saved to c:\\users\\pat\\AppData\\Local\\Ferret, \
                    C:\\\\Users\\\\Pat\\\\z, C:/Users/Pat/w, C:\\Users\\Patricia\\v, C:\\Users\\Pat";
        assert_eq!(
            scrub_windows(text, "C:\\Users\\Pat\\"),
            "attached to 7: ~\\Games\\x.exe, saved to ~\\AppData\\Local\\Ferret, ~\\\\z, ~/w, C:\\Users\\Patricia\\v, ~"
        );
    }

    #[test]
    fn leaves_out_what_the_player_left_empty() {
        let r = Report { id: online::new_id(), message: " hi ".into(), contact: " ".into(), game: None, log: None, profile: None };
        let body = r.body("0123456789abcdef0123456789abcdef");
        assert!(body.contains("\"message\":\"hi\""));
        assert!(body.starts_with(&format!("{{\"id\":\"{}\"", r.id)));
        assert!(!body.contains("contact") && !body.contains("log"));
    }
}
