// The Browse tab (Unreal, Unity and Godot games): the player's numbers by the names the game gives them, found
// through the engine's own objects instead of a search (`Core::player_values`). Each shows its
// number live (the 1 s refresh); Only What Changes keeps the ones that move after it's turned
// on, so the player changes something in the game and sees which it was; Save keeps one like
// a found value (by name only, `Core::save_listed`). The search box takes names and numbers (the
// places holding a number now). Above the list, the engine's tweaks
// (Unreal's and RPG Maker's: god mode, fly...), each a switch (`Core::set_tweak`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;

use super::{Event, Worker};
use crate::core::{self, Belongs, Loc, PlayerObject};
use crate::i18n::{ntr, tr};
use crate::ocr::Shown;

struct Row {
    widget: adw::ActionRow,
    value: gtk::Label,
    loc: Loc,
    /// What the search box looks in: the number's name and its object's, lowercased.
    hay: String,
    group: usize,
    /// Shown only with Show Everything (`PlayerObject.more`).
    more: bool,
    /// The number when the list was made or Only What Changes was turned on.
    since: Cell<Option<f64>>,
    now: Cell<Option<f64>>,
}

pub struct BrowseView {
    pub root: gtk::Box,
    stack: gtk::Stack,
    list: gtk::Box,
    tweaks: adw::PreferencesGroup,
    tweak_rows: RefCell<Vec<adw::SwitchRow>>,
    none: adw::StatusPage,
    search: gtk::SearchEntry,
    changed: gtk::ToggleButton,
    everything: gtk::ToggleButton,
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
        Belongs::Party => tr!("Your party ({class})", class),
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
            .placeholder_text(tr!("Look for a name or a number: health, ammo, 250…"))
            .hexpand(true)
            .build();
        let changed = gtk::ToggleButton::builder()
            .label(tr!("Only What Changes"))
            .tooltip_text(tr!("Turn this on, then change the number in the game: only the numbers that moved stay in the list."))
            .build();
        // Minecraft Dungeons II lists 2377 numbers, 248 of them the player's own.
        let everything = gtk::ToggleButton::builder()
            .label(tr!("Show Everything"))
            .tooltip_text(tr!("Also the game's inner workings: each ability's settings, the game's own systems, the controller and the settings."))
            .build();
        let again = gtk::Button::builder().icon_name("view-refresh-symbolic").tooltip_text(tr!("List Again")).build();
        let tools = gtk::Box::builder().spacing(6).build();
        tools.append(&search);
        tools.append(&changed);
        tools.append(&everything);
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
        let tweaks = adw::PreferencesGroup::builder()
            .title(tr!("Tweaks"))
            .description(tr!("Changes the game's engine understands, one switch each. Save your game before you try one."))
            .visible(false)
            .build();
        list.append(&tweaks);
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
            tweaks,
            tweak_rows: RefCell::default(),
            none,
            search,
            changed,
            everything,
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
            view.everything.connect_toggled(move |_| v.filter());
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
        self.worker.run(|core| Event::Tweaks(core.tweaks()));
    }

    /// The tweaks the game's engine allows, each a switch.
    pub fn show_tweaks(self: &Rc<Self>, list: Result<Vec<core::TweakRow>, String>) {
        for r in self.tweak_rows.borrow_mut().drain(..) {
            self.tweaks.remove(&r);
        }
        let list = list.unwrap_or_default();
        let mut rows = self.tweak_rows.borrow_mut();
        for t in list {
            let Some((title, about)) = tweak_text(&t.id) else { continue };
            let subtitle = match t.waiting {
                true => tr!("{about} On, but not all of it is in the game right now: Ferret keeps it once it is.", about),
                false => about,
            };
            let row = adw::SwitchRow::builder().title(title).subtitle(subtitle).subtitle_lines(4).active(t.on).build();
            {
                let (worker, id) = (self.worker.clone(), t.id.clone());
                row.connect_active_notify(move |r| {
                    let (id, on) = (id.clone(), r.is_active());
                    worker.run_now(move |core| {
                        Event::Done(core.set_tweak(&id, on).map(|_| match on {
                            true => tr!("Tweak on: Ferret keeps it while you play"),
                            false => tr!("Tweak off: the game has its own values back"),
                        }))
                    });
                    worker.run(|core| Event::Tweaks(core.tweaks()));
                    // Its places are found by name in the background: "waiting" a moment.
                    let worker = worker.clone();
                    gtk::glib::timeout_add_local_once(std::time::Duration::from_secs(3), move || worker.run(|core| Event::Tweaks(core.tweaks())));
                });
            }
            self.tweaks.add(&row);
            rows.push(row);
        }
        self.tweaks.set_visible(!rows.is_empty());
    }

    /// Another game: its list is made when the tab is shown.
    pub fn new_game(&self) {
        self.asked.set(false);
        self.clear();
        for r in self.tweak_rows.borrow_mut().drain(..) {
            self.tweaks.remove(&r);
        }
        self.tweaks.set_visible(false);
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
                value.set_label(&f.value.map_or("?".into(), |v| super::value_text(f.loc.kind, v, None)));
                rows.push(Row {
                    widget,
                    value,
                    loc: f.loc,
                    hay: format!("{} {}", f.label, o.name).to_lowercase(),
                    group: groups.len(),
                    more: o.more,
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
        let (mut moved, mut any) = (false, false);
        for (r, &v) in rows.iter().zip(values) {
            if r.now.replace(v) == v {
                continue;
            }
            any = true;
            r.value.set_label(&v.map_or("?".into(), |v| super::value_text(r.loc.kind, v, None)));
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
        // A number searched for: the places holding it now.
        if moved && self.changed.is_active() || any && !self.query().1.is_empty() {
            self.filter();
        }
    }

    /// The D-Bus `listed` action: `list`, `search <text>`, `changed on|off`, `everything on|off`,
    /// `save <row>,<name>`.
    pub fn debug(&self, arg: &str) {
        let (what, rest) = arg.split_once(' ').unwrap_or((arg, ""));
        match what {
            "search" => self.search.set_text(rest),
            "changed" => self.changed.set_active(rest == "on"),
            "everything" => self.everything.set_active(rest == "on"),
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

    /// The search box's words: parts of names, and numbers to find among the values (the name
    /// may mean nothing, while the game shows the number).
    fn query(&self) -> (Vec<String>, Vec<Shown>) {
        let (mut words, mut numbers) = (Vec::new(), Vec::new());
        for w in self.search.text().to_lowercase().split_whitespace() {
            match Shown::parse(w) {
                Some(n) => numbers.push(n),
                None => words.push(w.to_owned()),
            }
        }
        (words, numbers)
    }

    fn filter(&self) {
        let (words, numbers) = self.query();
        let only_changed = self.changed.is_active();
        let everything = self.everything.is_active();
        let rows = self.rows.borrow();
        let groups = self.groups.borrow();
        let mut shown_in = vec![false; groups.len()];
        let (mut shown, mut held_back) = (0, 0);
        for r in rows.iter() {
            let show = words.iter().all(|w| r.hay.contains(w.as_str()))
                && numbers.iter().all(|n| holds(r.loc.kind, r.now.get(), n))
                && (!only_changed || r.now.get() != r.since.get());
            // A number looks everywhere: the place holding it may be among the inner workings.
            if show && r.more && !everything && numbers.is_empty() {
                held_back += 1;
                r.widget.set_visible(false);
                continue;
            }
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
            (0, true) if held_back == 0 => tr!("Nothing changed yet: change the number in the game."),
            (0, false) if held_back == 0 && !numbers.is_empty() => tr!("Nothing listed holds that number now."),
            (0, false) if held_back == 0 => tr!("No names match."),
            (n, _) if held_back > 0 => ntr!(
                "{n} number shown, {more} more with Show Everything",
                "{n} numbers shown, {more} more with Show Everything",
                n,
                n = super::find::grouped(n),
                more = super::find::grouped(held_back)
            ),
            (n, _) => ntr!("{n} number", "{n} numbers", n, n = super::find::grouped(n)),
        });
    }
}

/// A listed number the game would show as `n`: whole numbers equal, decimals within one step
/// of its last digit (87 for a health of 87.4 or 86.6), as searches take them. Flags aren't
/// numbers.
fn holds(kind: core::Kind, v: Option<f64>, n: &Shown) -> bool {
    let step = 10f64.powi(-(n.decimals() as i32));
    !matches!(kind, core::Kind::Bit(_) | core::Kind::JsBool) && v.is_some_and(|v| (v - n.value()).abs() < step)
}

/// A tweak's title and what it does (helper/src/tweaks.rs ids); None for one this build doesn't
/// know.
fn tweak_text(id: &str) -> Option<(String, String)> {
    Some(match id {
        "godmode" => (
            tr!("God Mode"),
            tr!("Your character can't be damaged the engine's way, and its health stays at the most it has had since you turned this on. Games with damage rules of their own may ignore the first part."),
        ),
        "fly" => (
            tr!("Fly"),
            tr!("Your character flies instead of walking or falling. Going up and down depends on the game's controls; turned off, the character falls back down."),
        ),
        "heroes" => (
            tr!("God Mode"),
            tr!("Your heroes' HP and MP don't go down: they stay at the most they've had since you turned this on, so heal once to keep them full. One hit bigger than a hero's HP can still knock them out."),
        ),
        "walls" => (
            tr!("Walk Through Walls"),
            tr!("You walk through walls, people and water (not off the map). Places the story hasn't opened yet open too."),
        ),
        "battles" => (tr!("No Random Battles"), tr!("Walking around starts no random battles. Battles the story starts still happen.")),
        "speed" => (tr!("Walk Faster"), tr!("You walk twice as fast.")),
        _ => return None,
    })
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
        Event::BrowseSaved(
            core.save_listed(&name, loc).map(|_| (name.clone(), tr!("Saved {name}. Ferret finds it again every time you attach.", name), loc.kind.flag())),
        )
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

    #[test]
    fn numbers_found_as_the_game_shows_them() {
        let n = |t: &str| Shown::parse(t).unwrap();
        assert!(holds(core::Kind::I32, Some(250.0), &n("250")));
        assert!(!holds(core::Kind::I32, Some(251.0), &n("250")));
        assert!(holds(core::Kind::F32, Some(87.4), &n("87")));
        assert!(holds(core::Kind::F32, Some(86.6), &n("87")));
        assert!(!holds(core::Kind::F32, Some(88.0), &n("87")));
        assert!(holds(core::Kind::F32, Some(1.25), &n("1.2")));
        assert!(!holds(core::Kind::F32, Some(1.35), &n("1.2")));
        assert!(holds(core::Kind::I32, Some(197.0), &n("3:17")));
        assert!(!holds(core::Kind::Bit(1), Some(1.0), &n("1")));
        assert!(!holds(core::Kind::I32, None, &n("0")));
    }
}
