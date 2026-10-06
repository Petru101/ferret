// A big card in the middle of the window saying what Ferret is busy with and whether the
// player has to do something: scanning (don't change the number yet), ready (change it now),
// checking places (nothing to do), your turn (change it in the game: a countdown while a
// search waits, none while a save waits for the code that changes it), and
// opening a game (finding its saved values). While Start follows the number it shrinks to a
// strip at the bottom. The player watches the game and is tired: a small spinner and a status
// line were taken for "done" more than once. What the player has to know while the game is in
// front also goes out as a notification (`notify.rs`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use super::find::grouped;
use super::notify::Notifier;
use crate::bar::Kind as GaugeKind;

pub struct PhaseCard {
    pub root: gtk::Box,
    title: gtk::Label,
    bar: gtk::ProgressBar,
    hint: gtk::Label,
    /// On "Your turn" only: the wait can take minutes, and a game that pauses while it isn't
    /// in front (Prey, during a cutscene) never changes the number. The toolbar's Stop sat
    /// behind the blinking card and went unseen.
    pub give_up: gtk::Button,
    /// Counts the card's changes: timers stop, and "Ready!" stays, only while nothing came after.
    shown: Rc<Cell<u32>>,
    /// The places matching, for the strip that follows "Ready!".
    count: Cell<usize>,
    notify: Rc<Notifier>,
    /// What the card asks of the player now (title, text, button, seconds): sent again when they
    /// switch to the game (`remind`); "Ready" usually comes while Ferret is still in front.
    away: RefCell<Option<(String, String, Option<&'static str>, Option<u32>)>>,
    /// `away` went out already: once is enough (the player switching back and forth got the
    /// same one every time, until Plasma refused them as too many).
    reminded: Cell<bool>,
    /// The search follows a bar or a row of icons, not a number: the texts say so.
    pub follows: Cell<Option<GaugeKind>>,
}

/// How high the strip sits above the window's bottom: over the Find tab's log (110 px and its
/// margins).
const STRIP_ABOVE: i32 = 150;

const LOOKS: [&str; 5] = ["busy", "ready", "turn", "watching", "done"];

fn places(n: usize) -> String {
    format!("{} {}", grouped(n), if n == 1 { "place matches" } else { "places match" })
}

impl PhaseCard {
    pub fn new(notify: Rc<Notifier>) -> Rc<Self> {
        let title = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        let bar = gtk::ProgressBar::builder().show_text(true).width_request(360).build();
        let hint = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).build();
        let give_up = gtk::Button::builder()
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .visible(false)
            .build();
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
        root.append(&give_up);
        Rc::new(Self { root, title, bar, hint, give_up, shown: Rc::default(), count: Cell::new(0), notify, away: RefCell::default(), reminded: Cell::new(false), follows: Cell::new(None) })
    }

    /// Shows the card in one of `LOOKS`; `bar` = how far, when there's something to measure.
    fn show(&self, look: &str, title: &str, bar: Option<f64>, hint: &str) -> u32 {
        let shown = self.shown.get() + 1;
        self.shown.set(shown);
        self.notify.withdraw_waiting();
        self.away.take();
        for l in LOOKS.iter().chain(&["flash"]) {
            self.root.remove_css_class(l);
        }
        self.root.add_css_class(look);
        // The strip sits low, out of the way of the game picture and the buttons, above the Find
        // tab's log (it covered the log's last lines).
        let strip = look == "watching";
        self.root.set_valign(if strip { gtk::Align::End } else { gtk::Align::Center });
        self.root.set_margin_bottom(if strip { STRIP_ABOVE } else { 24 });
        self.title.set_css_classes(&[if strip { "title-4" } else { "title-1" }]);
        self.hint.set_css_classes(&[if strip { "heading" } else { "title-3" }]);
        self.title.set_label(title);
        self.bar.set_visible(bar.is_some());
        self.bar.set_show_text(look == "busy");
        self.bar.set_fraction(bar.unwrap_or(0.0).clamp(0.0, 1.0));
        self.hint.set_label(hint);
        // Clicks go through the card to the window, except while it has a button.
        self.give_up.set_visible(false);
        self.root.set_can_target(false);
        self.root.set_visible(true);
        shown
    }

