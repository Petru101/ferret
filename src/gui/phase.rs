// A big card in the middle of the window saying what Ferret is busy with and whether the
// player has to do something: scanning (don't change the number yet), ready (change it now),
// checking places (nothing to do), your turn (change it in the game, with a countdown), and
// opening a game (finding its saved values). While Start follows the number it shrinks to a
// strip at the bottom. The player watches the game and is tired: a small spinner and a status
// line were taken for "done" more than once.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use super::find::grouped;

pub struct PhaseCard {
    pub root: gtk::Box,
    title: gtk::Label,
    bar: gtk::ProgressBar,
    hint: gtk::Label,
    /// Counts the card's changes: timers stop, and "Ready!" stays, only while nothing came after.
    shown: Rc<Cell<u32>>,
    /// The places matching, for the strip that follows "Ready!".
    count: Cell<usize>,
}

const LOOKS: [&str; 5] = ["busy", "ready", "turn", "watching", "done"];

fn places(n: usize) -> String {
    format!("{} {}", grouped(n), if n == 1 { "place matches" } else { "places match" })
}

impl PhaseCard {
    pub fn new() -> Rc<Self> {
        let title = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        let bar = gtk::ProgressBar::builder().show_text(true).width_request(360).build();
        let hint = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .halign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .margin_bottom(24)
            .css_classes(["phase-card"])
            .can_target(false)
            .visible(false)
            .build();
        root.append(&title);
        root.append(&bar);
        root.append(&hint);
        Rc::new(Self { root, title, bar, hint, shown: Rc::default(), count: Cell::new(0) })
    }

    /// Shows the card in one of `LOOKS`; `bar` = how far, when there's something to measure.
    fn show(&self, look: &str, title: &str, bar: Option<f64>, hint: &str) -> u32 {
        let shown = self.shown.get() + 1;
        self.shown.set(shown);
        for l in LOOKS.iter().chain(&["flash"]) {
            self.root.remove_css_class(l);
        }
        self.root.add_css_class(look);
        // The strip sits low, out of the way of the game picture and the buttons.
        let strip = look == "watching";
        self.root.set_valign(if strip { gtk::Align::End } else { gtk::Align::Center });
        self.title.set_css_classes(&[if strip { "title-4" } else { "title-1" }]);
        self.hint.set_css_classes(&[if strip { "heading" } else { "title-3" }]);
        self.title.set_label(title);
        self.bar.set_visible(bar.is_some());
        self.bar.set_show_text(look == "busy");
        self.bar.set_fraction(bar.unwrap_or(0.0).clamp(0.0, 1.0));
        self.hint.set_label(hint);
        self.root.set_visible(true);
        shown
    }

    pub fn hide(&self) {
        self.shown.set(self.shown.get() + 1);
        self.root.set_visible(false);
    }

    pub fn scanning(&self, n: &str, done: f64) {
        self.show("busy", &format!("Scanning the game's memory for {n}…"), Some(done), "Don't change the number in the game yet.");
    }

    /// Start can follow the number: "Ready!" for a few seconds, then the strip.
    pub fn ready(self: &Rc<Self>, n: usize) {
        self.count.set(n);
        let shown = self.show("ready", "Ready!", None, &format!("{}. Change the number in the game now.", places(n)));
        let card = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if card.shown.get() == shown {
                card.watching(card.count.get());
            }
        });
    }

    pub fn watching(&self, n: usize) {
        self.count.set(n);
        // "Ready!" stays up its few seconds; the strip that follows shows the new count.
        if self.root.has_css_class("ready") && self.root.is_visible() {
            return;
        }
        self.show("watching", "Searching: keep changing the number in the game", None, &places(n));
    }

    pub fn checking(&self, places: usize, done: f64) {
        let title = match places {
            1 => "Checking the place found…".to_owned(),
            n => format!("Checking which of the {n} places is the value…"),
        };
        self.show("busy", &title, (done > 0.0).then_some(done), "A few seconds. You don't need to do anything.");
    }

    /// The player has to change the number in the game: blinks, and counts down.
    pub fn your_turn(self: &Rc<Self>, places: usize, secs: u64) {
        let hint = move |left: u64| {
            format!(
                "Pick some up or use some. Ferret watches which of the {places} places the game carries on from. {}:{:02} left.",
                left / 60,
                left % 60
            )
        };
        let shown = self.show("turn", "Your turn: change the number in the game!", Some(1.0), &hint(secs));
        let (card, start) = (self.clone(), Instant::now());
        glib::timeout_add_local(Duration::from_millis(600), move || {
            if card.shown.get() != shown {
                return glib::ControlFlow::Break;
            }
            let left = secs.saturating_sub(start.elapsed().as_secs());
            card.bar.set_fraction(left as f64 / secs as f64);
            card.hint.set_label(&hint(left));
            if card.root.has_css_class("flash") {
                card.root.remove_css_class("flash");
            } else {
                card.root.add_css_class("flash");
            }
            glib::ControlFlow::Continue
        });
    }

    /// A typed number narrowed the search: the player changes the number in the game next.
    /// Blinks for a few seconds.
    pub fn change_now(self: &Rc<Self>, hint: &str) {
        let shown = self.show("turn", "Now change the number in the game", None, hint);
        let (card, start) = (self.clone(), Instant::now());
        glib::timeout_add_local(Duration::from_millis(600), move || {
            if card.shown.get() != shown {
                return glib::ControlFlow::Break;
            }
            if start.elapsed() >= Duration::from_secs(6) {
                card.hide();
                return glib::ControlFlow::Break;
            }
            if card.root.has_css_class("flash") {
                card.root.remove_css_class("flash");
            } else {
                card.root.add_css_class("flash");
            }
            glib::ControlFlow::Continue
        });
    }

    /// How a search, or a step of it, ended: for a few seconds (the status line keeps it).
    pub fn done(self: &Rc<Self>, found: bool, title: &str, hint: &str) {
        let shown = self.show(if found { "ready" } else { "done" }, title, None, hint);
        let card = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(5), move || {
            if card.shown.get() == shown {
                card.hide();
            }
        });
    }

    pub fn restoring(&self, game: &str, name: &str, i: usize, n: usize) {
        self.show(
            "busy",
            &format!("Opening {game}…"),
            Some(i as f64 / n as f64),
            &format!("Finding your saved values: {name} ({} of {n})", i + 1),
        );
    }
}
