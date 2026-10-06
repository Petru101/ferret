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
}

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
    let parts: Vec<String> = state
        .split(", ")
        .filter(|p| !p.starts_with("restored 0 "))
        .map(|p| p.replace(" 1 times", " 1 time"))
        .collect();
    parts.join(" · ")
}

fn limit_subtitle(v: &ValueRow) -> String {
    match &v.limit_state {
        Some(state) if state.ends_with("turned off") => format!("Turned off with the hotkey · keeps {} when on", core::limit_text(v.min, v.max)),
        Some(state) => format!("Kept {} · {}", core::limit_text(v.min, v.max), state_text(state)),
        None => "Off".into(),
    }
}

/// Asks before forgetting a saved value; `parent` is any widget in the window.
pub fn ask_remove(worker: &Worker, name: &str, parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AlertDialog::new(
        Some(&format!("Remove {name}?")),
        Some("Ferret stops finding it and keeping it in range. The number in the game stays as it is."),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("remove", "Remove")]);
    dialog.set_response_appearance("remove", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (worker, name) = (worker.clone(), name.to_owned());
    dialog.connect_response(Some("remove"), move |_, _| {
        let name = name.clone();
        worker.run_now(move |core| Event::Done(core.remove(&name).map(|_| format!("Removed {name}"))));
        worker.run(|core| Event::Values(core.values()));
    });
    dialog.present(Some(parent));
}

