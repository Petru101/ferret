// The instruction bar over the game page: what Ferret is doing and what the player should do
// now, in one place. Two looks matter most: HANDS OFF (orange, Ferret is working: changing the
// number in the game now spoils it) and YOUR TURN (green, change it now). The Find tab switches
// to HANDS OFF the moment a button is clicked, before the core starts: the floating card it
// replaces followed the core's events, so Ferret was already scanning before it said so, and the
// player changed the number at the wrong time. A row of steps above says where the search is.
// What the player has to know while the game is in front also goes out as a notification
// (`notify.rs`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use adw::prelude::*;
use gtk::glib;

use super::find::grouped;
use super::notify::Notifier;
use crate::bar::Kind as GaugeKind;
use crate::i18n::{gettext, n_, ntr, tr};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Look {
    /// Nothing runs: pick a number, press Start, type one.
    Pick,
    /// Ferret is working: don't change anything in the game.
    Wait,
    /// Change the number in the game now.
    Go,
    /// Ferret can't tell: the player decides (try the places left, or go on).
    Call,
    Found,
}

impl Look {
    const ALL: [Look; 5] = [Look::Pick, Look::Wait, Look::Go, Look::Call, Look::Found];

    fn class(self) -> &'static str {
        match self {
            Look::Pick => "pick",
            Look::Wait => "wait",
            Look::Go => "go",
            Look::Call => "call",
            Look::Found => "found",
        }
    }

    fn tag(self) -> String {
        match self {
            // Translators: the instruction bar's tags, short and in capitals.
            Look::Pick => tr!("YOUR PICK"),
            Look::Wait => tr!("HANDS OFF"),
            Look::Go => tr!("YOUR TURN"),
            Look::Call => tr!("YOUR CALL"),
            Look::Found => tr!("FOUND"),
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Look::Pick => "input-mouse-symbolic",
            Look::Wait => "media-playback-pause-symbolic",
            Look::Go => "media-playback-start-symbolic",
            Look::Call => "dialog-question-symbolic",
            Look::Found => "object-select-symbolic",
        }
    }
}

/// The steps row: where a search is.
pub const PICK: usize = 0;
pub const HANDS_OFF: usize = 1;
pub const CHANGE: usize = 2;
pub const CHECK: usize = 3;
pub const SAVE: usize = 4;
// Translators: the steps of a search, shown in a row of small pills.
const STEPS: [&str; 5] = [n_!("Pick the number"), n_!("Hands off"), n_!("Change it"), n_!("Checking"), n_!("Save")];

pub struct Guide {
    pub root: gtk::Box,
    steps: Vec<gtk::Label>,
    icon: gtk::Image,
    tag: gtk::Label,
    spinner: gtk::Spinner,
    title: gtk::Label,
    hint: gtk::Label,
    bar: gtk::ProgressBar,
    /// The buttons for what to do next (Start, Stop, the save row): the Find tab's, put here.
    pub actions: gtk::Box,
    /// Cancel while a save waits for the game to change the value (`save_turn`).
    pub give_up: gtk::Button,
    look: Cell<Look>,
    /// Counts the bar's changes: its timers stop once something else came.
    shown: Rc<Cell<u32>>,
    /// The places matching, for the texts that follow.
    count: Cell<usize>,
    notify: Rc<Notifier>,
    /// What the bar asks of the player now (title, text, button, seconds): sent again when they
    /// switch to the game (`remind`); "Ready" usually comes while Ferret is still in front.
    away: RefCell<Option<(String, String, Option<String>, Option<u32>)>>,
    /// `away` went out already: once is enough (the player switching back and forth got the
    /// same one every time, until Plasma refused them as too many).
    reminded: Cell<bool>,
    /// The search follows a bar or a row of icons, not a number: the texts say so.
    pub follows: Cell<Option<GaugeKind>>,
    /// The Values tab is in front: the bar shows only while something happens.
    on_values: Cell<bool>,
    /// Buttons offered for what's on the bar now (`offer`): hidden when it changes.
    offered: RefCell<Option<gtk::Widget>>,
    /// Told each new look (the Find tab shows Start only while it's the player's pick).
    on_show: RefCell<Option<Box<dyn Fn(Look)>>>,
    /// "Got it" notifications: when the last went out, and one held back to keep them apart.
    got_it: Cell<Option<Instant>>,
    got_it_next: Rc<RefCell<Option<(String, String)>>>,
    /// A "Hands off" notification is out: the next hands-off step (Start, then the scan) keeps
    /// it instead of closing and sending it again.
    hands_off: Cell<bool>,
}

