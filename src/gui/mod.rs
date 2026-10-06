// The GTK4 + libadwaita interface. A worker thread owns the core, so slow work
// (scans, OCR, the window picker, tracing) never blocks the window; the
// interface sends it jobs and gets events back.

mod find;
mod hotkeys;
mod notify;
mod phase;
mod tips;
mod values;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::core::{AutoResult, Core, GameProcess, ValueRow};
use crate::ocr::Word;

pub const APP_ID: &str = "io.github.Petru101.Ferret";
/// How long a closed window waits for the core to say whether limits run (a search stopping
/// first, a restore finishing) before Ferret quits anyway.
const CLOSE_WAIT: u32 = 30;

pub enum Event {
    Log(String),
    /// Something the player has to do now for the search to go on.
    Status(String),
    /// What a search or a restore is doing, and when the player has to act.
    Phase(crate::core::Phase),
    /// The core could not start (for example the host helper is missing).
    Failed(String),
    Games(Vec<GameProcess>),
    /// The game's pid and program name.
    Attached(Result<(u32, String), String>),
    Values(Result<Vec<ValueRow>, String>),
    Numbers(Result<(PathBuf, Vec<Word>), String>),
    /// The read, the watched area afterwards (it snaps to the number found), and the matches
    /// kept when a different number was picked during a search.
    Read(Result<Option<(crate::ocr::Shown, bool)>, String>, Option<crate::ocr::Rect>, Option<usize>),
    Auto(Result<AutoResult, String>),
    Typed(Result<AutoResult, String>),
    /// Scan Again (the number on screen now, changed or not) while Start wasn't running.
    ScannedAgain(Result<AutoResult, String>),
    /// The name, and whether Ferret is sure to find it again after a restart.
    /// The name and what to tell the player.
    Saved(Result<(String, String), String>),
    /// The search was cleared (the Find tab already shows it).
    Reset,
    /// A step of the search was taken back (Undo) or again (Redo): the places matching now and
    /// what it was, and "Undid" or "Redid".
    Undone(Result<(usize, String), String>, &'static str),
    /// A job with nothing to show.
    Idle,
    /// The attached game's learned digits, 0 to 9.
    Digits(Vec<Vec<crate::font::DigitShape>>),
    /// How many kinds of places earlier finds were in (searches look there first).
    Shapes(usize),
    /// Anything else: a message to show, or an error.
    Done(Result<String, String>),
    /// The places still matching, with their values (the player tries them out).
    Matches(Result<Vec<crate::core::Match>, String>),
    /// The player picked one of them as the value.
    Chosen(Result<crate::core::Loc, String>),
    /// The attached game quit (its program name).
    Quit(String),
    /// A frame the watched number was just read from, and the watched area: shown while
    /// searching (at most one a second).
    Frame(gtk::gdk::Texture, Option<crate::ocr::Rect>),
    /// What a read of the watched box saw (None: no number there now).
    Seen(Option<String>),
    /// A job panicked (a bug): what it said. The core lives on for the next job.
    Bug(String),
    /// The window was closed: the saved values kept in range (Ferret stays in the background
    /// for them; none: it quits).
    Closing(Vec<String>),
    /// Try on a listed place: the places left and how it went.
    Tried(Result<(Vec<crate::core::Match>, String), String>),
    /// The limits hotkey: whether limits are on now, and their names.
    Switched(Result<(bool, Vec<String>), String>),
}

type Job = Box<dyn FnOnce(&mut Core) -> Event + Send>;

/// Runs jobs on the core's thread, one at a time.
#[derive(Clone)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    pending: Rc<Cell<usize>>,
}

impl Worker {
    pub fn run(&self, job: impl FnOnce(&mut Core) -> Event + Send + 'static) {
        self.pending.set(self.pending.get() + 1);
        self.jobs.send(Box::new(job)).ok();
    }

    fn idle(&self) -> bool {
        self.pending.get() == 0
    }
}

