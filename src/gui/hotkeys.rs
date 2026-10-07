// Global hotkeys through the GlobalShortcuts portal (KDE, GNOME 48+): turning limits off and on,
// starting or stopping the search and Scan Again without leaving the game (Prey pauses while
// it isn't in front). On by default (the user's call): Ferret binds them at start, the desktop
// asks the player to accept them the first time and keeps them for later runs (Set Up Hotkeys
// asks again after a refusal); a portal session binds its shortcuts only once, so the set is
// fixed. What a hotkey did is told by a notification (`notify.rs`).

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::i18n::{gettext, n_, tr};

const PORTAL: &str = "org.freedesktop.portal.Desktop";
const DESKTOP: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

/// Id, what it does, the trigger asked for (the desktop may give another, or none).
const KEYS: [(&str, &str, &str); 3] = [
    ("limits", n_!("Turn limits off or on"), "CTRL+SHIFT+F9"),
    ("search", n_!("Start or stop the search"), "CTRL+SHIFT+F10"),
    ("again", n_!("Scan again (the number stayed the same)"), "CTRL+SHIFT+F11"),
];

pub struct Hotkeys {
    pub button: gtk::MenuButton,
    bus: Option<gio::DBusConnection>,
    session: RefCell<Option<String>>,
    /// The keys the desktop gave each hotkey, by id ("" = none).
    bound: RefCell<Vec<(String, String)>>,
    /// Bound in this session (Activated only comes then).
    active: Cell<bool>,
    requests: Cell<u32>,
    on_key: RefCell<Option<Rc<dyn Fn(&str)>>>,
    keys: Vec<gtk::Label>,
    note: gtk::Label,
    setup: gtk::Button,
    signals: RefCell<Vec<gio::SignalSubscription>>,
}

impl Hotkeys {
    pub fn new() -> Rc<Self> {
        let grid = gtk::Grid::builder().row_spacing(8).column_spacing(18).build();
        let keys: Vec<gtk::Label> = KEYS
            .iter()
            .enumerate()
            .map(|(i, (_, what, _))| {
                grid.attach(&gtk::Label::builder().label(gettext(what)).xalign(0.0).build(), 0, i as i32, 1, 1);
                let key = gtk::Label::builder().xalign(1.0).hexpand(true).css_classes(["dim-label", "monospace"]).build();
                grid.attach(&key, 1, i as i32, 1, 1);
                key
            })
            .collect();
        let note = gtk::Label::builder().wrap(true).max_width_chars(40).xalign(0.0).css_classes(["dim-label"]).build();
        let setup = gtk::Button::builder().halign(gtk::Align::End).css_classes(["suggested-action"]).build();
        let content = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(12)
            .margin_end(12)
            .build();
        content.append(&gtk::Label::builder().label(tr!("Hotkeys")).xalign(0.0).css_classes(["heading"]).build());
        content.append(&grid);
        content.append(&note);
        content.append(&setup);
        let button = gtk::MenuButton::builder()
            .icon_name("input-keyboard-symbolic")
            .tooltip_text(tr!("Hotkeys"))
            .popover(&gtk::Popover::builder().child(&content).build())
            .build();
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok();
        let h = Rc::new(Self {
            button,
            bus,
            session: RefCell::default(),
            bound: RefCell::default(),
            active: Cell::new(false),
            requests: Cell::new(0),
            on_key: RefCell::default(),
            keys,
            note,
            setup,
            signals: RefCell::default(),
        });
        {
            let h2 = Rc::downgrade(&h);
            h.setup.connect_clicked(move |_| {
                if let Some(h) = h2.upgrade() {
                    match h.active.get() {
                        true => h.configure(),
                        false => h.bind(),
                    }
                }
            });
        }
        h.show_state(None);
        h.listen();
        h.start();
        h
    }

    pub fn on_key(&self, f: impl Fn(&str) + 'static) {
        self.on_key.replace(Some(Rc::new(f)));
    }

