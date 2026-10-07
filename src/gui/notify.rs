// Desktop notifications for what the player needs to know while the game is in front ("Your
// turn", "Found it!", ...), only while Ferret's window isn't. Sent straight to the notification
// server with critical urgency: while a fullscreen window is focused Plasma turns on Do Not
// Disturb and shows only critical ones (over the game, without taking focus); the portal can't
// send those (its "urgent" came out as a normal popup on KDE). One at a time: each replaces the
// last, and Ferret closes it itself (Plasma keeps critical ones until dismissed).

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use gtk::prelude::*;
use gtk::{gio, glib};

const SERVER: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

pub struct Notifier {
    bus: Option<gio::DBusConnection>,
    window: RefCell<Option<gtk::Window>>,
    /// The notification on screen (0: none).
    id: Cell<u32>,
    /// It waits for the player (no timer): gone once the card shows something else.
    waiting: Cell<bool>,
    /// Counts what was sent: a timer or a late reply only acts for the one it belongs to.
    sent: Cell<u32>,
    /// What the notification's button does (Stop Waiting, Cancel).
    on_button: RefCell<Option<Rc<dyn Fn()>>>,
    /// The activation token the server sends before a click, to bring the window to the front.
    token: RefCell<Option<String>>,
    signals: RefCell<Vec<gio::SignalSubscription>>,
}

/// Notifications and focus changes go to the log file too, to see afterwards what reached the
/// player (the core's log is on its worker thread).
fn log(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::core::cache_dir().join("ferret.log")) {
        let _ = writeln!(f, "{line}");
    }
}