fn start_worker(cancel: Arc<AtomicBool>, scan_now: Arc<AtomicBool>) -> (Worker, async_channel::Receiver<Event>) {
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let (events_tx, events_rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let log = events_tx.clone();
        let log = Box::new(move |msg: &str| {
            log.send_blocking(Event::Log(msg.to_owned())).ok();
        });
        let mut core = match Core::new(log, "ferret.log") {
            Ok(core) => core,
            Err(e) => {
                events_tx.send_blocking(Event::Failed(e)).ok();
                return;
            }
        };
        core.cancel = cancel;
        core.scan_now = scan_now;
        let status = events_tx.clone();
        core.on_status = Some(Box::new(move |msg: &str| {
            status.send_blocking(Event::Status(msg.to_owned())).ok();
        }));
        let matches = events_tx.clone();
        core.on_matches = Some(Box::new(move |list| {
            matches.send_blocking(Event::Matches(Ok(list))).ok();
        }));
        let phases = events_tx.clone();
        core.on_phase = Some(Box::new(move |p| {
            phases.send_blocking(Event::Phase(p)).ok();
        }));
        let frames = events_tx.clone();
        let mut shown = std::time::Instant::now() - std::time::Duration::from_secs(1);
        // Decoded here, so the window doesn't stall on big frames.
        core.on_frame = Some(Box::new(move |path, area| {
            if shown.elapsed() >= std::time::Duration::from_secs(1) {
                if let Ok(t) = gtk::gdk::Texture::from_filename(path) {
                    frames.send_blocking(Event::Frame(t, area)).ok();
                    shown = std::time::Instant::now();
                }
            }
        }));
        let seen = events_tx.clone();
        core.on_read = Some(Box::new(move |n| {
            seen.send_blocking(Event::Seen(n.map(|n| n.to_string()))).ok();
        }));
        for job in jobs_rx {
            // A bug in one job mustn't take the core and the helper with it: the window would
            // wait for its answer forever.
            let event = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut core))).unwrap_or_else(|panic| {
                let what = panic
                    .downcast_ref::<&str>()
                    .map(|s| s.to_string())
                    .or_else(|| panic.downcast_ref::<String>().cloned())
                    .unwrap_or_default();
                let msg = format!("Ferret hit a bug and stopped: {what}");
                core.say(&msg);
                Event::Bug(msg)
            });
            if events_tx.send_blocking(event).is_err() {
                break;
            }
        }
    });
    (Worker { jobs: jobs_tx, pending: Rc::new(Cell::new(0)) }, events_rx)
}

struct Ui {
    window: adw::ApplicationWindow,
    toasts: adw::ToastOverlay,
    nav: adw::NavigationView,
    games: gtk::ListBox,
    games_stack: gtk::Stack,
    games_error: adw::StatusPage,
    games_shown: RefCell<String>,
    game_page: adw::NavigationPage,
    stack: adw::ViewStack,
    values: Rc<values::ValuesView>,
    find: Rc<find::FindView>,
    phase: Rc<phase::PhaseCard>,
    tips: Rc<tips::Tips>,
    notify: Rc<notify::Notifier>,
    hotkeys: Rc<hotkeys::Hotkeys>,
    worker: Worker,
    /// The game Ferret is attached to.
    attached: Rc<Cell<Option<u32>>>,
    /// The game being attached to (restoring its values takes seconds): more clicks on it wait.
    attaching: Rc<Cell<Option<u32>>>,
    /// The program of a game that quit: attached to again when it starts.
    waiting_for: Rc<RefCell<Option<String>>>,
    /// The names games go by (Steam's), by program, from the games list.
    game_names: RefCell<std::collections::HashMap<String, String>>,
    /// The window was closed while limits ran: Ferret runs on without it until the game quits.
    background: Rc<Cell<bool>>,
    /// How many times the window was closed (a wait for the core belongs to one close).
    closes: Rc<Cell<u32>>,
    /// The Background portal's answer, while Ferret waits for it.
    portal_answer: RefCell<Option<gio::SignalSubscription>>,
}

impl Ui {
    fn app(&self) -> Option<gtk::Application> {
        self.window.application()
    }

    /// Closing hides the window at once; the core then says whether limits run.
    fn close(&self) {
        self.window.set_visible(false);
        self.find.stop();
        self.closes.set(self.closes.get() + 1);
        let close = self.closes.get();
        self.worker.run(|core| Event::Closing(core.limited()));
        // A job that doesn't end (a stuck helper) doesn't keep a hidden Ferret around.
        let (window, app, closes, background) = (self.window.clone(), self.app(), self.closes.clone(), self.background.clone());
        glib::timeout_add_seconds_local_once(CLOSE_WAIT, move || {
            if closes.get() == close && !window.is_visible() && !background.get() {
                app.inspect(|a| a.quit());
            }
        });
    }

