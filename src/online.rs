// Talking to Ferret's server (`server/`, a Cloudflare Worker): feedback (feedback.rs) and the
// shared library of saved values (library.rs). HTTPS with ureq + rustls and bundled root
// certificates; every call blocks for up to 30 s, so never on the GUI's thread.

use std::fs;
use std::time::Duration;

use crate::i18n::tr;

/// Where requests go; `FERRET_SERVER` points a test build at a local `wrangler dev`.
const SERVER: &str = "https://ferret.petru101.workers.dev";

/// A reply: its HTTP status and body.
pub struct Reply {
    pub status: u16,
    pub body: String,
}

fn url(path: &str) -> String {
    format!("{}{path}", std::env::var("FERRET_SERVER").unwrap_or(SERVER.into()))
}

fn unreachable(e: ureq::Error) -> String {
    tr!("Could not reach Ferret's server ({e}). Check your connection and try again.", e)
}

fn reply(mut response: ureq::http::Response<ureq::Body>) -> Result<Reply, String> {
    let status = response.status().as_u16();
    let body = response.body_mut().read_to_string().map_err(unreachable)?;
    Ok(Reply { status, body })
}

/// Requests carry the install id in a header: the server limits floods by it, and it is the
/// only proof an upload is the player's own (deleting it).
pub fn get(path: &str) -> Result<Reply, String> {
    let response = ureq::get(&url(path))
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .header("x-ferret-install", install_id())
        .call()
        .map_err(unreachable)?;
    reply(response)
}

pub fn post(path: &str, json: &str) -> Result<Reply, String> {
    let response = ureq::post(&url(path))
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .header("content-type", "application/json")
        .header("x-ferret-install", install_id())
        .send(json)
        .map_err(unreachable)?;
    reply(response)
}

pub fn delete(path: &str) -> Result<Reply, String> {
    let response = ureq::delete(&url(path))
        .config()
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(30)))
        .build()
        .header("x-ferret-install", install_id())
        .call()
        .map_err(unreachable)?;
    reply(response)
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

/// An id to show the player: "K7Q2-9XMB" (40 bits, so two never match in practice).
pub fn new_id() -> String {
    let chars: Vec<char> = random::<8>().iter().map(|b| ID_CHARS[(b % 32) as usize] as char).collect();
    format!("{}-{}", chars[..4].iter().collect::<String>(), chars[4..].iter().collect::<String>())
}

/// Whether a text has an id's shape (any case, the dash optional): "k7q29xmb" -> "K7Q2-9XMB".
pub fn parse_id(text: &str) -> Option<String> {
    let s: String = text.trim().chars().filter(|&c| c != '-').map(|c| c.to_ascii_uppercase()).collect();
    (s.len() == 8 && s.bytes().all(|b| ID_CHARS.contains(&b))).then(|| format!("{}-{}", &s[..4], &s[4..]))
}

/// A random id made once per install: the server's flood limits count by it, it tells one
/// player's reports and votes from another's, and it marks the player's own uploads. Nothing
/// else is tied to it.
pub fn install_id() -> String {
    let path = gtk::glib::user_config_dir().join("install-id");
    if let Some(id) = fs::read_to_string(&path).ok().map(|s| s.trim().to_owned()).filter(|s| s.len() == 32) {
        return id;
    }
    let id: String = random::<16>().iter().map(|b| format!("{b:02x}")).collect();
    fs::create_dir_all(path.parent().unwrap()).ok();
    fs::write(&path, &id).ok();
    id
}

pub fn json_string(s: &str) -> String {
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

/// What the server said went wrong, from its `{"error":"..."}` reply.
pub fn server_error(reply: &Reply) -> String {
    let why = reply
        .body
        .split_once("\"error\":\"")
        .and_then(|(_, rest)| rest.split_once('"'))
        .map_or(reply.body.trim(), |(e, _)| e);
    tr!("Ferret's server refused it ({why})", why = format!("{} {why}", reply.status))
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
        assert_eq!(parse_id(" k7q29xmb ").as_deref(), Some("K7Q2-9XMB"));
        assert_eq!(parse_id(&id).as_deref(), Some(id.as_str()));
        assert_eq!(parse_id("K7Q2-9XMI"), None, "I isn't used");
        assert_eq!(parse_id("K7Q2"), None);
    }

    #[test]
    fn escapes_json() {
        assert_eq!(json_string("a \"b\" \\ c\n\u{1}é"), "\"a \\\"b\\\" \\\\ c\\n\\u0001é\"");
    }

    #[test]
    fn reads_the_servers_reason() {
        let r = Reply { status: 409, body: "{\"error\":\"already shared\"}".into() };
        assert_eq!(server_error(&r), "Ferret's server refused it (409 already shared)");
    }
}