    /// Fills the popover in. `error`: the portal can't give hotkeys.
    fn show_state(&self, error: Option<&str>) {
        let bound = self.bound.borrow();
        for ((id, _, _), label) in KEYS.iter().zip(&self.keys) {
            let key = bound.iter().find(|(b, _)| b == id).map(|(_, k)| k.as_str());
            label.set_label(&match key {
                Some("") => tr!("None"),
                Some(k) => k.to_owned(),
                None => String::new(),
            });
        }
        let (note, setup) = match (error, self.active.get()) {
            (Some(_), _) => (tr!("This desktop doesn't offer hotkeys to apps."), None),
            (None, true) => (tr!("They work while the game is in front. A notification says what they did."), Some(tr!("Change Hotkeys…"))),
            (None, false) => (
                tr!("Turn limits off or on, and search, without leaving the game. They aren't set up: your desktop asks you to accept them."),
                Some(tr!("Set Up Hotkeys…")),
            ),
        };
        if let Some(e) = error {
            eprintln!("hotkeys: {e}");
        }
        self.note.set_label(&note);
        self.setup.set_visible(setup.is_some());
        self.setup.set_label(&setup.unwrap_or_default());
    }

    fn listen(self: &Rc<Self>) {
        let Some(bus) = &self.bus else { return };
        let h = Rc::downgrade(self);
        let activated = bus.subscribe_to_signal(Some(PORTAL), Some(IFACE), Some("Activated"), Some(DESKTOP), None, gio::DBusSignalFlags::NONE, move |s| {
            let Some(h) = h.upgrade() else { return };
            let (session, id) = (s.parameters.try_child_value(0), s.parameters.try_child_value(1).and_then(|v| v.get::<String>()));
            if session.and_then(|v| v.str().map(str::to_owned)) != *h.session.borrow() {
                return;
            }
            let f = h.on_key.borrow().clone();
            if let (Some(f), Some(id)) = (f, id) {
                eprintln!("hotkey: {id}");
                f(&id);
            }
        });
        let h = Rc::downgrade(self);
        let changed = bus.subscribe_to_signal(Some(PORTAL), Some(IFACE), Some("ShortcutsChanged"), Some(DESKTOP), None, gio::DBusSignalFlags::NONE, move |s| {
            let Some(h) = h.upgrade() else { return };
            if let Some(list) = s.parameters.try_child_value(1) {
                h.bound.replace(shortcuts(&list));
                h.show_state(None);
            }
        });
        self.signals.replace(vec![activated, changed]);
    }

    /// A portal call answered through a Request object: `args` gets the handle token.
    fn request(self: &Rc<Self>, method: &str, args: impl FnOnce(&str) -> glib::Variant, done: impl FnOnce(&Rc<Self>, u32, glib::VariantDict) + 'static) {
        let Some(bus) = &self.bus else { return self.show_state(Some("no session bus")) };
        self.requests.set(self.requests.get() + 1);
        let token = format!("ferret_hotkeys{}", self.requests.get());
        let sender = bus.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
        let path = format!("{DESKTOP}/request/{sender}/{token}");
        let answer: Rc<RefCell<Option<gio::SignalSubscription>>> = Rc::default();
        let (h, slot, done) = (Rc::downgrade(self), answer.clone(), RefCell::new(Some(done)));
        let sub = bus.subscribe_to_signal(Some(PORTAL), Some("org.freedesktop.portal.Request"), Some("Response"), Some(&path), None, gio::DBusSignalFlags::NONE, move |s| {
            slot.take();
            let (Some(h), Some(done)) = (h.upgrade(), done.take()) else { return };
            let (code, results) = s.parameters.get::<(u32, glib::VariantDict)>().unwrap_or((2, glib::VariantDict::new(None)));
            done(&h, code, results);
        });
        answer.replace(Some(sub));
        let (h, method_name) = (Rc::downgrade(self), method.to_owned());
        bus.call(Some(PORTAL), DESKTOP, IFACE, method, Some(&args(&token)), None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, move |r| {
            if let Err(e) = r {
                answer.take();
                if let Some(h) = h.upgrade() {
                    h.show_state(Some(&format!("{method_name}: {e}")));
                }
            }
        });
    }