impl Notifier {
    pub fn new() -> Rc<Self> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE)
            .inspect_err(|e| eprintln!("notifications: {e}"))
            .ok();
        let n = Rc::new(Self {
            bus,
            window: RefCell::default(),
            id: Cell::new(0),
            waiting: Cell::new(false),
            sent: Cell::new(0),
            on_button: RefCell::default(),
            token: RefCell::default(),
            signals: RefCell::default(),
        });
        n.listen();
        n
    }

    /// The window: notifications only go out while it isn't in front, and leave when it comes.
    pub fn set_window(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let n = Rc::downgrade(self);
        // Only logged when it closes a notification: every switch to and from the game filled
        // half the log (and bug reports) with "in front" / "left the front".
        window.connect_is_active_notify(move |w| {
            if let (true, Some(n)) = (w.is_active(), n.upgrade()) {
                if n.id.get() != 0 {
                    log("Ferret's window came to the front: its notification closed");
                }
                n.withdraw();
            }
        });
        self.window.replace(Some(window.clone().upcast()));
    }

    pub fn on_button(&self, f: impl Fn() + 'static) {
        self.on_button.replace(Some(Rc::new(f)));
    }

    fn listen(self: &Rc<Self>) {
        let Some(bus) = &self.bus else { return };
        let subscribe = |member: &str, f: Box<dyn Fn(&Self, u32, glib::Variant)>| {
            let n = Rc::downgrade(self);
            bus.subscribe_to_signal(Some(SERVER), Some(SERVER), Some(member), Some(PATH), None, gio::DBusSignalFlags::NONE, move |s| {
                let (Some(n), Some(id)) = (n.upgrade(), s.parameters.try_child_value(0).and_then(|v| v.get::<u32>())) else { return };
                if id != 0 && id == n.id.get() {
                    f(&n, id, s.parameters.clone());
                }
            })
        };
        let signals = vec![
            subscribe(
                "ActivationToken",
                Box::new(|n, _, p| {
                    n.token.replace(p.try_child_value(1).and_then(|v| v.get::<String>()));
                }),
            ),
            subscribe(
                "ActionInvoked",
                Box::new(|n, _, p| {
                    let action = p.try_child_value(1).and_then(|v| v.get::<String>()).unwrap_or_default();
                    n.forget();
                    if action == "button" {
                        let f = n.on_button.borrow().clone();
                        if let Some(f) = f {
                            f();
                        }
                        return;
                    }
                    if let Some(w) = n.window.borrow().as_ref() {
                        if let Some(token) = n.token.take() {
                            w.set_startup_id(&token);
                        }
                        w.present();
                    }
                }),
            ),
            subscribe("NotificationClosed", Box::new(|n, _, _| n.forget())),
        ];
        self.signals.replace(signals);
    }

    /// The notification is gone (clicked, dismissed, closed): later ones start a new one.
    fn forget(&self) {
        self.id.set(0);
        self.waiting.set(false);
        self.sent.set(self.sent.get() + 1);
    }

    /// Ferret's window isn't in front (the player is in the game).
    pub fn away(&self) -> bool {
        self.window.borrow().as_ref().is_some_and(|w| !w.is_active())
    }

    /// Tells the player, unless Ferret's window is in front. `button`: a button that does
    /// `on_button`. `secs`: how long it stays; none = until it's replaced or withdrawn.
    pub fn send(self: &Rc<Self>, title: &str, body: &str, button: Option<&str>, secs: Option<u32>) {
        let Some(bus) = &self.bus else { return };
        if !self.away() {
            return;
        }
        log(&format!("notification: {title}"));
        let sent = self.sent.get() + 1;
        self.sent.set(sent);
        self.waiting.set(secs.is_none());
        let mut actions = vec!["default".to_owned(), "Show Ferret".to_owned()];
        if let Some(label) = button {
            actions.extend(["button".to_owned(), label.to_owned()]);
        }
        let hints = glib::VariantDict::new(None);
        hints.insert("urgency", 2u8);
        hints.insert("desktop-entry", super::APP_ID);
        let args = glib::Variant::tuple_from_iter([
            "Ferret".to_variant(),
            self.id.get().to_variant(),
            super::APP_ID.to_variant(),
            title.to_variant(),
            body.to_variant(),
            actions.to_variant(),
            hints.end(),
            secs.map_or(0, |s| s as i32 * 1000).to_variant(),
        ]);
        let n = Rc::downgrade(self);
        bus.call(Some(SERVER), PATH, SERVER, "Notify", Some(&args), None, gio::DBusCallFlags::NONE, -1, gio::Cancellable::NONE, move |r| {
            let Some(n) = n.upgrade() else { return };
            let id = match r.map(|v| v.get::<(u32,)>()) {
                Ok(Some((id,))) => id,
                Ok(None) => return,
                Err(e) => return log(&format!("notification failed: {e}")),
            };
            // Withdrawn or replaced while the server answered.
            if n.sent.get() != sent {
                return n.close(id);
            }
            n.id.set(id);
            if let Some(secs) = secs {
                let weak = Rc::downgrade(&n);
                glib::timeout_add_local_once(Duration::from_secs(secs as u64), move || {
                    if let Some(n) = weak.upgrade().filter(|n| n.sent.get() == sent) {
                        n.withdraw();
                    }
                });
            }
        });
    }

    /// Closes the one on screen.
    pub fn withdraw(&self) {
        let id = self.id.get();
        self.forget();
        if id != 0 {
            self.close(id);
        }
    }

    /// Closes a waiting one ("Your turn"): what it waited for is over. Unless another is sent
    /// right after ("Found it!"), which replaces it in place.
    pub fn withdraw_waiting(self: &Rc<Self>) {
        if !self.waiting.get() {
            return;
        }
        let (n, sent) = (Rc::downgrade(self), self.sent.get());
        glib::idle_add_local_once(move || {
            if let Some(n) = n.upgrade().filter(|n| n.sent.get() == sent && n.waiting.get()) {
                n.withdraw();
            }
        });
    }

    fn close(&self, id: u32) {
        let Some(bus) = &self.bus else { return };
        bus.call(
            Some(SERVER),
            PATH,
            SERVER,
            "CloseNotification",
            Some(&(id,).to_variant()),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |_| {},
        );
    }
}