    /// Runs on without a window so the limits hold while the player plays: asks the desktop
    /// (the Background portal; the first time it asks the player), and a notification says so,
    /// with a way to quit.
    fn keep_running(self: &Rc<Self>, names: &[String]) {
        if names.is_empty() {
            return;
        }
        let list = listed(names);
        self.background.set(true);
        let Some(app) = self.app() else { return };
        let notify = {
            let (app, list) = (app.clone(), list.clone());
            move || {
                let n = gio::Notification::new("Ferret Is Still Running");
                n.set_body(Some(&format!("It keeps {list} in range while you play, and quits when the game does.")));
                n.set_default_action("app.show");
                n.add_button("Quit Ferret", "app.quit");
                app.send_notification(Some("background"), &n);
            }
        };
        let Some(conn) = app.dbus_connection() else { return notify() };
        let token = format!("ferret{}", self.closes.get());
        let sender = conn.unique_name().map(|n| n.trim_start_matches(':').replace('.', "_")).unwrap_or_default();
        let path = format!("/org/freedesktop/portal/desktop/request/{sender}/{token}");
        {
            let (ui, notify) = (Rc::downgrade(self), notify.clone());
            let answer = conn.subscribe_to_signal(
                Some("org.freedesktop.portal.Desktop"),
                Some("org.freedesktop.portal.Request"),
                Some("Response"),
                Some(&path),
                None,
                gio::DBusSignalFlags::NONE,
                move |signal| {
                    let Some(ui) = ui.upgrade() else { return };
                    ui.portal_answer.take();
                    let (code, results) = signal.parameters.get::<(u32, glib::VariantDict)>().unwrap_or((2, glib::VariantDict::new(None)));
                    let allowed = code == 0 && results.lookup::<bool>("background").ok().flatten().unwrap_or(false);
                    eprintln!("background portal: response {code}, allowed {allowed}");
                    match allowed {
                        true if ui.background.get() => notify(),
                        true => {}
                        // The desktop would end it anyway (GNOME does).
                        false if ui.background.get() => ui.quit(),
                        false => {}
                    }
                },
            );
            self.portal_answer.replace(Some(answer));
        }
        let options = glib::VariantDict::new(None);
        options.insert("handle_token", &token);
        options.insert("reason", format!("To keep {list} in range while you play"));
        options.insert("autostart", false);
        options.insert("dbus-activatable", false);
        let ui = Rc::downgrade(self);
        conn.call(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
            "RequestBackground",
            Some(&glib::Variant::tuple_from_iter(["".to_variant(), options.end()])),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            move |r| {
                // No portal: nothing ends a windowless Ferret.
                if let Err(e) = r {
                    eprintln!("background portal: {e}");
                    if let Some(ui) = ui.upgrade() {
                        ui.portal_answer.take();
                        notify();
                    }
                }
            },
        );
        // What desktops listing background apps show for it (GNOME); older portals lack it.
        let status = glib::VariantDict::new(None);
        status.insert("message", format!("Keeping {list} in range"));
        conn.call(
            Some("org.freedesktop.portal.Desktop"),
            "/org/freedesktop/portal/desktop",
            "org.freedesktop.portal.Background",
            "SetStatus",
            Some(&glib::Variant::tuple_from_iter([status.end()])),
            None,
            gio::DBusCallFlags::NONE,
            -1,
            gio::Cancellable::NONE,
            |_| {},
        );
    }

    /// A hotkey was pressed (in the game, most likely: a notification says what it did).
    fn hotkey(&self, id: &str) {
        let refused = match id {
            "limits" => {
                self.worker.run(|core| Event::Switched(core.switch_limits()));
                None
            }
            "search" if self.attached.get().is_none() => Some(("Can't search yet", "Open the game in Ferret first.")),
            "search" => self.find.hotkey_search().map(|why| ("Can't search yet", why)),
            "again" => self.find.hotkey_again().map(|why| ("Can't scan again yet", why)),
            _ => None,
        };
        if let Some((title, why)) = refused {
            self.toast(why);
            self.notify.send(title, why, None, Some(6));
        }
    }

    /// The window is back: Ferret is an ordinary window again.
    fn shown(&self) {
        if self.background.replace(false) {
            self.app().inspect(|a| a.withdraw_notification("background"));
        }
    }

    fn quit(&self) {
        if let Some(app) = self.app() {
            if self.background.get() {
                app.withdraw_notification("background");
            }
            app.quit();
        }
    }

    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    /// A longer explanation than a toast holds. Lines starting with two spaces are commands,
    /// shown selectable so they can be copied.
    fn explain(&self, heading: &str, text: &str) {
        let lines = gtk::Box::new(gtk::Orientation::Vertical, 6);
        for line in text.lines() {
            let command = line.strip_prefix("  ");
            let label = gtk::Label::builder()
                .label(command.unwrap_or(line))
                .wrap(true)
                .wrap_mode(gtk::pango::WrapMode::WordChar)
                .xalign(0.0)
                .selectable(command.is_some())
                .build();
            if command.is_some() {
                label.add_css_class("monospace");
            }
            lines.append(&label);
        }
        let dialog = adw::AlertDialog::new(Some(heading), None);
        dialog.set_extra_child(Some(&lines));
        dialog.add_response("close", "Close");
        dialog.present(Some(&self.nav));
    }

    fn on_game_page(&self) -> bool {
        self.nav.visible_page().as_ref() == Some(&self.game_page)
    }