    fn session_path(&self) -> Option<glib::Variant> {
        let s = self.session.borrow().clone()?;
        glib::variant::ObjectPath::try_from(s).ok().map(|p| p.to_variant())
    }

    /// A session, then the hotkeys (accepted in an earlier run: bound again without asking).
    fn start(self: &Rc<Self>) {
        self.request(
            "CreateSession",
            |token| {
                let options = glib::VariantDict::new(None);
                options.insert("handle_token", token);
                options.insert("session_handle_token", "ferret");
                glib::Variant::tuple_from_iter([options.end()])
            },
            |h, code, results| {
                let Some(session) = results.lookup::<String>("session_handle").ok().flatten().filter(|_| code == 0) else {
                    return h.show_state(Some(&format!("CreateSession answered {code}")));
                };
                h.session.replace(Some(session));
                let Some(session) = h.session_path() else { return };
                h.request(
                    "ListShortcuts",
                    |token| {
                        let options = glib::VariantDict::new(None);
                        options.insert("handle_token", token);
                        glib::Variant::tuple_from_iter([session, options.end()])
                    },
                    |h, _, results| {
                        let earlier = results.lookup_value("shortcuts", None).map(|l| shortcuts(&l)).unwrap_or_default();
                        eprintln!("hotkeys: session ready, accepted earlier: {earlier:?}");
                        if !earlier.is_empty() {
                            h.bound.replace(earlier);
                        }
                        h.show_state(None);
                        h.bind();
                    },
                );
            },
        );
    }

    /// Asks for the hotkeys (the desktop shows its dialog the first time).
    fn bind(self: &Rc<Self>) {
        let Some(session) = self.session_path() else { return };
        let list = KEYS.iter().map(|(id, what, key)| {
            let d = glib::VariantDict::new(None);
            d.insert("description", gettext(what));
            d.insert("preferred_trigger", *key);
            glib::Variant::tuple_from_iter([id.to_variant(), d.end()])
        });
        let list = glib::Variant::array_from_iter_with_type(glib::VariantTy::new("(sa{sv})").unwrap(), list);
        self.request(
            "BindShortcuts",
            |token| {
                let options = glib::VariantDict::new(None);
                options.insert("handle_token", token);
                glib::Variant::tuple_from_iter([session, list, "".to_variant(), options.end()])
            },
            |h, code, results| {
                if code == 0 {
                    h.bound.replace(results.lookup_value("shortcuts", None).map(|l| shortcuts(&l)).unwrap_or_default());
                    h.active.set(true);
                }
                eprintln!("hotkeys: bound ({code}): {:?}", h.bound.borrow());
                h.show_state(None);
            },
        );
    }

    /// The desktop's page for changing them.
    fn configure(&self) {
        let (Some(bus), Some(session)) = (&self.bus, self.session_path()) else { return };
        let args = glib::Variant::tuple_from_iter([session, "".to_variant(), glib::VariantDict::new(None).end()]);
        bus.call(Some(PORTAL), DESKTOP, IFACE, "ConfigureShortcuts", Some(&args), None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, |r| {
            if let Err(e) = r {
                eprintln!("hotkeys: ConfigureShortcuts: {e}");
            }
        });
        self.button.popdown();
    }
}

/// Id and keys of each shortcut in a portal's `a(sa{sv})` list.
fn shortcuts(list: &glib::Variant) -> Vec<(String, String)> {
    list.iter()
        .filter_map(|s| {
            let id = s.try_child_value(0)?.get::<String>()?;
            let props = glib::VariantDict::new(s.try_child_value(1).as_ref());
            Some((id, props.lookup::<String>("trigger_description").ok().flatten().unwrap_or_default()))
        })
        .collect()
}
