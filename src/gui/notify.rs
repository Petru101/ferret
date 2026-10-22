// Desktop notifications for what the player needs to know while the game is in front ("Your
// turn", "Found it!", ...), only while Ferret's window isn't. Sent straight to the notification
// server with critical urgency: while a fullscreen window is focused Plasma turns on Do Not
// Disturb and shows only critical ones (over the game, without taking focus); the portal can't
// send those (its "urgent" came out as a normal popup on KDE). One at a time: each replaces the
// last, and Ferret closes it itself (Plasma keeps critical ones until dismissed). On Windows
// they are Windows' own notifications (toasts, `toast` below), the same way: one at a time,
// with the same buttons.

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
    /// What the button of the one on screen does instead (`send_action`).
    this_button: RefCell<Option<Rc<dyn Fn()>>>,
    /// The activation token the server sends before a click, to bring the window to the front.
    token: RefCell<Option<String>>,
    signals: RefCell<Vec<gio::SignalSubscription>>,
    #[cfg(windows)]
    toasts: Option<toast::Toasts>,
}

/// Notifications and focus changes go to the log file too, to see afterwards what reached the
/// player (the core's log is on its worker thread).
fn log(line: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(crate::core::cache_dir().join("ferret.log")) {
        let _ = writeln!(f, "{line}");
    }
}

/// FERRET_FOCUS_LOG=1: every focus change of the window and the focus when a notification goes
/// out, timed, to find why Windows got notifications while Ferret was in front.
fn focus_log() -> bool {
    std::env::var_os("FERRET_FOCUS_LOG").is_some()
}

/// Whether the window Windows has in front is one of Ferret's.
#[cfg(windows)]
fn foreground_is_ours() -> bool {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(GetForegroundWindow(), Some(&mut pid)) };
    pid == std::process::id()
}

/// The time, the window's focus as GTK and Windows see it, and Ferret's other windows.
fn focus_state(window: &gtk::Window) -> String {
    let time = glib::DateTime::now_local().ok().and_then(|d| d.format("%H:%M:%S.%f").ok()).map_or_else(String::new, |t| t.get(..12).unwrap_or(&t).to_owned());
    #[cfg(windows)]
    let front = format!(" in_front={}", foreground_is_ours());
    #[cfg(not(windows))]
    let front = String::new();
    let others: Vec<String> = gtk::Window::list_toplevels()
        .iter()
        .filter_map(|w| w.downcast_ref::<gtk::Window>())
        .filter(|w| *w != window)
        .map(|w| format!("{}{}{}", w.type_().name(), if w.is_visible() { "" } else { "(hidden)" }, if w.is_active() { "(ACTIVE)" } else { "" }))
        .collect();
    format!("{time} active={}{front} others=[{}]", window.is_active(), others.join(" "))
}

