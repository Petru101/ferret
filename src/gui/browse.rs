// The Browse tab (Unreal games): the player's numbers by the names the game gives them, found
// through the engine's own objects instead of a search (`Core::player_values`). Each shows its
// number live (the 1 s refresh); Only What Changes keeps the ones that move after it's turned
// on, so the player changes something in the game and sees which it was; Save keeps one like
// a found value (by name only, `Core::save_listed`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use super::{Event, Worker};
use crate::core::{self, Belongs, Loc, PlayerObject};
use crate::i18n::{ntr, tr};

struct Row {
    widget: adw::ActionRow,
    value: gtk::Label,
    loc: Loc,
    /// What the search box looks in: the number's name and its object's, lowercased.
    hay: String,
    group: usize,
    /// The number when the list was made or Only What Changes was turned on.
    since: Cell<Option<f64>>,
    now: Cell<Option<f64>>,
}

pub struct BrowseView {
    pub root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::Box,
    none: adw::StatusPage,
    search: gtk::SearchEntry,
    changed: gtk::ToggleButton,
    count: gtk::Label,
    rows: RefCell<Vec<Row>>,
    groups: RefCell<Vec<adw::PreferencesGroup>>,
    worker: Worker,
    /// A list was asked for since the game was opened (the tab lists when first shown).
    asked: Cell<bool>,
}

fn belongs_text(b: &Belongs, class: &str) -> String {
    match b {
        Belongs::Character => tr!("Your character ({class})", class),
        Belongs::State => tr!("Your player's state ({class})", class),
        Belongs::Controller => tr!("Your controller ({class})", class),
        Belongs::Game => tr!("The game ({class})", class),
        Belongs::Owned(owner) => tr!("Belongs to {owner} ({class})", owner, class),
        Belongs::Part(owner) => tr!("Part of {owner} ({class})", owner, class),
        Belongs::Linked(owner) => tr!("Used by {owner} ({class})", owner, class),
    }
}

/// A name to save a number under: its own name without the objects and keys before it
/// ("Slots[Item=Arrow].Count" -> "count", "Health" -> "health").
fn suggested_name(label: &str) -> String {
    let last = label.rsplit('.').next().unwrap_or(label);
    let bare = last.split(['[', '{', ' ']).next().unwrap_or(last);
    core::one_word(bare)
}

impl BrowseView {
    pub fn new(worker: Worker) -> Rc<Self> {
        let search = gtk::SearchEntry::builder()
            .placeholder_text(tr!("Look for a name: health, ammo, gold…"))
            .hexpand(true)
            .build();
        let changed = gtk::ToggleButton::builder()
            .label(tr!("Only What Changes"))
            .tooltip_text(tr!("Turn this on, then change the number in the game: only the numbers that moved stay in the list."))
            .build();
        let again = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text(tr!("List Again")).build();
        let tools = gtk::Box::builder().spacing(6).build();
        tools.append(&search);
        tools.append(&changed);
        tools.append(&again);
        let hint = gtk::Label::builder()
            .label(tr!(
                "Your character's numbers, by the names the game gives them. Can't tell which one it is? Turn on Only What Changes and change it in the game."
            ))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        let count = gtk::Label::builder().xalign(0.0).css_classes(["caption", "dim-label"]).build();
        let top = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        top.append(&tools);
        top.append(&hint);
        top.append(&count);

        let list = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(24)
            .margin_top(12)
            .margin_bottom(24)
            .margin_start(12)
            .margin_end(12)
            .build();
        let scroll = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(&adw::Clamp::builder().maximum_size(720).child(&list).build())
            .build();
        let loading = adw::StatusPage::builder().title(tr!("Reading the game's objects…")).build();
        loading.set_paintable(Some(&adw::SpinnerPaintable::new(Some(&loading))));
        let try_again = gtk::Button::builder()
            .label(tr!("Try Again"))
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let none = adw::StatusPage::builder().icon_name("edit-find-symbolic").title(tr!("Nothing to List Yet")).child(&try_again).build();
        let stack = gtk::Stack::builder().vexpand(true).build();
        stack.add_named(&loading, Some("loading"));
        stack.add_named(&none, Some("none"));
        stack.add_named(&scroll, Some("list"));
        let root = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
        root.append(&top);
        root.append(&stack);

        let view = Rc::new(Self {
            root,
            stack,
            list,
            none,
            search,
            changed,
            count,
            rows: RefCell::default(),
            groups: RefCell::default(),
            worker,
            asked: Cell::new(false),
        });
        for button in [&again, &try_again] {
            let v = view.clone();
            button.connect_clicked(move |_| v.load());
        }
        {
            let v = view.clone();
            view.search.connect_search_changed(move |_| v.filter());
        }
        {
            let v = view.clone();
            view.changed.connect_toggled(move |b| {
                // From now on: what moves after this.
                if b.is_active() {
                    for r in v.rows.borrow().iter() {
                        r.since.set(r.now.get());
                        r.value.remove_css_class("accent");
                    }
                }
                v.filter();
            });
        }
        view
    }

