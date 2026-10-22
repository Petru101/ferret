// A test mode that uses the window like a careless player: FERRET_MONKEY=<seed> presses random
// buttons, flips toggles and switches, picks in drop-downs and types junk into entries, through
// GTK itself (never the real mouse or keyboard), every few hundred milliseconds. It logs each
// action, panics, and stalls of the window's thread to monkey.log in the cache folder, then quits.
// FERRET_MONKEY_STEPS sets how many actions (300). It needs FERRET_MONKEY_GAME=<pid>: the only
// game Ferret may open meanwhile (`Core::attach`), so a player's own games are never written
// to. It never presses what quits, sends anything, or opens a file dialog, and notifications
// only go to the log; point FERRET_SERVER at a dead port to keep the rest offline too.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

/// Whether the test mode runs (notifications then go only to the log).
pub fn on() -> bool {
    std::env::var_os("FERRET_MONKEY").is_some()
}

pub fn log(line: &str) {
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::core::cache_dir().join("monkey.log")) {
        let _ = writeln!(f, "{line}");
    }
}

/// Words on, or actions behind, what it must not press: quitting, anything sent to Ferret's
/// server, file dialogs, links.
const SKIP: [&str; 13] =
    ["quit", "send", "feedback", "share", "upload", "report", "export", "import", "delete", "take", "restart", "website", "github"];

/// Text typed into entries.
const JUNK: [&str; 12] = ["1000", "0", "-5", "1.5", "3:17", "abc", "", "99999999999999999999", "%s%n", "ünï", "1e308", "  42  "];

struct Rng(u64);

impl Rng {
    fn below(&mut self, n: usize) -> usize {
        // xorshift64
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 % n.max(1) as u64) as usize
    }
}

pub fn start() {
    let seed: u64 = std::env::var("FERRET_MONKEY").ok().and_then(|s| s.parse().ok()).unwrap_or(1);
    let steps: usize = std::env::var("FERRET_MONKEY_STEPS").ok().and_then(|s| s.parse().ok()).unwrap_or(300);
    if std::env::var("FERRET_MONKEY_GAME").ok().and_then(|p| p.parse::<u32>().ok()).is_none() {
        log("not started: FERRET_MONKEY_GAME=<pid> of the test game is needed");
        return;
    }
    log(&format!("seed {seed}, {steps} steps"));
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        log(&format!("PANIC: {info}"));
        previous(info);
    }));
    // The window's thread beats; a watcher says when it stops for a while (a stuck window).
    let start = Instant::now();
    let beat = Arc::new(AtomicU64::new(0));
    let done = Arc::new(AtomicBool::new(false));
    {
        let (beat, done) = (beat.clone(), done.clone());
        std::thread::spawn(move || {
            let mut stuck_since = None;
            while !done.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(250));
                let now = start.elapsed().as_millis() as u64;
                let last = beat.load(Ordering::Relaxed);
                match (now.saturating_sub(last) > 2000, stuck_since) {
                    (true, None) => {
                        stuck_since = Some(last);
                        log(&format!("STALL: the window's thread hasn't run for 2 s (since {:.1} s)", last as f64 / 1000.0));
                    }
                    (false, Some(since)) => {
                        log(&format!("STALL over: {:.1} s", (now - since) as f64 / 1000.0));
                        stuck_since = None;
                    }
                    _ => {}
                }
            }
        });
    }
    {
        let beat = beat.clone();
        glib::timeout_add_local(Duration::from_millis(100), move || {
            beat.store(start.elapsed().as_millis() as u64, Ordering::Relaxed);
            glib::ControlFlow::Continue
        });
    }
    let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let mut step = 0;
    // A moment for the game list first.
    glib::timeout_add_local(Duration::from_millis(350), move || {
        if start.elapsed() < Duration::from_secs(2) {
            return glib::ControlFlow::Continue;
        }
        step += 1;
        if step > steps {
            log(&format!("done: {steps} steps in {:.0} s", start.elapsed().as_secs_f64()));
            done.store(true, Ordering::Relaxed);
            std::process::exit(0);
        }
        let mut found = Vec::new();
        for w in gtk::Window::list_toplevels() {
            walk(&w, &mut found);
        }
        if found.is_empty() {
            log(&format!("#{step} nothing to press"));
            return glib::ControlFlow::Continue;
        }
        let w = found.swap_remove(rng.below(found.len()));
        let what = act(&w, &mut rng);
        log(&format!("{:6.1}s #{step} {what} {} '{}'", start.elapsed().as_secs_f64(), w.type_().name(), short(&words(&w))));
        glib::ControlFlow::Continue
    });
}