    fn show_games(&self, games: Vec<GameProcess>) {
        for g in &games {
            if let Some(name) = &g.name {
                self.game_names.borrow_mut().insert(g.exe.clone(), name.clone());
            }
        }
        // The game that quit is back: open it again (its saved values come back with it).
        let waiting = self.waiting_for.borrow().clone();
        if let Some(g) = waiting.and_then(|exe| games.iter().find(|g| g.exe == exe && g.anti_cheat.is_none() && !g.online_only)) {
            self.waiting_for.replace(None);
            self.toast(&format!("{} started again: opening it", g.name.as_deref().unwrap_or(&g.exe)));
            let pid = g.pid;
            self.attaching.set(Some(pid));
            self.worker.run(move |core| Event::Attached(core.attach(pid).map(|exe| (pid, exe))));
        }
        let shown: String = games.iter().map(|g| format!("{}:{};", g.pid, g.exe)).collect();
        self.games_stack.set_visible_child_name(if games.is_empty() { "empty" } else { "list" });
        if *self.games_shown.borrow() == shown {
            return;
        }
        *self.games_shown.borrow_mut() = shown;
        while let Some(row) = self.games.first_child() {
            self.games.remove(&row);
        }
        for g in games {
            let mut subtitle = format!("Process {}", g.pid);
            if let Some(id) = &g.app_id {
                subtitle = format!("Steam app {id} · {subtitle}");
            }
            let title = match &g.name {
                Some(name) => {
                    subtitle = format!("{} · {subtitle}", g.exe);
                    name
                }
                None => &g.exe,
            };
            if g.multiplayer {
                subtitle = format!("Multiplayer game: only change values when playing alone · {subtitle}");
            }
            let row = adw::ActionRow::builder().title(title).subtitle(&subtitle).build();
            row.set_subtitle_lines(2);
            let refused = match (&g.anti_cheat, g.online_only) {
                (Some(ac), _) => Some(format!("{ac} found. Ferret won't attach to games with anti-cheat.")),
                (None, true) => Some("Online-only game. Ferret won't attach to online games.".to_owned()),
                _ => None,
            };
            if let Some(why) = refused {
                row.set_subtitle(&why);
                row.add_suffix(&gtk::Image::from_icon_name("action-unavailable-symbolic"));
            } else {
                row.set_activatable(true);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (worker, find, nav, page, attached, attaching, waiting_for, toasts) = (
                    self.worker.clone(),
                    self.find.clone(),
                    self.nav.clone(),
                    self.game_page.clone(),
                    self.attached.clone(),
                    self.attaching.clone(),
                    self.waiting_for.clone(),
                    self.toasts.clone(),
                );
                let pid = g.pid;
                let opening = format!("Opening {}…", g.name.as_deref().unwrap_or(&g.exe));
                row.connect_activated(move |_| {
                    waiting_for.replace(None);
                    // Back to the same game: everything (a search in progress too) is still there.
                    if attached.get() == Some(pid) {
                        nav.push(&page);
                        return;
                    }
                    if attaching.get() == Some(pid) {
                        return;
                    }
                    attaching.set(Some(pid));
                    // Finding saved values again can take seconds in a big game.
                    toasts.add_toast(adw::Toast::new(&opening));
                    find.stop();
                    worker.run(move |core| Event::Attached(core.attach(pid).map(|exe| (pid, exe))));
                });
            }
            self.games.append(&row);
        }
    }