    /// Lists the numbers the first time the tab is shown for a game.
    pub fn shown(&self) {
        if !self.asked.get() {
            self.load();
        }
    }

    pub fn load(&self) {
        self.asked.set(true);
        self.stack.set_visible_child_name("loading");
        self.worker.run(|core| Event::Player(core.player_values()));
    }

    /// Another game: its list is made when the tab is shown.
    pub fn new_game(&self) {
        self.asked.set(false);
        self.clear();
        self.stack.set_visible_child_name("loading");
    }

    fn clear(&self) {
        for g in self.groups.borrow_mut().drain(..) {
            self.list.remove(&g);
        }
        self.rows.borrow_mut().clear();
        self.count.set_label("");
    }

    pub fn show(self: &Rc<Self>, list: Result<Vec<PlayerObject>, String>) {
        self.clear();
        let list = match list {
            Ok(l) if !l.is_empty() => l,
            other => {
                let why = other.err().unwrap_or_else(|| tr!("Ferret found no numbers in your character's objects."));
                self.none.set_description(Some(&glib_escape(&why)));
                self.stack.set_visible_child_name("none");
                return;
            }
        };
        let mut rows = self.rows.borrow_mut();
        let mut groups = self.groups.borrow_mut();
        for o in list {
            let group = adw::PreferencesGroup::builder().title(glib_escape(&o.name)).description(glib_escape(&belongs_text(&o.belongs, &o.class))).build();
            for f in o.fields {
                let value = gtk::Label::builder()
                    .css_classes(["numeric"])
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(14)
                    .build();
                let save = gtk::Button::builder().label(tr!("Save")).valign(gtk::Align::Center).css_classes(["flat"]).build();
                value.set_tooltip_text(Some(&format!("0x{:x}, {}", f.loc.addr, super::kind_text(f.loc.kind))));
                let widget = adw::ActionRow::builder().title(&f.label).use_markup(false).build();
                widget.add_suffix(&value);
                widget.add_suffix(&save);
                group.add(&widget);
                {
                    let (worker, label, loc) = (self.worker.clone(), f.label.clone(), f.loc);
                    save.connect_clicked(move |b| ask_save(&worker, &label, loc, b));
                }
                value.set_label(&f.value.map_or("?".into(), |v| core::number_text(v, None)));
                rows.push(Row {
                    widget,
                    value,
                    loc: f.loc,
                    hay: format!("{} {}", f.label, o.name).to_lowercase(),
                    group: groups.len(),
                    since: Cell::new(f.value),
                    now: Cell::new(f.value),
                });
            }
            self.list.append(&group);
            groups.push(group);
        }
        drop((rows, groups));
        self.stack.set_visible_child_name("list");
        self.filter();
    }

    /// Where the listed numbers are, for the 1 s refresh (none while the list isn't shown).
    pub fn locs(&self) -> Vec<Loc> {
        if self.stack.visible_child_name().as_deref() != Some("list") {
            return Vec::new();
        }
        self.rows.borrow().iter().map(|r| r.loc).collect()
    }