/// What can be pressed in `w` and below it, shown and not greyed out.
fn walk(w: &gtk::Widget, found: &mut Vec<gtk::Widget>) {
    if !w.is_visible() || w.is::<gtk::WindowControls>() || w.is::<gtk::LinkButton>() {
        return;
    }
    let pressable = w.is::<gtk::Button>()
        || w.is::<gtk::CheckButton>()
        || w.is::<gtk::Switch>()
        || w.is::<gtk::MenuButton>()
        || w.is::<gtk::SpinButton>()
        || w.is::<gtk::DropDown>()
        || w.is::<gtk::Entry>()
        || w.is::<gtk::SearchEntry>()
        || w.is::<adw::EntryRow>()
        || w.is::<adw::ComboRow>()
        // The game list's rows, and other rows that open something.
        || w.downcast_ref::<gtk::ListBoxRow>().is_some_and(|r| r.is_activatable());
    if pressable && w.is_mapped() && w.is_sensitive() && !skipped(w) {
        found.push(w.clone());
    }
    let mut child = w.first_child();
    while let Some(c) = child {
        walk(&c, found);
        child = c.next_sibling();
    }
}

fn skipped(w: &gtk::Widget) -> bool {
    let text = words(w).to_lowercase();
    let action = w.dynamic_cast_ref::<gtk::Actionable>().and_then(|a| a.action_name()).map(|a| a.to_lowercase()).unwrap_or_default();
    SKIP.iter().any(|s| text.contains(s) || action.contains(s))
}

/// The label, tooltip, icon and the texts inside a widget, to tell what it is.
fn words(w: &gtk::Widget) -> String {
    let mut parts = Vec::new();
    if let Some(b) = w.downcast_ref::<gtk::Button>() {
        parts.extend(b.label().map(|l| l.to_string()));
        parts.extend(b.icon_name().map(|l| l.to_string()));
    }
    if let Some(r) = w.downcast_ref::<adw::PreferencesRow>() {
        parts.push(r.title().to_string());
    }
    parts.extend(w.tooltip_text().map(|t| t.to_string()));
    fn labels(w: &gtk::Widget, parts: &mut Vec<String>) {
        let mut child = w.first_child();
        while let Some(c) = child {
            if let Some(l) = c.downcast_ref::<gtk::Label>() {
                parts.push(l.label().to_string());
            }
            labels(&c, parts);
            child = c.next_sibling();
        }
    }
    if !w.is::<gtk::Editable>() {
        labels(w, &mut parts);
    }
    parts.retain(|p| !p.is_empty());
    parts.join(" | ")
}

fn short(s: &str) -> String {
    let s = s.replace('\n', " ");
    match s.char_indices().nth(70) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s,
    }
}

/// Does something with `w`; what, for the log.
fn act(w: &gtk::Widget, rng: &mut Rng) -> String {
    if let Some(t) = w.downcast_ref::<gtk::ToggleButton>() {
        t.set_active(!t.is_active());
        return format!("toggled to {}", t.is_active());
    }
    if let Some(c) = w.downcast_ref::<gtk::CheckButton>() {
        c.set_active(!c.is_active());
        return format!("checked {}", c.is_active());
    }
    if let Some(s) = w.downcast_ref::<gtk::Switch>() {
        s.set_active(!s.is_active());
        return format!("switched {}", s.is_active());
    }
    if let Some(m) = w.downcast_ref::<gtk::MenuButton>() {
        m.popup();
        return "opened".into();
    }
    if let Some(s) = w.downcast_ref::<gtk::SpinButton>() {
        let a = s.adjustment();
        let v = match rng.below(4) {
            0 => a.lower(),
            1 => a.upper(),
            _ => a.lower() + (a.upper() - a.lower()) * rng.below(1000) as f64 / 1000.0,
        };
        s.set_value(v);
        return format!("set to {v}");
    }
    if let Some(d) = w.downcast_ref::<gtk::DropDown>() {
        let n = d.model().map_or(0, |m| m.n_items());
        if n > 0 {
            d.set_selected(rng.below(n as usize) as u32);
        }
        return format!("picked {}", d.selected());
    }
    if w.is::<gtk::ListBoxRow>() && !w.is::<adw::ComboRow>() {
        w.activate();
        return "activated row".into();
    }
    if let Some(c) = w.downcast_ref::<adw::ComboRow>() {
        let n = c.model().map_or(0, |m| m.n_items());
        if n > 0 {
            c.set_selected(rng.below(n as usize) as u32);
        }
        return format!("picked {}", c.selected());
    }
    if let Some(e) = w.dynamic_cast_ref::<gtk::Editable>() {
        let text = if rng.below(10) == 0 { "9".repeat(2000) } else { JUNK[rng.below(JUNK.len())].to_owned() };
        e.set_text(&text);
        if let Some(entry) = w.downcast_ref::<gtk::Entry>() {
            entry.emit_activate();
        } else if w.is::<adw::EntryRow>() {
            w.emit_by_name::<()>("entry-activated", &[]);
        } else {
            w.activate();
        }
        return format!("typed '{}' and pressed Enter", short(&text));
    }
    if let Some(b) = w.downcast_ref::<gtk::Button>() {
        b.emit_clicked();
        return "clicked".into();
    }
    "nothing for".into()
}