fn hands_off_notice() -> String {
    tr!("Hands off: don't change anything in the game yet")
}

/// "Got it" notifications at most this often (Plasma refuses an app that sends too many).
const GOT_IT_GAP: Duration = Duration::from_secs(2);

fn places(n: usize) -> String {
    ntr!("{n} place matches", "{n} places match", n, n = grouped(n))
}

impl Guide {
    pub fn new(notify: Rc<Notifier>) -> Rc<Self> {
        let steps: Vec<gtk::Label> = STEPS
            .iter()
            .enumerate()
            .map(|(i, s)| gtk::Label::builder().label(format!("{}  {}", i + 1, gettext(s))).css_classes(["guide-step"]).build())
            .collect();
        let steps_row = gtk::Box::builder().spacing(6).build();
        steps.iter().for_each(|s| steps_row.append(s));
        let icon = gtk::Image::builder().pixel_size(22).valign(gtk::Align::Start).css_classes(["guide-icon"]).build();
        let tag = gtk::Label::builder().css_classes(["guide-tag"]).build();
        let spinner = gtk::Spinner::new();
        let head = gtk::Box::builder().spacing(10).build();
        head.append(&tag);
        head.append(&spinner);
        let title = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["guide-title"]).build();
        let hint = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["guide-hint"]).build();
        let bar = gtk::ProgressBar::builder().show_text(true).visible(false).build();
        let text = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(4).hexpand(true).build();
        text.append(&head);
        text.append(&title);
        text.append(&hint);
        text.append(&bar);
        let give_up = gtk::Button::builder().label(tr!("Cancel")).visible(false).build();
        let actions = gtk::Box::builder().spacing(8).valign(gtk::Align::Center).build();
        actions.append(&give_up);
        let main = gtk::Box::builder().spacing(14).build();
        main.append(&icon);
        main.append(&text);
        main.append(&actions);
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(10)
            .margin_start(12)
            .margin_end(12)
            .margin_top(6)
            .margin_bottom(6)
            .css_classes(["guide"])
            .build();
        root.append(&steps_row);
        root.append(&main);
        let guide = Rc::new(Self {
            root,
            steps,
            icon,
            tag,
            spinner,
            title,
            hint,
            bar,
            actions,
            give_up,
            look: Cell::new(Look::Pick),
            shown: Rc::default(),
            count: Cell::new(0),
            notify,
            away: RefCell::default(),
            reminded: Cell::new(false),
            follows: Cell::new(None),
            on_values: Cell::new(false),
            offered: RefCell::default(),
            on_show: RefCell::default(),
            got_it: Cell::new(None),
            got_it_next: Rc::default(),
            hands_off: Cell::new(false),
        });
        guide.pick(&tr!("Click the number you want to find"), &tr!("In the picture of your game below, or drag a box around it."));
        guide
    }

    /// Shows the bar in `look` at `step`; `bar` = how far, when there's something to measure.
    pub fn show(&self, look: Look, step: usize, title: &str, hint: &str, bar: Option<f64>) -> u32 {
        let shown = self.shown.get() + 1;
        self.shown.set(shown);
        // Hands off for something in the game (not a capture, a read or Stop).
        let hands_off = look == Look::Wait && step != PICK;
        let still = hands_off && self.look.get() == Look::Wait && self.hands_off.get();
        if !still {
            self.notify.withdraw_waiting();
            self.hands_off.set(false);
        }
        self.away.take();
        self.look.set(look);
        for l in Look::ALL {
            self.root.remove_css_class(l.class());
        }
        self.root.remove_css_class("flash");
        self.root.add_css_class(look.class());
        for (i, s) in self.steps.iter().enumerate() {
            s.remove_css_class("now");
            s.remove_css_class("past");
            if i == step {
                s.add_css_class("now");
            } else if i < step {
                s.add_css_class("past");
            }
        }
        self.icon.set_icon_name(Some(look.icon()));
        self.tag.set_label(&look.tag());
        self.spinner.set_spinning(look == Look::Wait);
        self.spinner.set_visible(look == Look::Wait);
        self.title.set_label(title);
        self.hint.set_label(hint);
        self.hint.set_visible(!hint.is_empty());
        self.bar.set_visible(bar.is_some());
        self.bar.set_show_text(look == Look::Wait);
        self.bar.set_fraction(bar.unwrap_or(0.0).clamp(0.0, 1.0));
        self.give_up.set_visible(false);
        if let Some(w) = self.offered.take() {
            w.set_visible(false);
        }
        self.shown_on_tab();
        // The player in the game must hear it too: started with the hotkey from the game, or
        // switching to the game while Ferret scans (sent then, by `remind`). Until the next step.
        if hands_off {
            let body = tr!("{step}. Ferret tells you when it's your turn.", step = title);
            if still {
                self.away.replace(Some((hands_off_notice(), body, None, None)));
            } else {
                self.tell(&hands_off_notice(), &body, None, None, true);
                self.hands_off.set(self.notify.away());
            }
        }
        if let Some(f) = self.on_show.borrow().as_ref() {
            f(look);
        }
        shown
    }

    pub fn on_show(&self, f: impl Fn(Look) + 'static) {
        self.on_show.replace(Some(Box::new(f)));
    }

    /// Shows `w` (in `actions`) until the bar changes: the buttons for what it says now.
    pub fn offer(&self, w: &impl IsA<gtk::Widget>) {
        if let Some(old) = self.offered.replace(Some(w.clone().upcast())) {
            old.set_visible(false);
        }
        w.set_visible(true);
    }

    /// Nothing runs: what the player can do next.
    pub fn pick(&self, title: &str, hint: &str) {
        self.show(Look::Pick, PICK, title, hint, None);
    }

    /// Ferret works on something the player asked for: hands off at once, before the core
    /// says anything.
    pub fn wait(&self, step: usize, title: &str, hint: &str) {
        self.show(Look::Wait, step, title, hint, None);
    }

    /// A line from the core about what the search waits for (the box doesn't show the number,
    /// ...): the bar's look and title stay.
    pub fn note(&self, hint: &str) {
        self.hint.set_label(hint);
        self.hint.set_visible(!hint.is_empty());
    }

    /// The Values tab came to the front (or left): the bar shows there only while something
    /// happens.
    pub fn on_values(&self, yes: bool) {
        self.on_values.set(yes);
        self.shown_on_tab();
    }

    fn shown_on_tab(&self) {
        self.root.set_visible(!self.on_values.get() || self.look.get() != Look::Pick);
    }

    /// Notifies the player of what the bar asks of them (`send`: now too, not only when they
    /// switch to the game).
    fn tell(&self, title: &str, text: &str, button: Option<String>, secs: Option<u32>, send: bool) {
        if send {
            self.notify.send(title, text, button.as_deref(), secs);
        }
        self.reminded.set(send && self.notify.away());
        self.away.replace(Some((title.to_owned(), text.to_owned(), button, secs)));
    }

    /// Ferret's window lost the front: what the bar asks of the player, as a notification.
    pub fn remind(&self) {
        if self.reminded.replace(true) {
            return;
        }
        if let Some((title, text, button, secs)) = self.away.borrow().clone() {
            self.notify.send(&title, &text, button.as_deref(), secs);
            self.hands_off.set(title == hands_off_notice());
        }
    }

    /// Blinks while the player has to act (until the bar changes).
    fn blink(self: &Rc<Self>, shown: u32, every: impl Fn(&Self) + 'static) {
        let guide = self.clone();
        glib::timeout_add_local(Duration::from_millis(600), move || {
            if guide.shown.get() != shown {
                guide.root.remove_css_class("flash");
                return glib::ControlFlow::Break;
            }
            every(&guide);
            if guide.root.has_css_class("flash") {
                guide.root.remove_css_class("flash");
            } else {
                guide.root.add_css_class("flash");
            }
            glib::ControlFlow::Continue
        });
    }

    pub fn scanning(&self, n: &str, done: f64) {
        if self.look.get() == Look::Wait && self.bar.is_visible() {
            self.bar.set_fraction(done.clamp(0.0, 1.0));
            return;
        }
        self.show(
            Look::Wait,
            HANDS_OFF,
            &tr!("Scanning the game's memory for {n}", n),
            &tr!("Don't change the number in the game yet. This turns green when it's your turn."),
            Some(done),
        );
    }

    /// A bar's search copies the game's memory first (no number to look for).
    pub fn copying(&self, done: f64) {
        if self.look.get() == Look::Wait && self.bar.is_visible() {
            self.bar.set_fraction(done.clamp(0.0, 1.0));
            return;
        }
        let hint = match self.follows.get() {
            Some(GaugeKind::Icons) => tr!("Don't let the icons change in the game yet. This turns green when it's your turn."),
            _ => tr!("Don't let the bar change in the game yet. This turns green when it's your turn."),
        };
        self.show(Look::Wait, HANDS_OFF, &tr!("Copying the game's memory"), &hint, Some(done));
    }

    /// The copy is taken: the bar has to move before anything narrows down.
    pub fn bar_ready(&self) {
        let (title, notice) = match self.follows.get() {
            Some(GaugeKind::Icons) => (tr!("Now let the icons change in the game"), tr!("Ready: let the icons change in the game")),
            _ => (tr!("Now let the bar change in the game"), tr!("Ready: let the bar change in the game")),
        };
        let hint = tr!("Take a hit, or use some. Every change narrows it down.");
        self.show(Look::Go, CHANGE, &title, &hint, None);
        self.tell(&notice, &hint, None, Some(6), true);
    }

    /// What Start follows: "Keep the bar changing" and the like.
    fn keep_changing(&self) -> String {
        match self.follows.get() {
            Some(GaugeKind::Bar) => tr!("Keep the bar changing in the game"),
            Some(GaugeKind::Icons) => tr!("Keep the icons changing in the game"),
            None => tr!("Keep changing the number in the game"),
        }
    }

    /// Start can follow the number now.
    pub fn ready(&self, n: usize) {
        self.count.set(n);
        let (title, notice) = match self.follows.get() {
            Some(GaugeKind::Bar) => (tr!("Now let the bar change in the game"), tr!("Ready: keep the bar changing in the game")),
            Some(GaugeKind::Icons) => (tr!("Now let the icons change in the game"), tr!("Ready: keep the icons changing in the game")),
            None => (tr!("Now change the number in the game"), tr!("Ready: change the number in the game")),
        };
        let hint = ntr!("{n} place matches. Every change narrows it down.", "{n} places match. Every change narrows it down.", n, n = grouped(n));
        self.show(Look::Go, CHANGE, &title, &hint, None);
        self.tell(&notice, &hint, None, Some(6), true);
    }

    /// A change on screen narrowed it down.
    pub fn watching(self: &Rc<Self>, n: usize) {
        let before = self.count.replace(n);
        let hint = match before {
            b if b > n => ntr!(
                "{before} \u{2192} {n} place matches. Every change narrows it down.",
                "{before} \u{2192} {n} places match. Every change narrows it down.",
                n,
                before = grouped(b),
                n = grouped(n)
            ),
            _ => ntr!("{n} place matches. Every change narrows it down.", "{n} places match. Every change narrows it down.", n, n = grouped(n)),
        };
        if self.look.get() == Look::Go {
            self.title.set_label(&self.keep_changing());
            self.note(&hint);
        } else {
            self.show(Look::Go, CHANGE, &self.keep_changing(), &hint, None);
        }
        // Each change Ferret counted, while the player is in the game: they waited for a word
        // that it took and got nothing (only "keep changing" when switching to the game, before
        // any change).
        if before > n {
            self.got_it(
                ntr!("Got it: {before} \u{2192} {n} place matches", "Got it: {before} \u{2192} {n} places match", n, before = grouped(before), n = grouped(n)),
                format!("{}.", self.keep_changing()),
            );
        }
        self.away.replace(Some((self.keep_changing(), format!("{}.", places(n)), None, Some(6))));
        self.reminded.set(true);
    }

    /// Sends a "Got it" notification, or holds it back until `GOT_IT_GAP` after the last one
    /// (the newest text wins).
    fn got_it(self: &Rc<Self>, title: String, body: String) {
        let wait = self.got_it.get().map_or(Duration::ZERO, |t| GOT_IT_GAP.saturating_sub(t.elapsed()));
        if wait.is_zero() {
            self.got_it.set(Some(Instant::now()));
            self.notify.send(&title, &body, None, Some(4));
            return;
        }
        if self.got_it_next.replace(Some((title, body))).is_some() {
            return;
        }
        let guide = self.clone();
        glib::timeout_add_local_once(wait, move || {
            if let Some((title, body)) = guide.got_it_next.take() {
                if guide.look.get() == Look::Go {
                    guide.got_it.set(Some(Instant::now()));
                    guide.notify.send(&title, &body, None, Some(4));
                }
            }
        });
    }

    /// Scan Again while Start runs.
    pub fn scanned_again(&self, before: usize, after: usize) {
        self.count.set(after);
        let hint = match before == after {
            true => ntr!(
                "Scanned again: still {n} place matches. Keep changing it, or press Scan Again while it stays the same.",
                "Scanned again: still {n} places match. Keep changing it, or press Scan Again while it stays the same.",
                after,
                n = grouped(after)
            ),
            false => ntr!(
                "Scanned again: {before} \u{2192} {n} place matches. Keep changing it, or press Scan Again while it stays the same.",
                "Scanned again: {before} \u{2192} {n} places match. Keep changing it, or press Scan Again while it stays the same.",
                after,
                before = grouped(before),
                n = grouped(after)
            ),
        };
        self.show(Look::Go, CHANGE, &self.keep_changing(), &hint, None);
        self.notify.send(&tr!("Scanned again"), &hint, None, Some(4));
    }

    pub fn checking(&self, places: usize, done: f64) {
        if self.look.get() == Look::Wait && self.bar.is_visible() && done > 0.0 {
            self.bar.set_fraction(done.clamp(0.0, 1.0));
            return;
        }
        let title = match places {
            1 => tr!("Checking the place found"),
            n => ntr!("Checking which of the {n} place it is", "Checking which of the {n} places it is", n),
        };
        self.show(Look::Wait, CHECK, &title, &tr!("A few seconds. Don't change anything in the game."), Some(done));
    }

    /// The player has to change the number in the game: blinks, and counts down.
    pub fn your_turn(self: &Rc<Self>, places: usize, secs: u64) {
        let hint = move |left: u64| {
            ntr!(
                "Pick some up or use some, once. Ferret watches which of the {n} place the game carries on from. {time} left.",
                "Pick some up or use some, once. Ferret watches which of the {n} places the game carries on from. {time} left.",
                places,
                time = format!("{}:{:02}", left / 60, left % 60)
            )
        };
        let shown = self.show(Look::Go, CHECK, &tr!("Your turn: change the number in the game!"), &hint(secs), Some(1.0));
        self.tell(
            &tr!("Your turn: change the number in the game"),
            &ntr!(
                "Pick some up or use some. Ferret watches which of the {n} place the game carries on from.",
                "Pick some up or use some. Ferret watches which of the {n} places the game carries on from.",
                places
            ),
            Some(tr!("Stop Waiting")),
            None,
            true,
        );
        let start = Instant::now();
        self.blink(shown, move |g| {
            let left = secs.saturating_sub(start.elapsed().as_secs());
            g.bar.set_fraction(left as f64 / secs as f64);
            g.hint.set_label(&hint(left));
        });
    }

    /// Saving: the player changes the number so Ferret sees which code does. No countdown (the
    /// player may need a while to get to where it changes); blinks until Cancel or the change.
    pub fn save_turn(self: &Rc<Self>) {
        let shown = self.show(
            Look::Go,
            SAVE,
            &tr!("Your turn: change the number in the game!"),
            &tr!(
                "Use some or pick some up, once. Ferret watches which of the game's code changes it, \
                 the surest way to find it again after a restart."
            ),
            None,
        );
        self.give_up.set_label(&tr!("Cancel"));
        self.give_up.set_tooltip_text(Some(&tr!("Stop waiting and save it another way, which may not last a restart")));
        self.give_up.set_visible(true);
        self.tell(
            &tr!("Your turn: change the number in the game"),
            &tr!("Use some or pick some up, once. Ferret watches which of the game's code changes it."),
            Some(tr!("Cancel")),
            None,
            true,
        );
        self.blink(shown, |_| {});
    }

    /// A typed number narrowed the search to `text` ("7 places match"): the player changes the
    /// number in the game next, then types the new one.
    pub fn change_now(&self, text: &str) {
        self.show(
            Look::Go,
            CHANGE,
            &tr!("Now change the number in the game"),
            &tr!("{count}. Then type the new number below.", count = text),
            None,
        );
        self.tell(
            &tr!("Now change the number in the game"),
            &tr!("{count}. Then type the new number in Ferret.", count = text),
            None,
            Some(6),
            true,
        );
    }

    /// How a search or a step of it ended; `away` = the notification's text, when the hint
    /// points at the window ("below").
    pub fn ended(&self, look: Look, step: usize, title: &str, hint: &str, away: &str) {
        self.show(look, step, title, hint, None);
        self.notify.send(title, away, None, Some(if look == Look::Found { 10 } else { 6 }));
    }
}