    /// The numbers now, in the order `locs` gave.
    pub fn update(&self, values: &[Option<f64>]) {
        let rows = self.rows.borrow();
        if values.len() != rows.len() {
            return;
        }
        let mut moved = false;
        for (r, &v) in rows.iter().zip(values) {
            if r.now.replace(v) == v {
                continue;
            }
            r.value.set_label(&v.map_or("?".into(), |v| core::number_text(v, None)));
            let changed = v != r.since.get();
            if changed != r.value.has_css_class("accent") {
                moved = true;
                if changed {
                    r.value.add_css_class("accent");
                } else {
                    r.value.remove_css_class("accent");
                }
            }
        }
        drop(rows);
        if moved && self.changed.is_active() {
            self.filter();
        }
    }

    /// The D-Bus `listed` action: `list`, `search <text>`, `changed on|off`, `save <row>,<name>`.
    pub fn debug(&self, arg: &str) {
        let (what, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        match what {
            "search" => self.search.set_text(rest),
            "changed" => self.changed.set_active(rest == "on"),
            "save" => {
                let Some((i, name)) = rest.split_once(',') else { return };
                let loc = i.trim().parse::<usize>().ok().and_then(|i| self.rows.borrow().get(i).map(|r| r.loc));
                if let Some(loc) = loc {
                    save(&self.worker, name.trim(), loc);
                }
            }
            _ => self.load(),
        }
    }

    fn filter(&self) {
        let words: Vec<String> = self.search.text().to_lowercase().split_whitespace().map(str::to_owned).collect();
        let only_changed = self.changed.is_active();
        let rows = self.rows.borrow();
        let groups = self.groups.borrow();
        let mut shown_in = vec![false; groups.len()];
        let mut shown = 0;
        for r in rows.iter() {
            let show = words.iter().all(|w| r.hay.contains(w.as_str())) && (!only_changed || r.now.get() != r.since.get());
            r.widget.set_visible(show);
            if show {
                shown_in[r.group] = true;
                shown += 1;
            }
        }
        for (g, s) in groups.iter().zip(shown_in) {
            g.set_visible(s);
        }
        self.count.set_label(&match (shown, only_changed) {
            (0, true) => tr!("Nothing changed yet: change the number in the game."),
            (0, false) => tr!("No names match."),
            (n, _) => ntr!("{n} number", "{n} numbers", n),
        });
    }
}

/// libadwaita's group titles and descriptions are markup: the game's names are text.
fn glib_escape(text: &str) -> String {
    gtk::glib::markup_escape_text(text).to_string()
}

/// Asks what to save a listed number as, then saves it by name.
fn ask_save(worker: &Worker, label: &str, loc: Loc, parent: &impl IsA<gtk::Widget>) {
    let suggested = suggested_name(label);
    let dialog = adw::AlertDialog::new(Some(&tr!("Save {name}", name = label)), Some(&tr!("Ferret finds it again by this name every time you open the game.")));
    let entry = gtk::Entry::builder().placeholder_text(&suggested).activates_default(true).max_length(core::MAX_NAME as i32).build();
    dialog.set_extra_child(Some(&entry));
    dialog.add_responses(&[("cancel", &tr!("Cancel")), ("save", &tr!("Save"))]);
    dialog.set_response_appearance("save", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("save"));
    dialog.set_close_response("cancel");
    {
        let (worker, entry) = (worker.clone(), entry.clone());
        dialog.connect_response(Some("save"), move |_, _| {
            let typed = core::one_word(&entry.text());
            let name = if typed.is_empty() { suggested.clone() } else { typed };
            save(&worker, &name, loc);
        });
    }
    dialog.set_focus(Some(&entry));
    dialog.present(Some(parent));
}

/// Saves a listed number (the Save dialog, the D-Bus `listed save` action).
pub fn save(worker: &Worker, name: &str, loc: Loc) {
    let name = name.to_owned();
    worker.run(move |core| {
        Event::BrowseSaved(core.save_listed(&name, loc).map(|_| (name.clone(), tr!("Saved {name}. Ferret finds it again every time you attach.", name))))
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_names() {
        assert_eq!(suggested_name("Health"), "health");
        assert_eq!(suggested_name("Slots[Item=Bow Arrow].Count"), "count");
        assert_eq!(suggested_name("Ammo #2"), "ammo");
        assert_eq!(suggested_name("Currencies{Gold}"), "currencies");
    }
}