impl ValuesView {
    pub fn new(worker: Worker, on_find: impl Fn() + 'static) -> Rc<Self> {
        let find = gtk::Button::builder()
            .label("Find a Value")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        find.connect_clicked(move |_| on_find());
        let empty = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("No Saved Values Yet")
            .description("Find a value in the game and save it. Saved values are found again automatically every time you attach.")
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
            let mut about = match v.places {
                0 if v.read_only => format!("Saved in an earlier run, {}. Find it again and save it with the same name to see it", v.kind.describe()),
                0 => format!("Not in the game right now (none of it, or no save loaded), {}", v.kind.describe()),
                _ => format!("At 0x{:x}, {}", v.addr, v.kind.describe()),
            };
            match v.decimals {
                Some(1) => about.push_str(", shown with one decimal"),
                Some(d) if d > 1 => about.push_str(&format!(", shown with {d} decimals")),
                _ => {}
            }
            if v.places > 1 {
                about.push_str(&format!(". Kept in {} places (every stack): setting it sets each", v.places));
            }
            if v.confirmable {
                about.push_str(". Not confirmed yet: if the game shows this number, say so below");
            } else if v.unconfirmed {
                about.push_str(". Not confirmed yet: restart the game, then check the number here");
            }
            if let Some(d) = &v.doubtful {
                about.push_str(&format!(". Not written right now: {d}"));
            }
            if v.read_only {
                about.push_str(". Java game: only shown. Java moves its objects, so a write could land in another one and crash the game");
            }
            w.set.set_sensitive(!v.read_only);
            w.limit.set_sensitive(!v.read_only);
            w.current.set(v.value);
            w.keep.set_sensitive(v.value.is_some_and(|n| n > 0.0));
            w.group.set_description(Some(&about));
            w.value.set_label(&v.value.map_or("?".into(), |n| core::number_text(n, v.decimals)));
            w.limit.set_subtitle(&limit_subtitle(v));
            w.confirm.set_visible(v.confirmable);
            if let (true, Some(n)) = (v.confirmable, v.value) {
                let n = core::number_text(n, v.decimals);
                w.confirm.set_title(&format!("Does the game show {n}?"));
                w.confirm_button.set_label(&format!("Yes, It Shows {n}"));
            }
        }
    }

    fn create(&self, v: &ValueRow) -> ValueWidgets {
        let name = v.name.clone();
        let group = adw::PreferencesGroup::builder().title(&v.name).build();
        let remove = gtk::Button::builder()
            .icon_name("user-trash-symbolic")
            .tooltip_text("Remove")
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        {
            let (worker, name) = (self.worker.clone(), name.clone());
            remove.connect_clicked(move |button| ask_remove(&worker, &name, button));
        }
        group.set_header_suffix(Some(&remove));

        let value = gtk::Label::builder().css_classes(["title-3", "numeric"]).build();
        let entry = gtk::Entry::builder()
            .placeholder_text("New value")
            .width_chars(10)
            .input_purpose(if v.decimals == Some(0) { gtk::InputPurpose::Digits } else { gtk::InputPurpose::Number })
            .valign(gtk::Align::Center)
            .build();
        let set = gtk::Button::builder().label("Set").valign(gtk::Align::Center).build();
        let row = adw::ActionRow::builder().title("Value").build();
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
                worker.run_now(move |core| Event::Done(core.set(&name, n).map(|_| format!("{name} set to {n}"))));
            }
        };
        {
            let apply_set = apply_set.clone();
            set.connect_clicked(move |_| apply_set());
        }
        entry.connect_activate(move |_| apply_set());

        let confirm_button = gtk::Button::builder().valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
        let confirm = adw::ActionRow::builder()
            .subtitle("Then Ferret finds it this way from now on, and keeps it in range. If it shows another number, find it again instead.")
            .visible(false)
            .build();
        confirm.add_suffix(&confirm_button);
        group.add(&confirm);
        {
            let (worker, name) = (self.worker.clone(), name.clone());
            confirm_button.connect_clicked(move |_| confirm_value(&worker, &name));
        }

        let limit = adw::ExpanderRow::builder()
            .title("Keep in Range")
            .show_enable_switch(true)
            .enable_expansion(v.min.is_some() || v.max.is_some())
            .expanded(false)
            .build();
        // Floats get 2 decimals; whole numbers shown with decimals step by their last digit.
        let digits = v.decimals.unwrap_or(2);
        let step = v.decimals.map_or(1.0, |d| 10f64.powi(-(d as i32)));
        let top = i32::MAX as f64 * step;
        let max = adw::SpinRow::with_range(0.0, top, step);
        max.set_digits(digits);
        max.set_title("At Most");
        max.set_value(v.max.or(v.value).unwrap_or(0.0));
        // Only a minimum: the game can raise it as far as it likes.
        let endless = gtk::ToggleButton::builder()
            .label("\u{221e}")
            .tooltip_text("No maximum: only At Least is kept")
            .valign(gtk::Align::Center)
            .active(v.min.is_some() && v.max.is_none())
            .build();
        max.add_suffix(&endless);
        if endless.is_active() {
            max.set_subtitle("No maximum");
        }
        let min = adw::SpinRow::with_range(0.0, top, step);
        min.set_digits(digits);
        min.set_title("At Least");
        min.set_subtitle("0 means no minimum");
        min.set_value(v.min.unwrap_or(0.0));
        limit.add_row(&max);
        limit.add_row(&min);
        let keep = gtk::Button::builder()
            .label("Keep It")
            .tooltip_text("Never let it drop below the number it is now")
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
                max.set_subtitle(match () {
                    _ if backwards => "Lower than At Least: the range isn't changed",
                    _ if endless.is_active() => "No maximum",
                    _ => "",
                });
                if backwards {
                    max.add_css_class("error");
                    return;
                }
                max.remove_css_class("error");
                let name = name.clone();
                worker.run_now(move |core| {
                    Event::Done(core.limit(&name, min_v, max_v).map(|_| match (min_v, max_v) {
                        (None, None) => format!("{name} is no longer limited"),
                        _ => format!("{name} is kept {}", core::limit_text(min_v, max_v)),
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
            let (current, limit, min, endless) = (current.clone(), limit.clone(), min.clone(), endless.clone());
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
        ValueWidgets { group, value, limit, confirm, confirm_button, set: set_box, keep, current, keep_it }
    }
}
