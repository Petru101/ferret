// The Values tab: every saved value with its live number, a field to change
// it, a "Keep in range" switch, and a button to remove it.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;

use super::{Event, Worker};
use crate::core::{self, ValueRow};
use crate::i18n::{gettext, n_, ntr, tr};

struct ValueWidgets {
    group: adw::PreferencesGroup,
    value: gtk::Label,
    limit: adw::ExpanderRow,
    /// "Does the game show 31?" on a value whose pointer paths a restart hasn't confirmed.
    confirm: adw::ActionRow,
    confirm_button: gtk::Button,
    /// The Set field and button: off for values Ferret only shows.
    set: gtk::Box,
    /// "Keep It": never below the number now.
    keep: gtk::Button,
    /// The number now, as the game shows it.
    current: Rc<Cell<Option<f64>>>,
    keep_it: Rc<dyn Fn()>,
    /// The range as last shown (at least, at most), and what fills the fields with another one
    /// without applying it: Set moves a Keep It minimum to the new number in the core.
    shown_limit: Cell<(Option<f64>, Option<f64>)>,
    fill_limit: Rc<dyn Fn(Option<f64>, Option<f64>)>,
}

fn confirm_paths() -> String {
    tr!("Then Ferret finds it this way from now on, and keeps it in range. If it shows another number, find it again instead.")
}

/// The helper's states of a limit (`cmd_limits`, `paused`), shown in the player's language.
const LIMIT_STATES: [&str; 11] = [
    n_!("turned off"),
    n_!("active"),
    n_!("waiting for the game to change it once before writing"),
    n_!("its pointer paths don't agree on where it is, not writing"),
    n_!("its pointer paths lead nowhere right now, waiting"),
    n_!("looking for it by name"),
    n_!("it isn't anywhere right now (none in the game?), waiting"),
    n_!("its name leads to places that aren't one value, not written"),
    n_!("the object holding it changed, finding it again"),
    n_!("unreadable, finding it again"),
    n_!("not found yet (the game hasn't run the code that uses it), waiting"),
];

/// Confirms a value's pointer paths: the player saw the game show the number Ferret reads.
pub fn confirm_value(worker: &Worker, name: &str) {
    let name = name.to_owned();
    worker.run(move |core| Event::Done(core.confirm_paths(&name)));
    worker.run(|core| Event::Values(core.values()));
}

pub struct ValuesView {
    pub root: gtk::Stack,
    list: gtk::Box,
    rows: RefCell<HashMap<String, ValueWidgets>>,
    worker: Worker,
}

/// "fixed 1 times, restored 0 times, active" -> "fixed 1 time · active"
fn state_text(state: &str) -> String {
    let count = |p: &str, what: &str| p.strip_prefix(what)?.strip_suffix(" times")?.parse::<u64>().ok();
    let (counts, what) = match state.split_once(" times, restored ").and_then(|(_, r)| r.split_once(" times, ")) {
        Some((_, what)) => (&state[..state.len() - what.len() - 2], what),
        None => ("", state),
    };
    let mut parts: Vec<String> = Vec::new();
    for p in counts.split(", ") {
        if let Some(n) = count(p, "fixed ") {
            parts.push(ntr!("fixed {n} time", "fixed {n} times", n));
        } else if let Some(n) = count(p, "restored ").filter(|&n| n > 0) {
            parts.push(ntr!("restored {n} time", "restored {n} times", n));
        }
    }
    parts.push(LIMIT_STATES.iter().find(|s| **s == what).map_or(what.to_owned(), |s| gettext(s)));
    parts.join(" · ")
}

fn limit_subtitle(v: &ValueRow) -> String {
    match &v.limit_state {
        Some(state) if state.ends_with("turned off") => {
            tr!("Turned off with the hotkey · keeps {range} when on", range = super::range_text(v.min, v.max))
        }
        Some(state) => tr!("Kept {range} · {state}", range = super::range_text(v.min, v.max), state = state_text(state)),
        None => match v.suggested {
            (None, None) => tr!("Off"),
            (min, max) => tr!("Off · the import suggests keeping it {range}", range = super::range_text(min, max)),
        },
    }
}

/// Asks before forgetting a saved value; `parent` is any widget in the window.
pub fn ask_remove(worker: &Worker, name: &str, parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Remove {name}?", name)),
        Some(&tr!("Ferret stops finding it and keeping it in range. The number in the game stays as it is.")),
    );
    dialog.add_responses(&[("cancel", &tr!("Cancel")), ("remove", &tr!("Remove"))]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (worker, name) = (worker.clone(), name.to_owned());
    dialog.connect_response(Some("remove"), move |_, _| {
        let name = name.clone();
        worker.run_now(move |core| Event::Done(core.remove(&name).map(|_| tr!("Removed {name}", name))));
        worker.run(|core| Event::Values(core.values()));
    });
    dialog.present(Some(parent));
}