    pub fn hide(&self) {
        self.shown.set(self.shown.get() + 1);
        self.notify.withdraw_waiting();
        self.away.take();
        self.root.set_visible(false);
    }

    /// Notifies the player of what the card asks of them (`send`: now too, not only when they
    /// switch to the game).
    fn tell(&self, title: &str, text: &str, button: Option<&'static str>, secs: Option<u32>, send: bool) {
        if send {
            self.notify.send(title, text, button, secs);
        }
        self.reminded.set(send && self.notify.away());
        self.away.replace(Some((title.to_owned(), text.to_owned(), button, secs)));
    }

    /// Ferret's window lost the front: what the card asks of the player, as a notification.
    pub fn remind(&self) {
        if !self.root.is_visible() || self.reminded.replace(true) {
            return;
        }
        if let Some((title, text, button, secs)) = self.away.borrow().clone() {
            self.notify.send(&title, &text, button, secs);
        }
    }

    pub fn scanning(&self, n: &str, done: f64) {
        self.show("busy", &format!("Scanning the game's memory for {n}…"), Some(done), "Don't change the number in the game yet.");
    }

    /// A bar's search copies the game's memory first (no number to look for).
    pub fn copying(&self, done: f64) {
        let hint = match self.follows.get() {
            Some(GaugeKind::Icons) => "Don't let the icons change yet.",
            _ => "Don't let the bar change yet.",
        };
        self.show("busy", "Copying the game's memory…", Some(done), hint);
    }