impl Notifier {
    pub fn new() -> Rc<Self> {
        #[cfg(windows)]
        let bus = None;
        #[cfg(windows)]
        let (answers, toasts) = {
            let (tx, rx) = async_channel::unbounded();
            let toasts = toast::Toasts::new(super::APP_ID, tx).inspect_err(|e| log(&format!("notifications: {e}"))).ok();
            (rx, toasts)
        };
        #[cfg(not(windows))]
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
            this_button: RefCell::default(),
            token: RefCell::default(),
            signals: RefCell::default(),
            #[cfg(windows)]
            toasts,
        });
        #[cfg(windows)]
        n.listen_windows(answers);
        n.listen();
        n
    }

    /// Windows: a click or a dismissal of the toast on screen (`toast::Answer`; ones for an
    /// older toast are ignored).
    #[cfg(windows)]
    fn listen_windows(self: &Rc<Self>, answers: async_channel::Receiver<toast::Answer>) {
        let n = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            while let Ok(answer) = answers.recv().await {
                let Some(n) = n.upgrade() else { return };
                if let toast::Answer::Clicked(sent, action) = &answer {
                    log(&format!("notification clicked: {action} ({sent}, showing {})", n.id.get()));
                }
                match answer {
                    toast::Answer::Clicked(sent, action) if sent == n.id.get() => n.clicked(&action),
                    toast::Answer::Gone(sent) if sent == n.id.get() => n.forget(),
                    _ => {}
                }
            }
        });
    }

    /// A click on the notification (`action` "button": its button; anything else: its body or
    /// Show Ferret, which bring the window up).
    fn clicked(&self, action: &str) {
        self.forget();
        if action == "button" {
            let f = self.this_button.borrow().clone().or_else(|| self.on_button.borrow().clone());
            if let Some(f) = f {
                f();
            }
            return;
        }
        if let Some(w) = self.window.borrow().as_ref() {
            if let Some(token) = self.token.take() {
                w.set_startup_id(&token);
            }
            w.present();
        }
    }

    /// The window: notifications only go out while it isn't in front, and leave when it comes.
    pub fn set_window(self: &Rc<Self>, window: &impl IsA<gtk::Window>) {
        let n = Rc::downgrade(self);
        // Only logged when it closes a notification: every switch to and from the game filled
        // half the log (and bug reports) with "in front" / "left the front".
        window.connect_is_active_notify(move |w| {
            if focus_log() {
                log(&format!("focus: {}", focus_state(w.upcast_ref())));
            }
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
                    n.clicked(&action);
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
        if self.away() {
            self.this_button.replace(None);
        }
        self.notify(title, body, button, secs);
    }

    /// `send` with a button of its own (Quit Ferret), not `on_button`.
    pub fn send_action(self: &Rc<Self>, title: &str, body: &str, button: &str, action: impl Fn() + 'static, secs: Option<u32>) {
        if self.away() {
            self.this_button.replace(Some(Rc::new(action)));
        }
        self.notify(title, body, Some(button), secs);
    }

    fn notify(self: &Rc<Self>, title: &str, body: &str, button: Option<&str>, secs: Option<u32>) {
        // The test mode's random presses go on while the tester works elsewhere: not on their screen.
        if super::monkey::on() {
            return super::monkey::log(&format!("        notification (not shown): {title}"));
        }
        #[cfg(windows)]
        return self.toast(title, body, button, secs);
        #[cfg(not(windows))]
        self.notify_dbus(title, body, button, secs);
    }

    /// Windows: the same as a toast, which replaces the one on screen.
    #[cfg(windows)]
    fn toast(self: &Rc<Self>, title: &str, body: &str, button: Option<&str>, secs: Option<u32>) {
        let Some(toasts) = &self.toasts else { return };
        if !self.away() {
            return;
        }
        log(&format!("notification: {title}"));
        if let Some(w) = self.window.borrow().as_ref().filter(|_| focus_log()) {
            log(&format!("focus at notification: {}", focus_state(w)));
        }
        let sent = self.sent.get() + 1;
        self.sent.set(sent);
        self.waiting.set(secs.is_none());
        let show = crate::i18n::tr!("Show Ferret");
        let mut actions = vec![("default", show.as_str())];
        if let Some(label) = button {
            actions.push(("button", label));
        }
        if let Err(e) = toasts.show(sent, title, body, &actions, secs.is_none()) {
            return log(&format!("notification failed: {e}"));
        }
        // The id is what was sent: answers carry it.
        self.id.set(sent);
        if let Some(secs) = secs {
            let weak = Rc::downgrade(self);
            glib::timeout_add_local_once(Duration::from_secs(secs as u64), move || {
                if let Some(n) = weak.upgrade().filter(|n| n.sent.get() == sent) {
                    n.withdraw();
                }
            });
        }
    }

    #[cfg_attr(windows, allow(dead_code))]
    fn notify_dbus(self: &Rc<Self>, title: &str, body: &str, button: Option<&str>, secs: Option<u32>) {
        let Some(bus) = &self.bus else { return };
        if !self.away() {
            return;
        }
        log(&format!("notification: {title}"));
        let sent = self.sent.get() + 1;
        self.sent.set(sent);
        self.waiting.set(secs.is_none());
        let mut actions = vec!["default".to_owned(), crate::i18n::tr!("Show Ferret")];
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
        #[cfg(windows)]
        if let Some(toasts) = &self.toasts {
            let _ = id;
            return toasts.hide();
        }
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

/// Windows' notifications ("toasts", WinRT). Ferret registers its app ID for them under the
/// player's account (HKCU\Software\Classes\AppUserModelId\<id>: name and icon), the way Windows
/// App SDK apps without an installer do. Clicks come through a COM "notification activator"
/// (`Activator`, CLSID `ACTIVATOR`, registered there too and as the running process's class
/// object): an app without a package gets no in-process Activated event for them (tried: the
/// toast went away and nothing came). Each button's arguments carry the toast's number
/// ("button:12"), so a click on an older toast is told apart. A click with Ferret closed starts it.
#[cfg(windows)]
mod toast {
    use std::cell::RefCell;

    use gtk::prelude::TextureExt;
    use windows::core::{implement, IUnknown, Interface, Ref, BOOL, GUID, HSTRING, PCWSTR};
    use windows::Win32::System::Com::{CoInitializeEx, CoRegisterClassObject, IClassFactory, IClassFactory_Impl, CLSCTX_LOCAL_SERVER, COINIT_APARTMENTTHREADED, REGCLS_AGILE, REGCLS_MULTIPLEUSE};
    use windows::Win32::UI::Notifications::{INotificationActivationCallback, INotificationActivationCallback_Impl, NOTIFICATION_USER_INPUT_DATA};
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::Foundation::TypedEventHandler;
    use windows::UI::Notifications::{ToastDismissalReason, ToastDismissedEventArgs, ToastNotification, ToastNotificationManager, ToastNotifier};

    /// Ferret's notification activator.
    const ACTIVATOR: GUID = GUID::from_u128(0x2B2DC977_6813_4EBF_A7EC_13982956FEA9);

    /// What Windows calls when a toast is clicked: its arguments ("button:12") to the window.
    #[implement(INotificationActivationCallback)]
    struct Activator(async_channel::Sender<Answer>);

    impl INotificationActivationCallback_Impl for Activator_Impl {
        fn Activate(&self, _app: &PCWSTR, args: &PCWSTR, _data: *const NOTIFICATION_USER_INPUT_DATA, _count: u32) -> windows::core::Result<()> {
            let args = unsafe { args.to_string() }.unwrap_or_default();
            let (action, sent) = args.split_once(':').unwrap_or((&args, "0"));
            self.0.send_blocking(Answer::Clicked(sent.parse().unwrap_or(0), action.to_owned())).ok();
            Ok(())
        }
    }

    #[implement(IClassFactory)]
    struct Factory(async_channel::Sender<Answer>);

    impl IClassFactory_Impl for Factory_Impl {
        fn CreateInstance(&self, outer: Ref<IUnknown>, iid: *const GUID, object: *mut *mut core::ffi::c_void) -> windows::core::Result<()> {
            if outer.is_some() {
                return Err(windows::Win32::Foundation::CLASS_E_NOAGGREGATION.into());
            }
            let activator: INotificationActivationCallback = Activator(self.0.clone()).into();
            unsafe { activator.query(iid, object).ok() }
        }

        fn LockServer(&self, _lock: BOOL) -> windows::core::Result<()> {
            Ok(())
        }
    }
    use windows::Win32::System::Registry::{RegCreateKeyExW, RegSetValueExW, HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ};

    pub enum Answer {
        /// The toast `sent` was clicked: "button", or anything else for its body / Show Ferret.
        Clicked(u32, String),
        /// The player dismissed it, or it timed out into the notification centre.
        Gone(u32),
    }

    pub struct Toasts {
        notifier: ToastNotifier,
        shown: RefCell<Option<ToastNotification>>,
        tx: async_channel::Sender<Answer>,
    }

    fn set(key: HKEY, name: &str, value: &str) {
        let data: Vec<u8> = value.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect();
        unsafe { RegSetValueExW(key, &HSTRING::from(name), None, REG_SZ, Some(&data)) }.ok().ok();
    }

    /// The app's name and icon for its notifications (a PNG made from Ferret's SVG icon once).
    fn register(app_id: &str) {
        let mut key = HKEY::default();
        let path = HSTRING::from(format!("Software\\Classes\\AppUserModelId\\{app_id}"));
        let made = unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, &path, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut key, None) };
        if made.is_err() {
            return;
        }
        set(key, "DisplayName", "Ferret");
        let clsid = format!("{{{ACTIVATOR:?}}}");
        set(key, "CustomActivator", &clsid);
        let png = crate::core::cache_dir().join("ferret-icon.png");
        let svg = crate::core::share_dir().join(format!("icons/hicolor/scalable/apps/{app_id}.svg"));
        if !png.exists() {
            if let Ok(t) = gtk::gdk::Texture::from_filename(&svg) {
                t.save_to_png(&png).ok();
            }
        }
        if png.exists() {
            set(key, "IconUri", &png.to_string_lossy());
        }
        unsafe { windows::Win32::System::Registry::RegCloseKey(key) }.ok().ok();
        // Started by Windows when a toast is clicked while Ferret isn't running.
        let mut server = HKEY::default();
        let path = HSTRING::from(format!("Software\\Classes\\CLSID\\{clsid}\\LocalServer32"));
        if unsafe { RegCreateKeyExW(HKEY_CURRENT_USER, &path, None, None, REG_OPTION_NON_VOLATILE, KEY_SET_VALUE, None, &mut server, None) }.is_ok() {
            if let Ok(exe) = std::env::current_exe() {
                set(server, "", &format!("\"{}\"", exe.display()));
            }
            unsafe { windows::Win32::System::Registry::RegCloseKey(server) }.ok().ok();
        }
    }

    fn escape(s: &str) -> String {
        s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
    }

    impl Toasts {
        pub fn new(app_id: &str, tx: async_channel::Sender<Answer>) -> Result<Self, String> {
            register(app_id);
            // Agile: Windows calls the activator on a COM thread of its own, not through the
            // window thread's messages (the window's thread is an STA, and GTK's loop never let
            // the calls through: Windows App SDK registers it so for the same reason). Ferret's
            // class object lives as long as the process.
            unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.ok().ok();
            let factory: IClassFactory = Factory(tx.clone()).into();
            let reg = unsafe { CoRegisterClassObject(&ACTIVATOR, &factory, CLSCTX_LOCAL_SERVER, REGCLS_MULTIPLEUSE | REGCLS_AGILE) };
            reg.map_err(|e| e.message())?;
            let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(app_id)).map_err(|e| e.message())?;
            Ok(Self { notifier, shown: RefCell::default(), tx })
        }

        /// Shows a toast in place of the last one. `actions`: (argument, label) per button.
        /// `long`: it waits for the player (Your turn): it stays up longer.
        pub fn show(&self, sent: u32, title: &str, body: &str, actions: &[(&str, &str)], long: bool) -> Result<(), String> {
            let buttons: String = actions
                .iter()
                .map(|(arg, label)| format!(r#"<action content="{}" arguments="{arg}:{sent}" activationType="foreground"/>"#, escape(label)))
                .collect();
            // "urgent": shown over Do Not Disturb, which Windows turns on in fullscreen games,
            // where the player allows important notifications for Ferret (Windows 11).
            let xml = format!(
                r#"<toast launch="default:{sent}" scenario="urgent"{}><visual><binding template="ToastGeneric"><text>{}</text><text>{}</text></binding></visual><actions>{buttons}</actions></toast>"#,
                if long { r#" duration="long""# } else { "" },
                escape(title),
                escape(body)
            );
            let doc = XmlDocument::new().map_err(|e| e.message())?;
            doc.LoadXml(&HSTRING::from(xml)).map_err(|e| e.message())?;
            let toast = ToastNotification::CreateToastNotification(&doc).map_err(|e| e.message())?;
            toast.SetTag(&HSTRING::from("ferret")).ok();
            toast.SetGroup(&HSTRING::from("ferret")).ok();
            let tx = self.tx.clone();
            toast
                .Dismissed(&TypedEventHandler::<ToastNotification, ToastDismissedEventArgs>::new(move |_, args| {
                    let reason = args.as_ref().and_then(|a| a.Reason().ok());
                    if reason != Some(ToastDismissalReason::ApplicationHidden) {
                        tx.send_blocking(Answer::Gone(sent)).ok();
                    }
                    Ok(())
                }))
                .map_err(|e| e.message())?;
            self.hide();
            self.notifier.Show(&toast).map_err(|e| e.message())?;
            self.shown.replace(Some(toast));
            Ok(())
        }

        /// Takes the one on screen away (also from the notification centre).
        pub fn hide(&self) {
            if let Some(t) = self.shown.take() {
                self.notifier.Hide(&t).ok();
            }
        }
    }
}