    fn handle(self: &Rc<Self>, event: Event) {
        if !matches!(event, Event::Log(_) | Event::Status(_) | Event::Phase(_) | Event::Failed(_) | Event::Frame(..) | Event::Seen(_)) {
            self.worker.pending.set(self.worker.pending.get().saturating_sub(1));
        }
        // These can teach Ferret digits.
        if matches!(event, Event::Attached(Ok(_)) | Event::Auto(_) | Event::Typed(_) | Event::ScannedAgain(_)) {
            self.worker.run(|core| Event::Digits(core.digits()));
            self.worker.run(|core| Event::Shapes(core.shape_count()));
        }
        match event {
            Event::Log(msg) => self.find.log(&msg),
            Event::Status(msg) => self.find.ask(&msg),
            Event::Phase(crate::core::Phase::Restoring(exe, name, i, n)) => {
                let game = self.game_names.borrow().get(&exe).cloned().unwrap_or(exe);
                self.phase.restoring(&game, &name, i, n);
            }
            Event::Phase(p) => self.find.phase(p),
            Event::Failed(e) => {
                self.games_error.set_description(Some(&e));
                self.games_stack.set_visible_child_name("error");
            }
            Event::Games(games) => self.show_games(games),
            Event::Attached(Ok((pid, exe))) => {
                self.attaching.set(None);
                self.phase.hide();
                self.waiting_for.replace(None);
                if self.attached.replace(Some(pid)) != Some(pid) {
                    self.find.new_game();
                    self.tips.show(tips::Tip::SaveFirst);
                }
                self.game_page.set_title(&exe);
                if !self.on_game_page() {
                    self.nav.push(&self.game_page);
                }
                self.worker.run(|core| Event::Values(core.values()));
            }
            // Nothing left to keep in range.
            Event::Quit(_) if self.background.get() => self.quit(),
            Event::Quit(exe) => {
                self.attached.set(None);
                self.find.stop();
                if self.on_game_page() {
                    self.nav.pop();
                }
                let name = self.game_names.borrow().get(&exe).cloned().unwrap_or_else(|| exe.clone());
                self.toast(&format!("{name} closed. Ferret opens it again when it starts."));
                self.notify.send(&format!("{name} closed"), "Ferret opens it again when it starts.", None, Some(6));
                self.waiting_for.replace(Some(exe));
            }
            Event::Values(Ok(values)) => {
                self.find.saved_values(&values);
                self.values.update(values);
            }
            Event::Values(Err(_)) => {}
            Event::Numbers(r) => self.find.numbers(r),
            Event::Read(r, area, kept) => self.find.read(r, area, kept),
            Event::Frame(t, area) => self.find.show_frame(t, area),
            Event::Seen(n) => self.find.seen(n),
            Event::Matches(Ok(list)) => self.find.show_matches(list),
            Event::Tried(Ok((list, msg))) => self.find.tried(list, &msg),
            Event::Tried(Err(e)) => self.toast(&e),
            Event::Chosen(Ok(loc)) => self.find.chosen(loc),
            Event::Matches(Err(e)) | Event::Chosen(Err(e)) => self.toast(&e),
            Event::Auto(r) => self.find.auto_done(r),
            Event::Typed(r) => self.find.typed_done(r),
            Event::ScannedAgain(r) => self.find.scanned_again(r),
            Event::Undone(r, done) => self.find.undone(r, done),
            Event::Saved(Ok((_, msg))) => {
                self.toast(&msg);
                self.notify.send("Saved", &msg, None, Some(6));
                self.find.saved();
                self.tips.show(tips::Tip::Slots);
                self.stack.set_visible_child_name("values");
                self.worker.run(|core| Event::Values(core.values()));
            }
            // Opened again meanwhile.
            Event::Closing(_) if self.window.is_visible() => {}
            Event::Closing(names) if names.is_empty() => self.quit(),
            Event::Closing(names) => self.keep_running(&names),
            Event::Switched(Ok((on, names))) => {
                let (title, body) = match on {
                    true => ("Limits on", format!("Ferret keeps {} in range again.", listed(&names))),
                    false => ("Limits off", format!("{} can change freely until you turn limits on again.", listed(&names))),
                };
                self.toast(&format!("{title}: {body}"));
                self.notify.send(title, &body, None, Some(4));
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::Switched(Err(e)) => {
                self.toast(&e);
                self.notify.send("Limits", &e, None, Some(4));
            }
            Event::Reset | Event::Idle => {}
            Event::Digits(shapes) => self.find.show_digits(shapes),
            Event::Shapes(n) => self.find.show_shapes(n),
            Event::Done(Ok(msg)) => self.toast(&msg),
            Event::Saved(Err(e)) => {
                self.find.save_failed(&e);
                self.toast(&e);
            }
            Event::Attached(Err(e)) => {
                self.attaching.set(None);
                self.phase.hide();
                match e.contains('\n') {
                    true => self.explain("Ferret Can't Open This Game", &e),
                    false => self.toast(&e),
                }
            }
            Event::Done(Err(e)) => self.toast(&e),
            Event::Bug(msg) => {
                self.attaching.set(None);
                self.phase.hide();
                self.find.bug(&msg);
            }
        }
    }
}

fn games_page(ui_list: &gtk::ListBox, stack: &gtk::Stack, error: &adw::StatusPage, refresh: &gtk::Button, banner: &adw::Banner) -> adw::NavigationPage {
    let group = adw::PreferencesGroup::builder()
        .title("Running games")
        .description("Pick the game to attach to. Ferret only works with single-player games.")
        .build();
    group.add(ui_list);
    let list = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(
            &adw::Clamp::builder()
                .maximum_size(640)
                .margin_top(24)
                .margin_bottom(24)
                .margin_start(12)
                .margin_end(12)
                .child(&group)
                .build(),
        )
        .build();
    let empty = adw::StatusPage::builder()
        .icon_name("input-gaming-symbolic")
        .title("No Games Running")
        .description("Start a game and it shows up here.")
        .build();
    stack.add_named(&list, Some("list"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(error, Some("error"));
    stack.set_visible_child_name("empty");

    let header = adw::HeaderBar::new();
    header.pack_start(refresh);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(banner);
    toolbar.set_content(Some(stack));
    adw::NavigationPage::builder().title("Ferret").tag("games").child(&toolbar).build()
}

/// Renders the window into a PNG, so the interface can be checked without
/// taking a screenshot of the desktop.
fn save_screenshot(window: &adw::ApplicationWindow, path: &str) -> Result<(), String> {
    let (w, h) = (window.width(), window.height());
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    paintable.snapshot(&snapshot, w as f64, h as f64);
    let node = snapshot.to_node().ok_or("nothing to render")?;
    let renderer = window.native().and_then(|n| n.renderer()).ok_or("no renderer")?;
    let texture = renderer.render_texture(node, None);
    texture.save_to_png(path).map_err(|e| e.to_string())
}

/// Debug actions, callable over D-Bus (org.gtk.Actions) to drive the
/// interface from test scripts.
fn add_debug_actions(app: &adw::Application, ui: &Rc<Ui>) {
    let action = |name: &str, f: Box<dyn Fn(&Rc<Ui>, String)>| {
        let a = gio::SimpleAction::new(name, Some(glib::VariantTy::STRING));
        let ui = ui.clone();
        a.connect_activate(move |_, p| f(&ui, p.and_then(|p| p.get::<String>()).unwrap_or_default()));
        app.add_action(&a);
    };
    action(
        "attach",
        Box::new(|ui, pid| {
            if let Ok(pid) = pid.parse() {
                ui.worker.run(move |core| Event::Attached(core.attach(pid).map(|exe| (pid, exe))));
            }
        }),
    );
    action(
        "screenshot",
        Box::new(|ui, path| {
            if let Err(e) = save_screenshot(&ui.window, &path) {
                eprintln!("screenshot: {e}");
            }
        }),
    );
    action(
        "page",
        Box::new(|ui, page| match page.as_str() {
            "games" => {
                ui.nav.pop_to_tag("games");
            }
            p => ui.stack.set_visible_child_name(p),
        }),
    );
    action("capture", Box::new(|ui, _| ui.find.capture()));
    action(
        "select",
        Box::new(|ui, rect| {
            let n: Vec<u32> = rect.split(',').filter_map(|v| v.trim().parse().ok()).collect();
            if let [x, y, w, h] = n[..] {
                ui.find.select(crate::ocr::Rect { x, y, w, h });
            }
        }),
    );
    action("find", Box::new(|ui, _| ui.find.start()));
    action("confirm", Box::new(|ui, yes| ui.find.answer_read(yes == "yes")));
    action(
        "try",
        Box::new(|ui, arg| {
            if let Some((i, v)) = arg.split_once(',').and_then(|(i, v)| Some((i.trim().parse().ok()?, v.trim().to_owned()))) {
                ui.find.try_listed(i, v);
            }
        }),
    );
    action("use", Box::new(|ui, i| ui.find.use_listed(i.trim().parse().unwrap_or(usize::MAX))));
    // Without the confirmation the button asks for.
    action("drop", Box::new(|ui, i| ui.find.drop_listed(i.trim().parse().unwrap_or(usize::MAX))));
    action("types", Box::new(|ui, i| ui.find.pick_types(i.trim().parse().unwrap_or(0))));
    action("stop", Box::new(|ui, _| ui.find.stop()));
    action("reset", Box::new(|ui, _| ui.find.start_over()));
    action("unpick", Box::new(|ui, _| ui.find.unpick()));
    action("undo", Box::new(|ui, _| ui.find.undo()));
    action("redo", Box::new(|ui, _| ui.find.redo()));
    action("name", Box::new(|ui, name| ui.find.type_name(&name)));
    action("confirm-value", Box::new(|ui, name| values::confirm_value(&ui.worker, &name)));
    action("keep", Box::new(|ui, name| ui.values.keep(name.trim())));
    action("forget-shapes", Box::new(|ui, _| find::forget_shapes(&ui.worker)));
    action("again", Box::new(|ui, _| ui.find.scan_again()));
    action(
        "type",
        Box::new(|ui, n| {
            ui.stack.set_visible_child_name("find");
            ui.find.root.set_visible_child_name("pick");
            ui.find.type_number(&n);
        }),
    );
    action("save", Box::new(|ui, name| ui.find.save_as(&name)));
    action("tip", Box::new(|ui, how| ui.tips.close(how == "never")));
    action("close", Box::new(|ui, _| ui.window.close()));
    // What a hotkey does, without the portal: `limits`, `search`, `again`.
    action("hotkey", Box::new(|ui, id| ui.hotkey(&id)));
    // Shows the status card in one state, for looking at it (`turn` can't be set up easily).
    action(
        "phase",
        Box::new(|ui, what| match what.as_str() {
            "scan" => ui.phase.scanning("192", 0.4),
            "ready" => ui.phase.ready(5094),
            "watch" => ui.phase.watching(183),
            "check" => ui.phase.checking(3, 0.5),
            "turn" => ui.phase.your_turn(3, 180),
            "save" => ui.phase.save_turn(),
            "restore" => ui.phase.restoring("Lumencraft", "lumen", 2, 5),
            "found" => ui.phase.done(true, "Found it!", "Give it a name below to keep it."),
            "several" => ui.phase.done(false, "7 places match", "Change the number in the game, then type the new one."),
            "change" => ui.phase.change_now("7 places match"),
            _ => ui.phase.hide(),
        }),
    );
    action("remove", Box::new(|ui, name| values::ask_remove(&ui.worker, name.trim(), &ui.stack)));
    action(
        "forget",
        Box::new(|ui, d| {
            if let Ok(d) = d.parse::<u8>() {
                ui.worker.run(move |core| {
                    core.forget_digit(d).ok();
                    Event::Digits(core.digits())
                });
            }
        }),
    );
}

/// Ferret's own styles.
fn load_css() {
    let css = gtk::CssProvider::new();
    css.load_from_string(
        ".found-row { background-color: alpha(@success_color, 0.15); border: 1px solid alpha(@success_color, 0.6); \
         border-radius: 12px; padding: 8px 12px; } \
         entry.searching { background-color: alpha(@accent_bg_color, 0.3); outline: 2px solid @accent_bg_color; \
         outline-offset: -2px; } \
         label.news { background-color: alpha(@accent_bg_color, 0.18); border-radius: 8px; padding: 4px 8px; \
         transition: background-color 1200ms ease-out; } \
         label.news.flash { background-color: alpha(@accent_bg_color, 0.65); transition: none; } \
         label.news.success { background-color: alpha(@success_color, 0.15); } \
         label.news.success.flash { background-color: alpha(@success_color, 0.5); transition: none; } \
         .phase-card { padding: 18px 24px; border-radius: 16px; border: 3px solid @warning_color; \
         background-color: mix(@window_bg_color, @warning_color, 0.25); box-shadow: 0 4px 16px alpha(black, 0.3); \
         transition: background-color 300ms ease-out; } \
         .phase-card.ready { border-color: @success_color; background-color: mix(@window_bg_color, @success_color, 0.3); } \
         .phase-card.watching { padding: 8px 20px; border-color: @accent_bg_color; \
         background-color: mix(@window_bg_color, @accent_bg_color, 0.2); } \
         .phase-card.done { border-color: @accent_bg_color; background-color: mix(@window_bg_color, @accent_bg_color, 0.2); } \
         .phase-card.turn { border-color: @accent_bg_color; background-color: mix(@window_bg_color, @accent_bg_color, 0.25); } \
         .phase-card.turn.flash { background-color: mix(@window_bg_color, @accent_bg_color, 0.6); } \
         .phase-card progressbar trough, .phase-card progressbar progress { min-height: 14px; border-radius: 7px; } \
         .phase-card progressbar text { font-size: 1.4em; font-weight: bold; color: @window_fg_color; opacity: 1; } \
         @keyframes nudge { from { box-shadow: 0 0 0 0 alpha(@accent_bg_color, 0.9); } \
         to { box-shadow: 0 0 0 12px alpha(@accent_bg_color, 0); } } \
         button.nudge { animation: nudge 1.3s ease-out infinite; } \
         .pick-hint { font-size: 1.4em; font-weight: bold; padding: 10px 18px; border-radius: 12px; \
         background-color: alpha(@accent_bg_color, 0.92); color: @accent_fg_color; box-shadow: 0 2px 8px alpha(black, 0.4); } \
         .read-number { font-size: 40px; font-weight: 800; font-feature-settings: \"tnum\"; } \
         .tip { background-color: alpha(@accent_bg_color, 0.15); padding: 6px 6px 6px 12px; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

fn build(app: &adw::Application) {
    load_css();
    let cancel = Arc::new(AtomicBool::new(false));
    let scan_now = Arc::new(AtomicBool::new(false));
    let (worker, events) = start_worker(cancel.clone(), scan_now.clone());

    let stack = adw::ViewStack::new();
    let values = {
        let stack = stack.clone();
        values::ValuesView::new(worker.clone(), move || stack.set_visible_child_name("find"))
    };
    let notify = notify::Notifier::new();
    let phase = phase::PhaseCard::new(notify.clone());
    let find = find::FindView::new(worker.clone(), cancel, scan_now, phase.clone());
    {
        let find = find.clone();
        phase.give_up.connect_clicked(move |_| find.stop());
        let give_up = phase.give_up.clone();
        notify.on_button(move || give_up.emit_clicked());
    }
    stack.add_titled_with_icon(&values.root, Some("values"), "Values", "view-list-symbolic");
    stack.add_titled_with_icon(&find.root, Some("find"), "Find Value", "edit-find-symbolic");

    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    let hotkeys = hotkeys::Hotkeys::new();
    header.pack_end(&hotkeys.button);
    // One per page (a widget has one parent), shown together.
    let banners = [(); 2].map(|_| adw::Banner::builder().title("A newer version of Ferret is installed").button_label("Restart").build());
    banners.iter().for_each(|b| b.set_action_name(Some("app.restart")));
    let restart = gio::SimpleAction::new("restart", None);
    {
        let app = app.clone();
        restart.connect_activate(move |_, _| match crate::core::restart_after_quit() {
            Ok(()) => app.quit(),
            Err(e) => eprintln!("restart: {e}"),
        });
    }
    app.add_action(&restart);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banners[0]);
    let tips = tips::Tips::new();
    toolbar.add_top_bar(&tips.root);
    toolbar.set_content(Some(&stack));
    let game_page = adw::NavigationPage::builder().title("Game").tag("game").child(&toolbar).build();

    let games = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let games_stack = gtk::Stack::new();
    let games_error = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title("Ferret Could Not Start")
        .build();
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some("Refresh"));
    let nav = adw::NavigationView::new();
    // Esc is for pausing the game; pressing it here by mistake shouldn't leave the game page.
    nav.set_pop_on_escape(false);
    nav.add(&games_page(&games, &games_stack, &games_error, &refresh, &banners[1]));

    // Over every page: the player may look at the Values tab or the games list meanwhile.
    let over = gtk::Overlay::builder().child(&nav).build();
    over.add_overlay(&phase.root);
    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&over));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ferret")
        .default_width(960)
        .default_height(720)
        .content(&toasts)
        .build();
    notify.set_window(&window);
    {
        let phase = phase.clone();
        window.connect_is_active_notify(move |w| {
            if !w.is_active() && w.is_visible() {
                phase.remind();
            }
        });
    }

    let ui = Rc::new(Ui {
        window,
        toasts,
        nav,
        games,
        games_stack,
        games_error,
        games_shown: RefCell::new(String::from("?")),
        game_page,
        stack,
        values,
        find,
        phase,
        tips,
        notify,
        hotkeys,
        worker,
        attached: Rc::default(),
        attaching: Rc::default(),
        waiting_for: Rc::default(),
        game_names: RefCell::default(),
        background: Rc::default(),
        closes: Rc::default(),
        portal_answer: RefCell::default(),
    });

    {
        let ui = ui.clone();
        glib::spawn_future_local(async move {
            while let Ok(event) = events.recv().await {
                ui.handle(event);
            }
        });
    }
    {
        let worker = ui.worker.clone();
        refresh.connect_clicked(move |_| worker.run(|core| Event::Games(core.games())));
    }
    // Keep the games list, the values and the matches list fresh while nothing else is running.
    {
        let ui = ui.clone();
        glib::timeout_add_seconds_local(1, move || {
            if ui.worker.idle() {
                let values = ui.on_game_page() && ui.attached.get().is_some();
                // The places still matching a search, with their values as they are now.
                let matches = values && ui.stack.visible_child_name().as_deref() == Some("find") && ui.find.matches_shown();
                ui.worker.run(move |core| match core.check_game() {
                    Some(exe) => Event::Quit(exe),
                    None if matches => Event::Matches(Ok(core.matches())),
                    None if values => Event::Values(core.values()),
                    None => Event::Games(core.games()),
                });
            }
            glib::ControlFlow::Continue
        });
    }
    // Notice a newer install: when the window comes back to the front (after installing from a
    // terminal) and every 30 s.
    {
        let check = Rc::new(move || {
            if banners[0].is_revealed() {
                return;
            }
            let banners = banners.clone();
            glib::spawn_future_local(async move {
                if gio::spawn_blocking(crate::core::newer_install).await.unwrap_or(false) {
                    banners.iter().for_each(|b| b.set_revealed(true));
                }
            });
        });
        let on_active = check.clone();
        ui.window.connect_is_active_notify(move |w| {
            if w.is_active() {
                on_active();
            }
        });
        glib::timeout_add_seconds_local(30, move || {
            check();
            glib::ControlFlow::Continue
        });
    }
    // Closing the window while limits run keeps Ferret running without it.
    {
        let ui = ui.clone();
        ui.window.clone().connect_close_request(move |_| {
            ui.close();
            glib::Propagation::Stop
        });
    }
    {
        let ui = ui.clone();
        ui.window.clone().connect_visible_notify(move |w| {
            if w.is_visible() {
                ui.shown();
            }
        });
    }
    let show = gio::SimpleAction::new("show", None);
    {
        let window = ui.window.clone();
        show.connect_activate(move |_, _| window.present());
    }
    app.add_action(&show);
    let quit = gio::SimpleAction::new("quit", None);
    {
        let app = app.clone();
        quit.connect_activate(move |_, _| app.quit());
    }
    app.add_action(&quit);
    {
        let ui2 = Rc::downgrade(&ui);
        ui.hotkeys.on_key(move |id| {
            if let Some(ui) = ui2.upgrade() {
                ui.hotkey(id);
            }
        });
    }
    ui.worker.run(|core| Event::Games(core.games()));
    add_debug_actions(app, &ui);
    ui.window.present();
}

/// "a", "a and b", "a, b and c".
fn listed(names: &[String]) -> String {
    match names {
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
        [] => String::new(),
    }
}

pub fn run() -> glib::ExitCode {
    let app = adw::Application::builder().application_id(APP_ID).build();
    app.connect_activate(|app| {
        if let Some(window) = app.active_window() {
            window.present();
            return;
        }
        build(app);
    });
    app.run_with_args::<&str>(&[])
}
