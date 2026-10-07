// Translations: gettext through GLib, in the "ferret" domain (po/). Only the window turns them
// on (`init`): the CLI and the log stay English, so test scripts and bug reports read the same
// everywhere.
//
// Texts with values use named placeholders, `tr!("{name} closed", name)` or
// `tr!("Opening {game}…", game = g.exe)`, so translators can move them around; xgettext reads
// the macros (po/update.sh) and checks the placeholders as rust-format.

use std::ffi::{c_char, CString};
use std::fmt::Display;
use std::sync::atomic::{AtomicBool, Ordering};

use gtk::glib;

pub const DOMAIN: &str = "ferret";
const LOCALE_DIR: &str = "/app/share/locale";

static ON: AtomicBool = AtomicBool::new(false);

extern "C" {
    fn bindtextdomain(domain: *const c_char, dir: *const c_char) -> *mut c_char;
    fn bind_textdomain_codeset(domain: *const c_char, codeset: *const c_char) -> *mut c_char;
}

/// The player's language from the environment, and Ferret's texts in it.
pub fn init() {
    let (domain, dir, utf8) = (CString::new(DOMAIN).unwrap(), CString::new(LOCALE_DIR).unwrap(), c"UTF-8");
    unsafe {
        libc::setlocale(libc::LC_ALL, c"".as_ptr());
        bindtextdomain(domain.as_ptr(), dir.as_ptr());
        bind_textdomain_codeset(domain.as_ptr(), utf8.as_ptr());
    }
    ON.store(true, Ordering::Relaxed);
}

pub fn gettext(msgid: &str) -> String {
    match ON.load(Ordering::Relaxed) {
        true => glib::dgettext(Some(DOMAIN), msgid).into(),
        false => msgid.to_owned(),
    }
}

pub fn ngettext(singular: &str, plural: &str, n: u64) -> String {
    match ON.load(Ordering::Relaxed) {
        true => glib::dngettext(Some(DOMAIN), singular, plural, n as _).into(),
        false if n == 1 => singular.to_owned(),
        false => plural.to_owned(),
    }
}

/// Puts the values in for their `{name}`s, in one pass (a value holding braces stays as it is).
pub fn fill(text: String, args: &[(&str, &dyn Display)]) -> String {
    if args.is_empty() {
        return text;
    }
    let mut out = String::with_capacity(text.len() + 16);
    let mut rest = text.as_str();
    while let Some(open) = rest.find('{') {
        out.push_str(&rest[..open]);
        let after = &rest[open + 1..];
        // The last of a name wins: `ntr!` puts the count first, and a value given for `n` after it.
        match after.find('}').and_then(|close| Some((close, args.iter().rev().find(|(k, _)| *k == &after[..close])?))) {
            Some((close, (_, value))) => {
                out.push_str(&value.to_string());
                rest = &after[close + 1..];
            }
            None => {
                out.push('{');
                rest = after;
            }
        }
    }
    out.push_str(rest);
    out
}

/// A translated text: `tr!("Saved")`, `tr!("{name} closed", name)`, `tr!("Opening {game}…", game = g.exe)`.
macro_rules! tr {
    ($msgid:literal $(, $name:ident $(= $value:expr)?)* $(,)?) => {
        $crate::i18n::fill(
            $crate::i18n::gettext($msgid),
            &[$((stringify!($name), &$crate::i18n::tr_arg!($name $(= $value)?) as &dyn ::std::fmt::Display)),*],
        )
    };
}

/// A translated text with a count: `ntr!("{n} place matches", "{n} places match", n)`; the count
/// is filled in as `n` unless given another value.
macro_rules! ntr {
    ($singular:literal, $plural:literal, $n:expr $(, $name:ident $(= $value:expr)?)* $(,)?) => {{
        let n = $n;
        $crate::i18n::fill(
            $crate::i18n::ngettext($singular, $plural, n as u64),
            &[("n", &n as &dyn ::std::fmt::Display) $(, (stringify!($name), &$crate::i18n::tr_arg!($name $(= $value)?) as &dyn ::std::fmt::Display))*],
        )
    }};
}

/// A text translated where it's used (a table of labels): `n_!("All Types")`, shown through
/// `gettext`. Marks it for xgettext.
macro_rules! n_ {
    ($msgid:literal) => {
        $msgid
    };
}

macro_rules! tr_arg {
    ($name:ident) => {
        $name
    };
    ($name:ident = $value:expr) => {
        $value
    };
}

pub(crate) use {n_, ntr, tr, tr_arg};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fills_names() {
        let (game, n) = ("Moonlighter", 3);
        assert_eq!(tr!("{game} has {n} values", game, n), "Moonlighter has 3 values");
        assert_eq!(tr!("{a} and {b}", a = 1, b = "two"), "1 and two");
        assert_eq!(tr!("left {unknown} and {{"), "left {unknown} and {{");
        assert_eq!(tr!("{v}", v = "{v}"), "{v}");
        assert_eq!(ntr!("{n} place", "{n} places", 1usize), "1 place");
        assert_eq!(ntr!("{n} place in {game}", "{n} places in {game}", 2, game), "2 places in Moonlighter");
        assert_eq!(ntr!("{n} place", "{n} places", 1200, n = "1,200"), "1,200 places");
    }
}