impl ValuesView {
    pub fn new(worker: Worker, on_find: impl Fn() + 'static) -> Rc<Self> {
        let find = gtk::Button::builder()
            .label(tr!("Find a Value"))
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        find.connect_clicked(move |_| on_find());
        let empty = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title(tr!("No Saved Values Yet"))
            .description(tr!("Find a value in the game and save it. Saved values are found again automatically every time you attach."))
            .child(&find)
            .build();
        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(24)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&adw::Clamp::builder().maximum_size(640).child(&list).build())
            .build();
        let root = gtk::Stack::new();
        root.add_named(&empty, Some("empty"));
        root.add_named(&scroll, Some("list"));
        Rc::new(Self { root, list, rows: RefCell::default(), worker })
    }

    /// Keep It on a value by name (the D-Bus `keep` action).
    pub fn keep(&self, name: &str) {
        let keep_it = self.rows.borrow().get(name).map(|w| w.keep_it.clone());
        if let Some(keep_it) = keep_it {
            keep_it();
        }
    }

    pub fn update(&self, values: Vec<ValueRow>) {
        self.root.set_visible_child_name(if values.is_empty() { "empty" } else { "list" });
        let mut rows = self.rows.borrow_mut();
        rows.retain(|name, w| {
            let keep = values.iter().any(|v| v.name == *name);
            if !keep {
                self.list.remove(&w.group);
            }
            keep
        });
        for v in &values {
            if !rows.contains_key(&v.name) {
                let w = self.create(v);
                self.list.append(&w.group);
                rows.insert(v.name.clone(), w);
            }
            let w = &rows[&v.name];
            // One sentence each, in this order.
            let kind = super::kind_text(v.kind);
            let mut about = vec![match v.places {
                0 if v.read_only => tr!("Saved in an earlier run, {kind}. Find it again and save it with the same name to see it.", kind),
                0 => tr!("Not in the game right now (none of it, or no save loaded), {kind}.", kind),
                _ => match v.decimals {
                    Some(d) if d > 0 => ntr!(
                        "At {place}, {kind}, shown with {n} decimal.",
                        "At {place}, {kind}, shown with {n} decimals.",
                        d,
                        place = format!("0x{:x}", v.addr),
                        kind
                    ),
                    _ => tr!("At {place}, {kind}.", place = format!("0x{:x}", v.addr), kind),
                },
            }];
            if v.places > 1 {
                about.push(ntr!(
                    "Kept in {n} place (every stack): setting it sets each.",
                    "Kept in {n} places (every stack): setting it sets each.",
                    v.places
                ));
            }
            if v.imported {
                about.push(tr!("Imported, not checked yet: if the game shows this number, say so below. Ferret won't change it until then."));
            } else if v.confirmable {
                about.push(tr!("Not confirmed yet: if the game shows this number, say so below."));
            } else if v.unconfirmed {
                about.push(tr!("Not confirmed yet: restart the game, then check the number here."));
            }
            if let (Some(d), false) = (&v.doubtful, v.imported) {
                about.push(tr!("Not written right now: {why}.", why = d));
            }
            if v.read_only {
                about.push(tr!("Java game: only shown. Java moves its objects, so a write could land in another one and crash the game."));
            }
            let about = about.join(" ");
            w.set.set_sensitive(!v.read_only);
            w.limit.set_sensitive(!v.read_only);
            w.current.set(v.value);
            if w.shown_limit.replace((v.min, v.max)) != (v.min, v.max) {
                (w.fill_limit)(v.min, v.max);
            }
            w.keep.set_sensitive(v.value.is_some_and(|n| n > 0.0));
            w.group.set_description(Some(&about));
            let shown = v.value.map_or("?".into(), |n| core::number_text(n, v.decimals));
            w.value.set_tooltip_text(Some(&shown));
            w.value.set_label(&shown);
            w.limit.set_subtitle(&limit_subtitle(v));
            w.confirm.set_visible(v.confirmable);
            if let (true, Some(n)) = (v.confirmable, v.value) {
                let n = core::number_text(n, v.decimals);
                w.confirm.set_title(&tr!("Does the game show {n}?", n));
                w.confirm_button.set_label(&tr!("Yes, It Shows {n}", n));
                w.confirm.set_subtitle(&match v.imported {
                    true => tr!("Then Ferret can change it and keep it in range. If it shows another number, remove it and find it yourself."),
                    false => confirm_paths(),
                });
            }
        }
    }

    fn create(&self, v: &ValueRow) -> ValueWidgets {
        let name = v.name.clone();
        let group = adw::PreferencesGroup::builder().title(&v.name).build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text(tr!("Remove"))
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let (worker, name) = (self.worker.clone(), name.clone());
            remove.connect_clicked(move |button| ask_remove(&worker, &name, button));
        }
        group.set_header_suffix(Some(&remove));

        // A huge number (Age of War's coins at 5555554545654 and up) cut short, whole in the
        // tooltip: it stretched the window before.
        let value = gtk::Label::builder()
            .css_classes(["title-3", "numeric"])
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .max_width_chars(16)
            .build();
        let entry = gtk::Entry::builder()
            .placeholder_text(tr!("New value"))
            .width_chars(10)
            .input_purpose(if v.decimals == Some(0) { gtk::InputPurpose::Digits } else { gtk::InputPurpose::Number })
            .valign(gtk::Align::Center)
            .build();
        let set = gtk::Button::builder().label(tr!("Set")).valign(gtk::Align::Center).build();
        let row = adw::ActionRow::builder().title(tr!("Value")).build();
        row.add_suffix(&value);
        let set_box = gtk::Box::builder().spacing(6).valign(gtk::Align::Center).build();
        set_box.append(&entry);
        set_box.append(&set);
        row.add_suffix(&set_box);
        group.add(&row);
        let apply_set = {
            let (worker, entry, name) = (self.worker.clone(), entry.clone(), name.clone());
            move || {
                let Some(n) = core::parse_number(&entry.text()) else { return };
                entry.set_text("");
                let name = name.clone();
                worker.run_now(move |core| Event::Done(core.set(&name, n).map(|_| tr!("{name} set to {n}", name, n))));
            }
        };
        {
            let apply_set = apply_set.clone();
            set.connect_clicked(move |_| apply_set());
        }
        entry.connect_activate(move |_| apply_set());

        let confirm_button = gtk::Button::builder().valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        let confirm = adw::ActionRow::builder()
            .title_lines(1)
            .subtitle(confirm_paths())
            .visible(false)
            .build();
        confirm.add_suffix(&confirm_button);
        group.add(&confirm);
        {
            let (worker, name) = (self.worker.clone(), name.clone());
            confirm_button.connect_clicked(move |_| confirm_value(&worker, &name));
        }

        let limit = adw::ExpanderRow::builder()
            .title(tr!("Keep in Range"))
            .subtitle_lines(2)
            .show_enable_switch(true)
            .enable_expansion(v.min.is_some() || v.max.is_some())
            .expanded(false)
            .build();
        // Floats get 2 decimals; whole numbers shown with decimals step by their last digit.
        let digits = v.decimals.unwrap_or(2);
        // The fields start at the saved range, else at the one an import suggested (off).
        let (lo, hi) = if v.min.is_some() || v.max.is_some() { (v.min, v.max) } else { v.suggested };
        let step = v.decimals.map_or(1.0, |d| 10f64.powi(-(d as i32)));
        let top = i32::MAX as f64 * step;
        let max = adw::SpinRow::with_range(0.0, top, step);
        max.set_digits(digits);
        max.set_title(&tr!("At Most"));
        max.set_value(hi.or(v.value).unwrap_or(0.0));
        // Only a minimum: the game can raise it as far as it likes.
        let endless = gtk::ToggleButton::builder()
            .label("\u{221e}")
            .tooltip_text(tr!("No maximum: only At Least is kept"))
            .valign(gtk::Align::Center)
            .active(lo.is_some() && hi.is_none())
            .build();
        max.add_suffix(&endless);
        if endless.is_active() {
            max.set_subtitle(&tr!("No maximum"));
        }
        let min = adw::SpinRow::with_range(0.0, top, step);
        min.set_digits(digits);
        min.set_title(&tr!("At Least"));
        min.set_subtitle(&tr!("0 means no minimum"));
        min.set_value(lo.unwrap_or(0.0));
        limit.add_row(&max);
        limit.add_row(&min);
        let keep = gtk::Button::builder()
            .label(tr!("Keep It"))
            .tooltip_text(tr!("Never let it drop below the number it is now"))
            .valign(gtk::Align::Center)
            .build();
        limit.add_suffix(&keep);
        group.add(&limit);
        // Keep It fills the fields in one go and applies the range once.
        let filling = Rc::new(Cell::new(false));

        // Writes only when the game goes past the range, see Core::limit.
        let apply_limit = {
            let (worker, limit, max, min, endless, name) = (self.worker.clone(), limit.clone(), max.clone(), min.clone(), endless.clone(), name);
            move || {
                // What the field shows, without float noise past its digits.
                let shown = |s: &adw::SpinRow| {
                    let f = 10f64.powi(s.digits() as i32);
                    (s.value() * f).round() / f
                };
                let (min_v, max_v) = if limit.enables_expansion() {
                    let m = shown(&min);
                    ((m > 0.0).then_some(m), (!endless.is_active()).then(|| shown(&max)))
                } else {
                    (None, None)
                };
                // The saved range stays until the two numbers make sense again.
                let backwards = matches!((min_v, max_v), (Some(lo), Some(hi)) if hi < lo);
                max.set_subtitle(&match () {
                    _ if backwards => tr!("Lower than At Least: the range isn't changed"),
                    _ if endless.is_active() => tr!("No maximum"),
                    _ => String::new(),
                });
                if backwards {
                    max.add_css_class("error");
                    return;
                }
                max.remove_css_class("error");
                let name = name.clone();
                worker.run_now(move |core| {
                    Event::Done(core.limit(&name, min_v, max_v).map(|_| match (min_v, max_v) {
                        (None, None) => tr!("{name} is no longer limited", name),
                        _ => tr!("{name} is kept {range}", name, range = super::range_text(min_v, max_v)),
                    }))
                });
            }
        };
        {
            let apply_limit = apply_limit.clone();
            let filling = filling.clone();
            limit.connect_enable_expansion_notify(move |_| {
                if !filling.get() {
                    apply_limit();
                }
            });
        }
        {
            let (apply_limit, limit, filling) = (apply_limit.clone(), limit.clone(), filling.clone());
            endless.connect_toggled(move |_| {
                if limit.enables_expansion() && !filling.get() {
                    apply_limit();
                }
            });
        }
        // Changing a number re-applies the limit once typing has settled.
        let pending: Rc<Cell<Option<glib::SourceId>>> = Rc::default();
        for spin in [&max, &min] {
            let (apply_limit, pending, limit, filling) = (apply_limit.clone(), pending.clone(), limit.clone(), filling.clone());
            spin.connect_value_notify(move |_| {
                if !limit.enables_expansion() || filling.get() {
                    return;
                }
                if let Some(id) = pending.take() {
                    id.remove();
                }
                let (apply_limit, pending_done) = (apply_limit.clone(), pending.clone());
                pending.set(Some(glib::timeout_add_local_once(Duration::from_millis(700), move || {
                    pending_done.take();
                    apply_limit();
                })));
            });
        }
        let current: Rc<Cell<Option<f64>>> = Rc::new(Cell::new(v.value));
        let keep_it: Rc<dyn Fn()> = {
            let (current, limit, min, endless, filling) = (current.clone(), limit.clone(), min.clone(), endless.clone(), filling.clone());
            Rc::new(move || {
                let Some(n) = current.get().filter(|&n| n > 0.0) else { return };
                // Rounded down to the field's digits: never above what the game has now.
                let f = 10f64.powi(min.digits() as i32);
                filling.set(true);
                endless.set_active(true);
                min.set_value((n * f).floor() / f);
                limit.set_enable_expansion(true);
                filling.set(false);
                apply_limit();
            })
        };
        {
            let keep_it = keep_it.clone();
            keep.connect_clicked(move |_| keep_it());
        }
        let fill_limit: Rc<dyn Fn(Option<f64>, Option<f64>)> = {
            let (limit, min, max, endless, filling) = (limit.clone(), min.clone(), max.clone(), endless.clone(), filling.clone());
            Rc::new(move |lo, hi| {
                filling.set(true);
                endless.set_active(lo.is_some() && hi.is_none());
                if let Some(hi) = hi {
                    max.set_value(hi);
                }
                max.set_subtitle(&if endless.is_active() { tr!("No maximum") } else { String::new() });
                min.set_value(lo.unwrap_or(0.0));
                limit.set_enable_expansion(lo.is_some() || hi.is_some());
                filling.set(false);
            })
        };
        let shown_limit = Cell::new((v.min, v.max));
        ValueWidgets { group, value, limit, confirm, confirm_button, set: set_box, keep, current, keep_it, shown_limit, fill_limit }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shows_limit_states() {
        assert_eq!(state_text("fixed 1 times, restored 0 times, active"), "fixed 1 time · active");
        assert_eq!(
            state_text("fixed 0 times, restored 3 times, its pointer paths lead nowhere right now, waiting"),
            "fixed 0 times · restored 3 times · its pointer paths lead nowhere right now, waiting"
        );
        assert_eq!(state_text("something new"), "something new");
    }
}
