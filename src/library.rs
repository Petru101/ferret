// The shared library of saved values (server/src/library.js): other players' uploads for the
// game that is open, downloading one (core::import takes it, unconfirmed), sharing the game's
// own (core::export's text), deleting one's own, and saying whether one worked. Every call
// blocks (online.rs): never on the GUI's thread.

use crate::online::{self, json_string};
use crate::i18n::tr;

/// One upload, as the list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pack {
    /// "K7Q2-9XMB".
    pub id: String,
    /// The day it was shared, "2026-10-07".
    pub day: String,
    /// Players it worked for, and for whom it didn't.
    pub worked: u32,
    pub failed: u32,
    /// The game build it was made in.
    pub build: Option<String>,
    /// Shared from this install: it can be deleted.
    pub mine: bool,
    /// "coins, health".
    pub names: String,
}

/// For a URL's query: everything but letters, digits and `-._~` as %XX.
fn query_value(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => (b as char).to_string(),
            b => format!("%{b:02X}"),
        })
        .collect()
}

/// The list's lines: id, day, worked, failed, build or -, 1 if mine, names (tab-separated).
fn parse_list(body: &str) -> Vec<Pack> {
    body.lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.splitn(7, '\t').collect();
            let [id, day, worked, failed, build, mine, names] = f[..] else { return None };
            Some(Pack {
                id: online::parse_id(id)?,
                day: day.to_owned(),
                worked: worked.parse().ok()?,
                failed: failed.parse().ok()?,
                build: (build != "-").then(|| build.to_owned()),
                mine: mine == "1",
                names: names.to_owned(),
            })
        })
        .collect()
}

/// Uploads for a game (its program, and its Steam id when known), best first.
pub fn list(game: &str, steam: Option<&str>) -> Result<Vec<Pack>, String> {
    let mut path = format!("/v1/packs?game={}", query_value(game));
    if let Some(s) = steam {
        path += &format!("&steam={}", query_value(s));
    }
    let reply = online::get(&path)?;
    match reply.status {
        200 => Ok(parse_list(&reply.body)),
        _ => Err(online::server_error(&reply)),
    }
}

/// An upload's text, for core::import.
pub fn download(id: &str) -> Result<String, String> {
    let reply = online::get(&format!("/v1/packs/{id}"))?;
    match reply.status {
        200 => Ok(reply.body),
        404 => Err(tr!("Upload {id} isn't there anymore: its player deleted it", id)),
        _ => Err(online::server_error(&reply)),
    }
}

/// Shares a `.ferret` text (core::export's); its id.
pub fn upload(text: &str) -> Result<String, String> {
    let body = format!("{{\"version\":{},\"body\":{}}}", json_string(&crate::core::version()), json_string(text));
    let reply = online::post("/v1/packs", &body)?;
    let id = || reply.body.split_once("\"id\":\"").and_then(|(_, r)| r.split_once('"')).map(|(id, _)| id.to_owned());
    match reply.status {
        200 => id().ok_or_else(|| online::server_error(&reply)),
        409 => Err(tr!("These exact values are shared already, as {id}", id = id().unwrap_or_default())),
        429 => Err(tr!("You shared a lot today: try again tomorrow.")),
        _ => Err(online::server_error(&reply)),
    }
}

/// Deletes one of this install's uploads.
pub fn delete(id: &str) -> Result<(), String> {
    let reply = online::delete(&format!("/v1/packs/{id}"))?;
    match reply.status {
        200 => Ok(()),
        _ => Err(online::server_error(&reply)),
    }
}

/// Tells the server whether a downloaded value worked: counted once per install and upload.
pub fn vote(id: &str, worked: bool) -> Result<(), String> {
    let reply = online::post(&format!("/v1/packs/{id}/vote"), &format!("{{\"worked\":{worked}}}"))?;
    match reply.status {
        200 => Ok(()),
        _ => Err(online::server_error(&reply)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_list() {
        let body = "4QFJ-DE58\t2026-10-07\t3\t1\t-\t0\tcoins, health\nbroken line\nQZZE-CE29\t2026-10-06\t0\t0\tab12\t1\tgold";
        let list = parse_list(body);
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].names, "coins, health");
        assert_eq!((list[0].worked, list[0].failed, list[0].mine), (3, 1, false));
        assert_eq!(list[1].build.as_deref(), Some("ab12"));
        assert!(list[1].mine);
    }

    #[test]
    fn escapes_query_values() {
        assert_eq!(query_value("Particle Fleet.exe"), "Particle%20Fleet.exe");
        assert_eq!(query_value("a&b=c"), "a%26b%3Dc");
    }
}