    /// The copy is taken: the bar has to move before anything narrows down.
    pub fn bar_ready(self: &Rc<Self>) {
        let icons = self.follows.get() == Some(GaugeKind::Icons);
        let (hint, title, searching, waiting) = match icons {
            true => (
                "Now let the icons change in the game: take a hit, or use some.",
                "Ready: let the icons change in the game",
                "Searching: let the icons change in the game",
                "Waiting for the icons to change.",
            ),
            false => (
                "Now let the bar change in the game: take a hit, or use some.",
                "Ready: let the bar change in the game",
                "Searching: let the bar change in the game",
                "Waiting for the bar to move.",
            ),
        };
        let shown = self.show("ready", "Ready!", None, hint);
        self.tell(title, "Take a hit or use some. Every change narrows it down.", None, Some(6), true);
        let card = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if card.shown.get() == shown {
                card.show("watching", searching, None, waiting);
                card.tell(searching, waiting, None, Some(6), false);
            }
        });
    }

    /// What Start follows changing: "Keep the bar changing" and the like, with the count.
    fn keep_changing(&self, n: usize) -> String {
        match self.follows.get() {
            Some(GaugeKind::Bar) => format!("{}. Keep the bar changing in the game.", places(n)),
            Some(GaugeKind::Icons) => format!("{}. Keep the icons changing in the game.", places(n)),
            None => format!("{}. Change the number in the game now.", places(n)),
        }
    }

    /// Start can follow the number: "Ready!" for a few seconds, then the strip.
    pub fn ready(self: &Rc<Self>, n: usize) {
        self.count.set(n);
        let shown = self.show("ready", "Ready!", None, &self.keep_changing(n));
        let title = match self.follows.get() {
            Some(GaugeKind::Bar) => "Ready: keep the bar changing in the game",
            Some(GaugeKind::Icons) => "Ready: keep the icons changing in the game",
            None => "Ready: change the number in the game",
        };
        self.tell(title, &format!("{}. Every change narrows it down.", places(n)), None, Some(6), true);
        let card = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(4), move || {
            if card.shown.get() == shown {
                card.strip(card.count.get());
            }
        });
    }

    pub fn watching(&self, n: usize) {
        self.count.set(n);
        // "Ready!" stays up its few seconds, with the new count; then the strip.
        if self.root.has_css_class("ready") && self.root.is_visible() {
            self.hint.set_label(&self.keep_changing(n));
            return;
        }
        let title = match self.follows.get() {
            Some(GaugeKind::Bar) => "Searching: keep the bar changing in the game",
            Some(GaugeKind::Icons) => "Searching: keep the icons changing in the game",
            None => "Searching: keep changing the number in the game",
        };
        self.show("watching", title, None, &places(n));
        self.tell(title, &format!("{}.", places(n)), None, Some(6), false);
    }

    /// The Searching strip, after "Ready!" or "Scanned again" had their few seconds. `watching`
    /// leaves "Ready!" up: called from Ready's own timer, the strip never came and every switch
    /// to the game sent "Ready" again (Plasma refused them after a few).
    fn strip(&self, n: usize) {
        self.root.remove_css_class("ready");
        self.watching(n);
    }

    /// Scan Again while Start runs: how it went for a few seconds, then the strip again.
    pub fn scanned_again(self: &Rc<Self>, before: usize, after: usize) {
        self.count.set(after);
        let hint = match before == after {
            true => format!("Still {}", places(after)),
            false => format!("{} \u{2192} {}", grouped(before), places(after)),
        };
        let shown = self.show("done", "Scanned again", None, &hint);
        self.notify.send("Scanned again", &hint, None, Some(4));
        let card = self.clone();
        glib::timeout_add_local_once(Duration::from_secs(3), move || {
            if card.shown.get() == shown {
                card.strip(card.count.get());
            }
        });
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
        self.offer_button("Stop Waiting", "Undo the test values and list the places, to try them one by one");
        self.tell(
            "Your turn: change the number in the game",
            &format!("Pick some up or use some. Ferret watches which of the {places} places the game carries on from."),
            Some("Stop Waiting"),
            None,
            true,
        );
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

    /// Saving: the player changes the number so Ferret sees which code does. No countdown (the
    /// player may need a while to get to where it changes); blinks until Cancel or the change.
    pub fn save_turn(self: &Rc<Self>) {
        let shown = self.show(
            "turn",
            "Your turn: change the number in the game!",
            None,
            "Use some or pick some up, once. Ferret watches which of the game's code changes it, \
             the surest way to find it again after a restart.",
        );
        self.offer_button("Cancel", "Stop waiting and save it another way, which may not last a restart");
        self.tell(
            "Your turn: change the number in the game",
            "Use some or pick some up, once. Ferret watches which of the game's code changes it.",
            Some("Cancel"),
            None,
            true,
        );
        let card = self.clone();
        glib::timeout_add_local(Duration::from_millis(600), move || {
            if card.shown.get() != shown {
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

    /// Shows the card's button (it stops what the card waits for); the card takes clicks then.
    fn offer_button(&self, label: &str, tooltip: &str) {
        self.give_up.set_label(label);
        self.give_up.set_tooltip_text(Some(tooltip));
        self.give_up.set_visible(true);
        self.root.set_can_target(true);
    }

    /// A typed number narrowed the search to `text` ("7 places match"): the player changes the
    /// number in the game next. Blinks for a few seconds.
    pub fn change_now(self: &Rc<Self>, text: &str) {
        let shown = self.show("turn", "Now change the number in the game", None, &format!("{text}. Then type the new number here."));
        self.tell("Now change the number in the game", &format!("{text}. Then type the new number in Ferret."), None, Some(6), true);
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
        self.done_away(found, title, hint, hint);
    }

    /// `done`, with `away` for the notification when the hint points at the window ("below").
    pub fn done_away(self: &Rc<Self>, found: bool, title: &str, hint: &str, away: &str) {
        let shown = self.show(if found { "ready" } else { "done" }, title, None, hint);
        self.notify.send(title, away, None, Some(if found { 10 } else { 6 }));
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
