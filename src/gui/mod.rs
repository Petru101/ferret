// The GTK4 + libadwaita interface. A worker thread owns the core, so slow work
// (scans, OCR, the window picker, tracing) never blocks the window; the
// interface sends it jobs and gets events back.

mod browse;
mod feedback;
mod find;
mod hotkeys;
mod monkey;
mod notify;
mod guide;
mod phase;
mod share;
mod tips;
mod values;

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::AtomicBool;
use std::sync::{mpsc, Arc};

use adw::prelude::*;
use gtk::{gio, glib};

use crate::core::{AutoResult, Core, GameProcess, Gone, Motion, Refusal, ValueRow};
use crate::i18n::{ntr, tr};
use crate::ocr::Word;

pub const APP_ID: &str = "io.github.Petru101.Ferret";
/// How long a closed window waits for the core to say whether limits run (a search stopping
/// first, a restore finishing) before Ferret quits anyway.
const CLOSE_WAIT: u32 = 30;
/// Seconds between looks for games with anti-cheat while a game is open (`Ui::warn_risky`).
const RISKY_EVERY: u32 = 5;
/// How long a toast stays when nothing else happens (libadwaita's default is 5 s).
const TOAST_SECONDS: u32 = 4;

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
    Attached(Result<(u32, String), crate::core::AttachError>),
    Values(Result<Vec<ValueRow>, String>),
    Numbers(Result<(PathBuf, Vec<Word>), String>),
    /// The read, the watched area afterwards (it snaps to the number found), and the matches
    /// kept when a different number was picked during a search.
    /// The pick's read, where Ferret watches, the matches kept, and whether the box is a bar
    /// (asked only when it holds no number).
    Read(Result<Option<(crate::ocr::Shown, bool)>, String>, Option<crate::ocr::Rect>, Option<usize>, Option<Result<crate::bar::Kind, String>>),
    Auto(Result<AutoResult, String>),
    Typed(Result<AutoResult, String>),
    /// Scan Again (the number on screen now, changed or not) while Start wasn't running.
    ScannedAgain(Result<AutoResult, String>),
    /// No Number?: the copy of the game's memory is taken (or why not).
    Copied(Result<(), String>),
    /// No Number?'s readings ended (`Core::follow_motion`).
    Followed(Result<AutoResult, String>),
    /// A save failed (why): the pick taken back, with the places matching again (`Core::unpick`).
    Unpicked(String, Option<usize>),
    /// The name, and whether Ferret is sure to find it again after a restart.
    /// The name and what to tell the player.
    Saved(Result<(String, String), String>),
    /// "More Like This" ended: what it found and saved.
    Others(Result<String, String>),
    /// The search was cleared (the Find tab already shows it).
    Reset,
    /// A step of the search was taken back (Undo) or again (Redo): the places matching now and
    /// what it was, and whether it was Redo.
    Undone(Result<(usize, String), String>, bool),
    /// A job with nothing to show.
    Idle,
    /// The attached game's learned digits, 0 to 9.
    Digits(Vec<Vec<crate::font::DigitShape>>),
    /// How many kinds of places earlier finds were in (searches look there first).
    Shapes(usize),
    /// Anything else: a message to show, or an error.
    Done(Result<String, String>),
    /// The attached game's values as a `.ferret` file, and where it goes.
    Exported(Result<crate::core::Export, String>, share::To),
    /// What an import added and skipped.
    Imported(Result<crate::core::Imported, String>),
    /// What the shared library knows the open game by (program, Steam id, build).
    Browse(Result<(String, Option<String>, Option<String>), String>),
    /// The player's numbers by name (the Browse tab), or why there are none.
    Player(Result<Vec<crate::core::PlayerObject>, String>),
    /// The saved values and the Browse tab's numbers now, in its order.
    Peeked(Result<Vec<ValueRow>, String>, Vec<Option<f64>>),
    /// Games running with anti-cheat that may look at the whole computer (`Core::risky_games`).
    Risky(Vec<GameProcess>),
    /// Whether the game (its pid) has numbers the Browse tab can list.
    Browsable(u32, bool),
    /// A number from the Browse tab was saved: its name and what to tell the player.
    /// A listed value saved: its name, the message, whether it's a flag.
    BrowseSaved(Result<(String, String, bool), String>),
    /// The places still matching, with their values (the player tries them out).
    Matches(Result<Vec<crate::core::Match>, String>),
    /// The player picked one of them as the value.
    Chosen(Result<crate::core::Loc, String>),
    /// The attached game quit (its program name).
    Quit(String),
    /// Anti-cheat started in the attached game (its program name, the anti-cheat): Ferret let go.
    /// The game's program, the anti-cheat, and whether it started elsewhere on the computer.
    AntiCheat(String, String, bool),
    /// Ferret's helper stopped and a new one took over the game (its program name): the search
    /// starts over.
    HelperRestarted(String),
    /// Ferret's helper stopped and the new one couldn't open the game (its program name, why).
    HelperLost(String, String),
    /// The game just opened hides its numbers (its program name, the memory scrambler).
    Scrambler(Option<(String, String)>),
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
    /// Trying the places one at a time: the game didn't show the number tried, so the place
    /// got its old number back; the places left.
    PutBack(Vec<crate::core::Match>),
    /// The limits hotkey: whether limits are on now, and their names.
    Switched(Result<(bool, Vec<String>), String>),
}

type Job = Box<dyn FnOnce(&mut Core) -> Event + Send>;

/// Runs jobs on the core's thread, one at a time.
#[derive(Clone)]
pub struct Worker {
    jobs: mpsc::Sender<Job>,
    /// Jobs a running search takes between its reads (`run_now`).
    side: mpsc::Sender<crate::core::SideJob>,
    events: async_channel::Sender<Event>,
    pending: Rc<Cell<usize>>,
}

impl Worker {
    /// A job the player expects done now (a limit, a value set, the limits hotkey): a running
    /// search does it between its reads instead of after it ends. The queue gets a job that
    /// does it too, in case the search ends before taking it.
    pub fn run_now(&self, job: impl FnOnce(&mut Core) -> Event + Send + 'static) {
        let events = self.events.clone();
        self.pending.set(self.pending.get() + 1);
        self.side.send(Box::new(move |core| drop(events.send_blocking(job(core))))).ok();
        self.run(|core| {
            core.run_side_jobs();
            Event::Idle
        });
    }

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
    let (side_tx, side_rx) = mpsc::channel::<crate::core::SideJob>();
    let (events_tx, events_rx) = async_channel::unbounded();
    let events = events_tx.clone();
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
        core.side_jobs = Some(side_rx);
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
                core.say(&format!("Ferret hit a bug and stopped: {what}"));
                Event::Bug(tr!("Ferret hit a bug and stopped: {what}", what))
            });
            if events_tx.send_blocking(event).is_err() {
                break;
            }
        }
    });
    (Worker { jobs: jobs_tx, side: side_tx, events, pending: Rc::new(Cell::new(0)) }, events_rx)
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
    browse: Rc<browse::BrowseView>,
    phase: Rc<phase::PhaseCard>,
    guide: Rc<guide::Guide>,
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
    /// The toast saying a game is being opened: closed once it's open (or couldn't be). Left
    /// to its own 5 s it covered the Find tab's buttons ("Type the Number Instead") long after
    /// the game had opened.
    opening: Rc<RefCell<Option<adw::Toast>>>,
    /// The toast shown last (`toast`).
    last_toast: RefCell<Option<adw::Toast>>,
    /// Games with anti-cheat the player was told to quit Ferret for, by pid (`warn_risky`).
    warned_risky: RefCell<std::collections::HashSet<u32>>,
    /// Anti-cheat running on the computer the player was told about, by name
    /// (`warn_anti_cheat_elsewhere`).
    warned_elsewhere: RefCell<std::collections::HashSet<String>>,
    /// The names games go by (Steam's), by program, from the games list.
    game_names: RefCell<std::collections::HashMap<String, String>>,
    /// What the games list knows of each game's online features, by pid: the warning on opening it.
    online: RefCell<std::collections::HashMap<u32, Vec<String>>>,
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
        // Windows: no Background portal, and nothing ends a windowless program: Windows'
        // notification says so (click = the window, Quit Ferret).
        #[cfg(windows)]
        {
            let ui = self.clone();
            self.notify.send_action(
                &tr!("Ferret Is Still Running"),
                &tr!("It keeps {values} in range while you play, and quits when the game does.", values = list),
                &tr!("Quit Ferret"),
                move || ui.quit(),
                None,
            );
            return;
        }
        #[cfg_attr(windows, allow(unreachable_code))]
        let Some(app) = self.app() else { return };
        let notify = {
            let (app, list) = (app.clone(), list.clone());
            move || {
                let n = gio::Notification::new(&tr!("Ferret Is Still Running"));
                n.set_body(Some(&tr!("It keeps {values} in range while you play, and quits when the game does.", values = list)));
                n.set_default_action("app.show");
                n.add_button(&tr!("Quit Ferret"), "app.quit");
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
        options.insert("reason", tr!("To keep {values} in range while you play", values = list));
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
        status.insert("message", tr!("Keeping {values} in range", values = list));
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
                self.worker.run_now(|core| Event::Switched(core.switch_limits()));
                None
            }
            "search" if self.attached.get().is_none() => Some((tr!("Can't search yet"), tr!("Open the game in Ferret first."))),
            "search" => self.find.hotkey_search().map(|why| (tr!("Can't search yet"), why)),
            "again" => self.find.hotkey_again().map(|why| (tr!("Can't scan again yet"), why)),
            "moving" | "still" if self.attached.get().is_none() => Some((tr!("Can't search yet"), tr!("Open the game in Ferret first."))),
            "moving" => self.find.hotkey_motion(Motion::Moving).map(|why| (tr!("Can't search yet"), why)),
            "still" => self.find.hotkey_motion(Motion::Still).map(|why| (tr!("Can't search yet"), why)),
            _ => None,
        };
        if let Some((title, why)) = refused {
            self.toast(&why);
            self.notify.send(&title, &why, None, Some(6));
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

    /// A short message over the window, one at a time: libadwaita queues toasts and shows each
    /// for its whole time, so a save's message and the next ones stood over the Find tab's
    /// buttons for 15 s. It goes when the player clicks anything, switches tab or page
    /// (`dismiss_toast`): the player has moved on.
    fn toast(&self, msg: &str) {
        let toast = adw::Toast::builder().title(msg).timeout(TOAST_SECONDS).build();
        self.toasts.add_toast(toast.clone());
        if let Some(old) = self.last_toast.replace(Some(toast)) {
            old.dismiss();
        }
    }

    /// Toasts go above the Find tab's row of tools while it shows (they covered Start Over,
    /// Search and No Number?), and stay low elsewhere: raised on its first page they covered
    /// Type the Number Instead.
    fn place_toasts(&self) {
        let tools = self.on_game_page()
            && self.stack.visible_child_name().as_deref() == Some("find")
            && self.find.root.visible_child_name().as_deref() == Some("pick");
        if tools {
            self.toasts.add_css_class("raised");
        } else {
            self.toasts.remove_css_class("raised");
        }
    }

    fn dismiss_toast(&self) {
        if let Some(t) = self.last_toast.take() {
            t.dismiss();
        }
    }

    /// The game is open, or couldn't be: its "Opening" toast goes.
    fn opened(&self) {
        if let Some(t) = self.opening.take() {
            t.dismiss();
        }
    }

    /// A longer explanation than a toast holds. Lines starting with two spaces are commands,
    /// shown selectable so they can be copied.
    fn explain(&self, heading: &str, text: &str) {
        self.explanation(heading, text).present(Some(&self.nav));
    }

    /// The system blocks opening the game (kernel.yama.ptrace_scope): the explanation, with
    /// Allow With Your Password, which starts the helper again through pkexec (the desktop
    /// asks for the password once) and opens the game with it.
    fn offer_password(self: &Rc<Self>, pid: u32, heading: &str, text: &str) {
        let dialog = self.explanation(heading, text);
        dialog.add_response("allow", &tr!("Allow With Your Password"));
        dialog.set_response_appearance("allow", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("allow"));
        let ui = self.clone();
        dialog.connect_response(Some("allow"), move |_, _| {
            if ui.attaching.get().is_some() {
                return;
            }
            ui.attaching.set(Some(pid));
            ui.find.stop();
            ui.worker.run(move |core| Event::Attached(core.attach_with_password(pid).map(|exe| (pid, exe))));
        });
        dialog.present(Some(&self.nav));
    }

    fn explanation(&self, heading: &str, text: &str) -> adw::AlertDialog {
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
        dialog.add_response("close", &tr!("Close"));
        dialog
    }

    /// The first time a game with online features is opened: what they are and what to keep to.
    fn warn_online(&self, pid: u32, exe: &str) {
        let Some(online) = self.online.borrow().get(&pid).cloned().filter(|o| !o.is_empty()) else { return };
        let setting = format!("warned-online {exe}");
        if tips::has_setting(&setting) {
            return;
        }
        if let Err(e) = tips::add_setting(&setting) {
            eprintln!("settings: {e}");
        }
        let name = self.game_names.borrow().get(exe).cloned().unwrap_or_else(|| exe.to_owned());
        let mut text = online.join("\n");
        text.push('\n');
        text.push_str(&tr!(
            "Only change values while you play alone and offline. If anti-cheat starts in the game, Ferret lets go \
             of it at once."
        ));
        self.explain(&tr!("{name} has online features", name), &text);
    }

    /// The first time a game with a memory scrambler is opened: most of its numbers can't be found.
    fn warn_scrambler(&self, exe: &str, scrambler: &str) {
        let setting = format!("warned-scrambled {exe}");
        if tips::has_setting(&setting) {
            return;
        }
        if let Err(e) = tips::add_setting(&setting) {
            eprintln!("settings: {e}");
        }
        let name = self.game_names.borrow().get(exe).cloned().unwrap_or_else(|| exe.to_owned());
        let text = tr!(
            "{name} uses {scrambler}, which keeps its numbers scrambled in memory. Ferret can't find or change \
             most of them: the plain copies it finds are decoys the game puts back, and changing them may be \
             noticed by the game.",
            name,
            scrambler
        );
        self.explain(&tr!("{name} hides its numbers", name), &text);
    }

    /// Another game: the Browse tab shows only once the game is known to have numbers it can
    /// list (Unreal games).
    fn new_browse(self: &Rc<Self>, pid: u32) {
        self.browse.new_game();
        self.browsable(pid, false);
        self.worker.run(move |core| Event::Browsable(pid, core.browsable()));
    }

    fn browsable(self: &Rc<Self>, pid: u32, can: bool) {
        if self.attached.get() != Some(pid) {
            return;
        }
        let page = self.stack.page(&self.browse.root);
        if !can && self.stack.visible_child_name().as_deref() == Some("browse") {
            self.stack.set_visible_child_name("values");
        }
        page.set_visible(can);
        // A game opened as it starts has no objects yet: asked again a while later.
        if !can {
            let ui = self.clone();
            glib::timeout_add_seconds_local_once(30, move || {
                if ui.attached.get() == Some(pid) && !ui.stack.page(&ui.browse.root).is_visible() {
                    ui.worker.run(move |core| Event::Browsable(pid, core.browsable()));
                }
            });
        }
    }

    fn on_game_page(&self) -> bool {
        self.nav.visible_page().as_ref() == Some(&self.game_page)
    }

    /// A game with anti-cheat started while Ferret runs (open or hidden, keeping limits): Ferret
    /// won't open it, but some anti-cheat looks for cheat tools anywhere on the computer (the
    /// user's call: offer to quit, don't quit by itself). Once per game launch.
    fn warn_risky<'a>(self: &Rc<Self>, games: impl IntoIterator<Item = &'a GameProcess>) {
        for g in games {
            if let Some(Refusal::AntiCheatElsewhere(ac)) = &g.refused {
                self.warn_anti_cheat_elsewhere(ac);
                continue;
            }
            if !matches!(g.refused, Some(Refusal::AntiCheat(_) | Refusal::Vac)) {
                continue;
            }
            if !self.warned_risky.borrow_mut().insert(g.pid) {
                continue;
            }
            let game = g.name.clone().unwrap_or_else(|| g.exe.clone());
            let anticheat = match &g.refused {
                Some(Refusal::AntiCheat(ac)) => ac.clone(),
                _ => tr!("Valve Anti-Cheat"),
            };
            let ui = self.clone();
            self.notify.send_action(
                &tr!("{game} has anti-cheat", game),
                &tr!("Some anti-cheat looks for cheat tools anywhere on your computer. Quit Ferret while you play it."),
                &tr!("Quit Ferret"),
                move || ui.quit(),
                None,
            );
            if !self.window.is_visible() {
                continue;
            }
            let dialog = adw::AlertDialog::new(
                Some(&tr!("Quit Ferret While You Play {game}?", game)),
                Some(&tr!(
                    "{game} runs {anticheat}. Some anti-cheat looks for cheat tools anywhere on your computer, even ones that don't touch the game, and may ban for them. Ferret won't open this game, but it's safest to quit Ferret while you play it.",
                    game,
                    anticheat
                )),
            );
            dialog.add_responses(&[("keep", &tr!("Keep Ferret Open")), ("quit", &tr!("Quit Ferret"))]);
            dialog.set_response_appearance("quit", adw::ResponseAppearance::Suggested);
            dialog.set_default_response(Some("quit"));
            dialog.set_close_response("keep");
            let ui = self.clone();
            dialog.connect_response(Some("quit"), move |_, _| ui.quit());
            dialog.present(Some(&self.nav));
        }
    }

    /// Anti-cheat running on the computer, not in one game (Windows: a service or driver that
    /// watches every program): every game is refused meanwhile, and Ferret offers to quit, once
    /// per anti-cheat in a run (`warn_risky` is per game).
    fn warn_anti_cheat_elsewhere(self: &Rc<Self>, anticheat: &str) {
        if !self.warned_elsewhere.borrow_mut().insert(anticheat.to_owned()) {
            return;
        }
        let ui = self.clone();
        self.notify.send_action(
            &tr!("{anticheat} is running", anticheat),
            &tr!("Some anti-cheat looks for cheat tools anywhere on your computer. Quit Ferret while it runs."),
            &tr!("Quit Ferret"),
            move || ui.quit(),
            None,
        );
        if !self.window.is_visible() {
            return;
        }
        let dialog = adw::AlertDialog::new(
            Some(&tr!("Quit Ferret While {anticheat} Runs?", anticheat)),
            Some(&tr!(
                "{anticheat} is running on your computer. Some anti-cheat looks for cheat tools anywhere on your computer, even ones that don't touch its game, and may ban for them. Ferret won't open any game while it runs, but it's safest to quit Ferret.",
                anticheat
            )),
        );
        dialog.add_responses(&[("keep", &tr!("Keep Ferret Open")), ("quit", &tr!("Quit Ferret"))]);
        dialog.set_response_appearance("quit", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("quit"));
        dialog.set_close_response("keep");
        let ui = self.clone();
        dialog.connect_response(Some("quit"), move |_, _| ui.quit());
        dialog.present(Some(&self.nav));
    }

    fn show_games(self: &Rc<Self>, games: Vec<GameProcess>) {
        self.warn_risky(&games);
        for g in &games {
            if let Some(name) = &g.name {
                self.game_names.borrow_mut().insert(g.exe.clone(), name.clone());
            }
        }
        // The game that quit is back: open it again (its saved values come back with it).
        let waiting = self.waiting_for.borrow().clone();
        if let Some(g) = waiting.and_then(|exe| games.iter().find(|g| g.exe == exe && g.refused.is_none())) {
            self.waiting_for.replace(None);
            let toast = adw::Toast::new(&tr!("{name} started again: opening it", name = g.name.as_deref().unwrap_or(&g.exe)));
            self.toasts.add_toast(toast.clone());
            if let Some(old) = self.opening.replace(Some(toast)) {
                old.dismiss();
            }
            let pid = g.pid;
            self.attaching.set(Some(pid));
            self.worker.run(move |core| Event::Attached(core.attach(pid).map(|exe| (pid, exe))));
        }
        *self.online.borrow_mut() = games.iter().map(|g| (g.pid, online_features(g))).collect();
        let blocked = crate::core::native_games_blocked();
        let shown: String =
            games.iter().map(|g| format!("{}:{}:{:?}:{:?}:{}:{}:{}:{:?};", g.pid, g.exe, g.refused, g.ships, g.multiplayer, g.marks_accounts, g.leaderboards, g.resident)).chain([format!("{blocked:?}")]).collect();
        self.games_stack.set_visible_child_name(if games.is_empty() { "empty" } else { "list" });
        if *self.games_shown.borrow() == shown {
            return;
        }
        *self.games_shown.borrow_mut() = shown;
        while let Some(row) = self.games.first_child() {
            self.games.remove(&row);
        }
        for g in games {
            let mut subtitle = tr!("Process {pid}", pid = g.pid);
            if let Some(id) = &g.app_id {
                subtitle = format!("{} · {subtitle}", tr!("Steam app {id}", id));
            }
            let title = match &g.name {
                Some(name) => {
                    subtitle = format!("{} · {subtitle}", g.exe);
                    name
                }
                None => &g.exe,
            };
            // Native games on a system whose ptrace_scope keeps Ferret out (Windows games open
            // regardless: Wine lets anyone trace it).
            if !g.exe.to_ascii_lowercase().ends_with(".exe") {
                match blocked {
                    Some(true) => subtitle = format!("{} · {subtitle}", tr!("Needs your password")),
                    Some(false) => subtitle = format!("{} · {subtitle}", tr!("Blocked by your system until it restarts")),
                    None => {}
                }
            }
            let row = adw::ActionRow::builder().title(title).subtitle(&subtitle).build();
            row.set_subtitle_lines(2);
            let refused = g.refused.as_ref().map(|r| match r {
                Refusal::AntiCheat(ac) => tr!("{anticheat} is running. Ferret won't attach while anti-cheat runs.", anticheat = ac),
                Refusal::AntiCheatElsewhere(ac) => {
                    tr!("{anticheat} is running on your computer. Ferret won't attach to any game while it runs.", anticheat = ac)
                }
                Refusal::OnlineOnly => tr!("Online-only game. Ferret won't attach to online games."),
                Refusal::Vac => tr!(
                    "Uses Valve Anti-Cheat. Ferret won't attach to it. Source games started with -insecure \
                     in their launch options are allowed."
                ),
                Refusal::FlagsEdits => tr!("This game flags edited characters when they go online. Ferret won't attach to it."),
            });
            if let Some(why) = refused {
                row.set_subtitle(&why);
                row.add_suffix(&gtk::Image::from_icon_name("action-unavailable-symbolic"));
            } else {
                let online = online_features(&g);
                if !online.is_empty() {
                    let badge = gtk::Label::builder()
                        .label(tr!("Online features"))
                        .tooltip_text(online.join("\n"))
                        .valign(gtk::Align::Center)
                        .css_classes(["caption-heading", "warning"])
                        .build();
                    row.add_suffix(&badge);
                }
                row.set_activatable(true);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (worker, find, nav, page, attached, attaching, waiting_for, toasts, opening_toast) = (
                    self.worker.clone(),
                    self.find.clone(),
                    self.nav.clone(),
                    self.game_page.clone(),
                    self.attached.clone(),
                    self.attaching.clone(),
                    self.waiting_for.clone(),
                    self.toasts.clone(),
                    self.opening.clone(),
                );
                let pid = g.pid;
                let opening = tr!("Opening {game}…", game = g.name.as_deref().unwrap_or(&g.exe));
                row.connect_activated(move |_| {
                    waiting_for.replace(None);
                    // Back to the same game: everything (a search in progress too) is still there.
                    // Not pushed twice: the list can still be clicked while the page slides in
                    // (libadwaita: "Page is already in navigation stack").
                    if attached.get() == Some(pid) {
                        if nav.find_page("game").is_none() {
                            nav.push(&page);
                        }
                        return;
                    }
                    if attaching.get() == Some(pid) {
                        return;
                    }
                    attaching.set(Some(pid));
                    // Finding saved values again can take seconds in a big game.
                    let toast = adw::Toast::new(&opening);
                    toasts.add_toast(toast.clone());
                    if let Some(old) = opening_toast.replace(Some(toast)) {
                        old.dismiss();
                    }
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
                self.opened();
                self.phase.hide();
                self.waiting_for.replace(None);
                if self.attached.replace(Some(pid)) != Some(pid) {
                    self.find.new_game();
                    self.new_browse(pid);
                    self.tips.show(tips::Tip::SaveFirst);
                    self.warn_online(pid, &exe);
                    self.worker.run(|core| Event::Scrambler(core.scrambler()));
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
                self.toast(&tr!("{name} closed. Ferret opens it again when it starts.", name));
                self.notify.send(&tr!("{name} closed", name), &tr!("Ferret opens it again when it starts."), None, Some(6));
                self.waiting_for.replace(Some(exe));
            }
            Event::HelperRestarted(exe) => {
                let name = self.game_names.borrow().get(&exe).cloned().unwrap_or_else(|| exe.clone());
                self.find.start_over();
                self.toast(&tr!("Ferret's helper stopped and started again. {name} keeps its values in range; a search starts over.", name));
            }
            Event::HelperLost(exe, why) => {
                self.attached.set(None);
                self.find.stop();
                if self.on_game_page() {
                    self.nav.pop();
                }
                let name = self.game_names.borrow().get(&exe).cloned().unwrap_or_else(|| exe.clone());
                self.explain(&tr!("Ferret let go of {name}", name), &tr!("Ferret's helper stopped, and the new one couldn't open the game: {why}", why));
            }
            Event::Scrambler(Some((exe, scrambler))) => self.warn_scrambler(&exe, &scrambler),
            Event::Scrambler(None) => {}
            Event::AntiCheat(exe, ac, elsewhere) => {
                self.attached.set(None);
                self.find.stop();
                if self.on_game_page() {
                    self.nav.pop();
                }
                let name = self.game_names.borrow().get(&exe).cloned().unwrap_or_else(|| exe.clone());
                let heading = tr!("Ferret let go of {name}", name);
                let text = match elsewhere {
                    false => tr!(
                        "{anticheat} started in {name}. Ferret stopped keeping values in range and won't change anything in \
                         the game while the anti-cheat runs.",
                        anticheat = ac,
                        name
                    ),
                    true => tr!(
                        "{anticheat} started on your computer. Ferret stopped keeping values in range and won't change \
                         anything in {name} while the anti-cheat runs.",
                        anticheat = ac,
                        name
                    ),
                };
                self.notify.send(&heading, &text, None, Some(10));
                if self.background.get() {
                    self.quit();
                } else {
                    self.explain(&heading, &text);
                }
            }
            Event::Values(Ok(values)) => {
                self.find.saved_values(&values);
                self.values.update(values);
            }
            Event::Values(Err(_)) => {}
            Event::Peeked(values, now) => {
                if let Ok(values) = values {
                    self.find.saved_values(&values);
                    self.values.update(values);
                }
                self.browse.update(&now);
            }
            Event::Player(list) => self.browse.show(list),
            Event::Browsable(pid, can) => self.browsable(pid, can),
            Event::Risky(games) => self.warn_risky(&games),
            Event::BrowseSaved(Ok((name, msg, flag))) => {
                self.toast(&msg);
                if flag {
                    self.tips.show(tips::Tip::Flags);
                }
                self.values.show_when_listed(&name);
                self.stack.set_visible_child_name("values");
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::BrowseSaved(Err(e)) => self.toast(&e),
            Event::Numbers(r) => self.find.numbers(r),
            Event::Read(r, area, kept, bar) => self.find.read(r, area, kept, bar),
            Event::Frame(t, area) => self.find.show_frame(t, area),
            Event::Seen(n) => self.find.seen(n),
            Event::Matches(Ok(list)) => self.find.show_matches(list),
            Event::Tried(Ok((list, msg))) => self.find.tried(list, &msg),
            Event::Tried(Err(e)) => {
                self.toast(&e);
                self.find.try_failed();
            }
            Event::PutBack(list) => self.find.put_back(list),
            Event::Chosen(Ok(loc)) => self.find.chosen(loc),
            Event::Matches(Err(e)) | Event::Chosen(Err(e)) => self.toast(&e),
            Event::Auto(r) => self.find.auto_done(r),
            Event::Typed(r) => self.find.typed_done(r),
            Event::ScannedAgain(r) => self.find.scanned_again(r),
            Event::Copied(r) => self.find.copied(r),
            Event::Followed(r) => self.find.followed(r),
            Event::Unpicked(why, n) => self.find.unpicked(&why, n),
            Event::Undone(r, done) => self.find.undone(r, done),
            Event::Saved(Ok((name, msg))) => {
                self.toast(&msg);
                self.notify.send(&tr!("Saved"), &msg, None, Some(6));
                self.find.saved(&name);
                self.tips.show(tips::Tip::Slots);
                self.stack.set_visible_child_name("values");
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::Others(r) => {
                let (title, text) = match &r {
                    Ok(msg) => (tr!("More like this"), msg.clone()),
                    Err(e) => (tr!("Nothing saved"), e.clone()),
                };
                self.toast(&text);
                self.notify.send(&title, &text, None, Some(10));
                self.guide.pick(&tr!("Click the number you want to find"), &tr!("In the picture of your game below, or drag a box around it."));
                self.worker.run(|core| Event::Values(core.values()));
            }
            // Opened again meanwhile.
            Event::Closing(_) if self.window.is_visible() => {}
            Event::Closing(names) if names.is_empty() => self.quit(),
            Event::Closing(names) => self.keep_running(&names),
            Event::Switched(Ok((on, names))) => {
                let (title, body) = match on {
                    true => (tr!("Limits on"), tr!("Ferret keeps {values} in range again.", values = listed(&names))),
                    false => (tr!("Limits off"), tr!("{values} can change freely until you turn limits on again.", values = listed(&names))),
                };
                self.toast(&format!("{title}: {body}"));
                self.notify.send(&title, &body, None, Some(4));
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::Switched(Err(e)) => {
                self.toast(&e);
                self.notify.send(&tr!("Limits"), &e, None, Some(4));
            }
            Event::Reset | Event::Idle => {}
            Event::Digits(shapes) => self.find.show_digits(shapes),
            Event::Shapes(n) => self.find.show_shapes(n),
            Event::Done(Ok(msg)) => self.toast(&msg),
            Event::Exported(r, to) => share::exported(self, r, to),
            Event::Imported(r) => share::imported(self, r),
            Event::Browse(key) => share::browse(self, key),
            Event::Saved(Err(e)) => {
                self.find.save_failed(&e);
                self.toast(&e);
            }
            Event::Attached(Err(e)) => {
                self.attaching.set(None);
                self.opened();
                self.phase.hide();
                let heading = tr!("Ferret Can't Open This Game");
                if e.password_may_help {
                    self.offer_password(e.pid, &heading, &e.text);
                } else if e.text.contains('\n') {
                    self.explain(&heading, &e.text);
                } else {
                    self.toast(&e.text);
                }
            }
            Event::Done(Err(e)) => self.toast(&e),
            Event::Bug(msg) => {
                self.attaching.set(None);
                self.opened();
                self.phase.hide();
                self.find.bug(&msg);
            }
        }
    }
}

fn games_page(ui_list: &gtk::ListBox, stack: &gtk::Stack, error: &adw::StatusPage, refresh: &gtk::Button, banner: &adw::Banner) -> adw::NavigationPage {
    let group = adw::PreferencesGroup::builder()
        .title(tr!("Running games"))
        .description(tr!("Pick the game to attach to. Ferret only works with single-player games."))
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
        .title(tr!("No Games Running"))
        .description(tr!("Start a game and it shows up here."))
        .build();
    stack.add_named(&list, Some("list"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(error, Some("error"));
    stack.set_visible_child_name("empty");

    let header = adw::HeaderBar::new();
    header.pack_start(refresh);
    header.pack_end(&share::games_menu_button());
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(banner);
    toolbar.set_content(Some(stack));
    adw::NavigationPage::builder().title("Ferret").tag("games").child(&toolbar).build()
}

/// Renders the window into a PNG, so the interface can be checked without
/// taking a screenshot of the desktop. At the window's scale (2x on HiDPI screens).
fn save_screenshot(window: &adw::ApplicationWindow, path: &str) -> Result<(), String> {
    let (w, h) = (window.width(), window.height());
    let paintable = gtk::WidgetPaintable::new(Some(window));
    let snapshot = gtk::Snapshot::new();
    let scale = window.scale_factor() as f32;
    snapshot.scale(scale, scale);
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
    action("repick", Box::new(|ui, _| ui.find.repick()));
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
    // No Number?, then `motion moving|still`.
    action("copy", Box::new(|ui, _| ui.find.copy_memory()));
    action(
        "motion",
        Box::new(|ui, m| match m.as_str() {
            "moving" => ui.find.set_motion(Motion::Moving),
            _ => ui.find.set_motion(Motion::Still),
        }),
    );
    action("confirm", Box::new(|ui, yes| ui.find.answer_read(yes == "yes")));
    // Trying the places one at a time: start, try (with the number in the entry), yes, no, skip, stop.
    action("place", Box::new(|ui, step| ui.find.place_step(&step)));
    action("frame", Box::new(|ui, path| ui.find.debug_frame(&path)));
    action("zoom", Box::new(|ui, f| ui.find.debug_zoom(f.parse().unwrap_or(0.0))));
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
    // "More Like This" on a value, as its Find Them button (then `stop` = Done).
    action("others", Box::new(|ui, name| values::find_others(&ui.worker, name.trim())));
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
    // What a hotkey does, without the portal: `limits`, `search`, `again`, `moving`, `still`.
    action("hotkey", Box::new(|ui, id| ui.hotkey(&id)));
    // Shows the status card in one state, for looking at it (`turn` can't be set up easily).
    action(
        "phase",
        Box::new(|ui, what| match what.as_str() {
            "scan" => ui.guide.scanning("192", 0.4),
            "ready" => ui.guide.ready(5094),
            "watch" => ui.guide.watching(183),
            "check" => ui.guide.checking(3, 0.5),
            "turn" => ui.guide.your_turn(3, 180),
            "save" => ui.guide.save_turn(),
            "others" => ui.guide.others("jellify", 2),
            "restore" => ui.phase.restoring("Lumencraft", "lumen", 2, 5),
            "found" => ui.guide.ended(guide::Look::Found, guide::SAVE, "Found it!", "Give it a name and save it.", "Give it a name."),
            "several" => ui.guide.ended(guide::Look::Call, guide::CHECK, "7 places match: Ferret can't tell which one it is", "Try them below.", "Try them."),
            "change" => ui.guide.change_now("7 places match"),
            _ => {
                ui.phase.hide();
                ui.guide.pick("Click the number you want to find", "");
            }
        }),
    );
    action("remove", Box::new(|ui, name| values::ask_remove(&ui.worker, name.trim(), &ui.stack)));
    // The Browse tab: `list` (again), `search <text>`, `changed on|off`, `save <row>,<name>`
    // (the row's number among all listed, without the dialog).
    action("listed", Box::new(|ui, arg| ui.browse.debug(&arg)));
    // `<name>,<new name>`, without the dialog.
    action(
        "rename",
        Box::new(|ui, arg| {
            if let Some((name, to)) = arg.split_once(',') {
                values::rename(&ui.worker, name.trim(), to.trim());
            }
        }),
    );
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
        ".raised toast { margin-bottom: 64px; } \
         entry.searching { background-color: alpha(@accent_bg_color, 0.3); outline: 2px solid @accent_bg_color; \
         outline-offset: -2px; } \
         .phase-card { padding: 18px 24px; border-radius: 16px; border: 3px solid @accent_bg_color; \
         background-color: mix(@window_bg_color, @accent_bg_color, 0.2); box-shadow: 0 4px 16px alpha(black, 0.3); } \
         .phase-card progressbar trough, .phase-card progressbar progress { min-height: 14px; border-radius: 7px; } \
         .phase-card progressbar text { font-size: 1.4em; font-weight: bold; color: @window_fg_color; opacity: 1; } \
         .guide { padding: 12px 16px; border-radius: 12px; border: 2px solid alpha(currentColor, 0.15); \
         background-color: @card_bg_color; transition: background-color 300ms ease-out; } \
         .guide.wait { border-color: #e66100; background-color: mix(@window_bg_color, #e66100, 0.18); } \
         .guide.go { border-color: #26a269; background-color: mix(@window_bg_color, #26a269, 0.2); animation: go 1.6s ease-in-out infinite; } \
         .guide.go.flash { background-color: mix(@window_bg_color, #26a269, 0.45); } \
         .guide.call { border-color: #9141ac; background-color: mix(@window_bg_color, #9141ac, 0.16); } \
         .guide.found { border-color: #3584e4; background-color: mix(@window_bg_color, #3584e4, 0.18); } \
         @keyframes go { 0% { box-shadow: 0 0 0 0 alpha(#26a269, 0); } 50% { box-shadow: 0 0 0 5px alpha(#26a269, 0.35); } \
         100% { box-shadow: 0 0 0 0 alpha(#26a269, 0); } } \
         .guide-tag { font-size: 0.8em; font-weight: 800; letter-spacing: 1px; color: white; padding: 2px 8px; \
         border-radius: 6px; background-color: #3a4a5e; } \
         .guide-icon { min-width: 40px; min-height: 40px; border-radius: 20px; color: white; background-color: #3a4a5e; } \
         .guide.wait .guide-tag, .guide.wait .guide-icon, .guide.wait .guide-step.now { background-color: #b84700; } \
         .guide.go .guide-tag, .guide.go .guide-icon, .guide.go .guide-step.now { background-color: #16713f; } \
         .guide.call .guide-tag, .guide.call .guide-icon, .guide.call .guide-step.now { background-color: #6b2f86; } \
         .guide.found .guide-tag, .guide.found .guide-icon, .guide.found .guide-step.now { background-color: #1c5fb8; } \
         .guide-title { font-size: 1.45em; font-weight: 800; } \
         .guide-hint { font-size: 1.05em; } \
         .guide progressbar trough, .guide progressbar progress { min-height: 10px; border-radius: 5px; } \
         .guide progressbar text { font-weight: bold; color: @window_fg_color; opacity: 1; } \
         .guide-step { padding: 2px 10px; border-radius: 99px; border: 1px solid alpha(currentColor, 0.2); \
         font-size: 0.9em; font-weight: 600; opacity: 0.6; } \
         .guide-step.past { opacity: 0.85; } \
         .guide-step.now { opacity: 1; color: white; background-color: #3a4a5e; border-color: transparent; font-weight: 800; } \
         button.guide-button.chosen { background-color: #26a269; color: white; box-shadow: 0 0 0 3px alpha(#26a269, 0.35); } \
         button.guide-button { min-height: 40px; padding-left: 20px; padding-right: 20px; font-weight: 800; } \
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
    let phase = Rc::new(phase::PhaseCard::new());
    let guide = guide::Guide::new(notify.clone());
    let find = find::FindView::new(worker.clone(), cancel, scan_now, guide.clone());
    {
        // Stop Waiting and Cancel (a save waiting for the game) both stop what waits.
        let find_ = find.clone();
        guide.give_up.connect_clicked(move |_| find_.stop());
        let find_ = find.clone();
        notify.on_button(move || find_.stop());
    }
    stack.add_titled_with_icon(&values.root, Some("values"), &tr!("Values"), "view-list-symbolic");
    stack.add_titled_with_icon(&find.root, Some("find"), &tr!("Find Value"), "edit-find-symbolic");
    let browse = browse::BrowseView::new(worker.clone());
    stack.add_titled_with_icon(&browse.root, Some("browse"), &tr!("Browse"), "view-grid-symbolic").set_visible(false);
    {
        let browse = browse.clone();
        stack.connect_visible_child_name_notify(move |s| {
            if s.visible_child_name().as_deref() == Some("browse") {
                browse.shown();
            }
        });
    }

    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
    let hotkeys = hotkeys::Hotkeys::new();
    header.pack_end(&share::menu_button());
    header.pack_end(&hotkeys.button);
    // One per page (a widget has one parent), shown together.
    let banners = [(); 2].map(|_| adw::Banner::builder().title(tr!("A newer version of Ferret is installed")).button_label(tr!("Restart")).build());
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
    // Windows: a downloaded build installed after the window closes (`update::apply_if_asked`).
    #[cfg(windows)]
    {
        let update = gio::SimpleAction::new("update", None);
        let quit = app.clone();
        update.connect_activate(move |_, _| {
            crate::update::ask_to_apply();
            quit.quit();
        });
        app.add_action(&update);
    }
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.add_top_bar(&banners[0]);
    let tips = tips::Tips::new();
    toolbar.add_top_bar(&tips.root);
    // The instruction bar: on the Find tab always, on the Values tab while a search does something.
    toolbar.add_top_bar(&guide.root);
    {
        let guide_ = guide.clone();
        stack.connect_visible_child_name_notify(move |s| guide_.on_values(s.visible_child_name().as_deref() != Some("find")));
        guide.on_values(stack.visible_child_name().as_deref() != Some("find"));
    }
    toolbar.set_content(Some(&stack));
    let game_page = adw::NavigationPage::builder().title(tr!("Game")).tag("game").child(&toolbar).build();

    let games = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
    let games_stack = gtk::Stack::new();
    let games_error = adw::StatusPage::builder()
        .icon_name("dialog-error-symbolic")
        .title(tr!("Ferret Could Not Start"))
        .build();
    let refresh = gtk::Button::from_icon_name("view-refresh-symbolic");
    refresh.set_tooltip_text(Some(&tr!("Refresh")));
    let nav = adw::NavigationView::new();
    // Esc is for pausing the game; pressing it here by mistake shouldn't leave the game page.
    nav.set_pop_on_escape(false);
    nav.add(&games_page(&games, &games_stack, &games_error, &refresh, &banners[1]));

    // Over every page: a game opens from the games list.
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
        let guide = guide.clone();
        window.connect_is_active_notify(move |w| {
            if !w.is_active() && w.is_visible() {
                guide.remind();
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
        browse,
        phase,
        guide,
        tips,
        notify,
        hotkeys,
        worker,
        attached: Rc::default(),
        attaching: Rc::default(),
        waiting_for: Rc::default(),
        opening: Rc::default(),
        last_toast: RefCell::default(),
        warned_risky: RefCell::default(),
        warned_elsewhere: RefCell::default(),
        game_names: RefCell::default(),
        online: RefCell::default(),
        background: Rc::default(),
        closes: Rc::default(),
        portal_answer: RefCell::default(),
    });

    // The player moved on: the last message goes (`Ui::toast`).
    {
        let click = gtk::GestureClick::builder().button(0).propagation_phase(gtk::PropagationPhase::Capture).build();
        let ui_ = ui.clone();
        click.connect_pressed(move |_, _, _, _| ui_.dismiss_toast());
        ui.window.add_controller(click);
        let ui_ = ui.clone();
        ui.stack.connect_visible_child_name_notify(move |_| {
            ui_.dismiss_toast();
            ui_.place_toasts();
        });
        let ui_ = ui.clone();
        ui.nav.connect_visible_page_notify(move |_| {
            ui_.dismiss_toast();
            ui_.place_toasts();
        });
        let ui_ = ui.clone();
        ui.find.root.connect_visible_child_name_notify(move |_| ui_.place_toasts());
    }
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
        let ticks = Cell::new(0u32);
        glib::timeout_add_seconds_local(1, move || {
            ticks.set(ticks.get().wrapping_add(1));
            // With a game open the games list isn't refreshed: games with anti-cheat started
            // meanwhile are looked for every 5 s.
            if ui.worker.idle() && ui.attached.get().is_some() && ticks.get() % RISKY_EVERY == 0 {
                ui.worker.run(|core| Event::Risky(core.risky_games()));
                return glib::ControlFlow::Continue;
            }
            if ui.worker.idle() {
                let values = ui.on_game_page() && ui.attached.get().is_some();
                // The places still matching a search, with their values as they are now.
                let matches = values && ui.stack.visible_child_name().as_deref() == Some("find") && ui.find.matches_shown();
                // The Browse tab's numbers as they are now.
                let listed = if values && ui.stack.visible_child_name().as_deref() == Some("browse") { ui.browse.locs() } else { Vec::new() };
                ui.worker.run(move |core| match core.check_game() {
                    Some(Gone::Quit(exe)) => Event::Quit(exe),
                    Some(Gone::AntiCheat(exe, ac, elsewhere)) => Event::AntiCheat(exe, ac, elsewhere),
                    Some(Gone::HelperRestarted(exe)) => Event::HelperRestarted(exe),
                    Some(Gone::HelperLost(exe, why)) => Event::HelperLost(exe, why),
                    None if matches => Event::Matches(Ok(core.matches())),
                    None if !listed.is_empty() => Event::Peeked(core.values(), core.peek_values(&listed)),
                    None if values => Event::Values(core.values()),
                    None => Event::Games(core.games()),
                });
            }
            glib::ControlFlow::Continue
        });
    }
    // Windows: a newer build downloaded in the background, once per start; the banner offers
    // to install it now (else the next start does).
    #[cfg(windows)]
    {
        let banners = banners.clone();
        glib::timeout_add_seconds_local_once(5, move || {
            glib::spawn_future_local(async move {
                if let Ok(Some(version)) = gio::spawn_blocking(crate::update::download).await {
                    for b in &banners {
                        b.set_title(&tr!("Ferret {version} is ready to install", version = version));
                        b.set_action_name(Some("app.update"));
                        b.set_revealed(true);
                    }
                }
            });
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
    // Quit Ferret (the main menu, Ctrl+Q, the background notification): the only way out
    // while it keeps values in range, since closing the window then hides it.
    let quit = gio::SimpleAction::new("quit", None);
    {
        let ui = Rc::downgrade(&ui);
        quit.connect_activate(move |_, _| {
            if let Some(ui) = ui.upgrade() {
                ui.quit();
            }
        });
    }
    app.add_action(&quit);
    app.set_accels_for_action("app.quit", &["<Control>q"]);
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
    share::add_actions(app, &ui);
    ui.window.present();
    if monkey::on() {
        monkey::start();
    }
}

/// "a", "a and b", "a, b and c".
fn listed(names: &[String]) -> String {
    match names {
        [one] => one.clone(),
        [rest @ .., last] => tr!("{names} and {last}", names = rest.join(", "), last),
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

/// How a value is stored (core's `Kind::describe`, which the log keeps in English).
fn kind_text(kind: crate::core::Kind) -> String {
    use crate::core::Kind;
    match kind {
        Kind::I32 => tr!("whole number"),
        Kind::F32 => tr!("float"),
        Kind::F64 => tr!("double"),
        Kind::Xor => tr!("encoded whole number"),
        Kind::U16 => tr!("2-byte whole number"),
        Kind::Gm => tr!("GameMaker number"),
        Kind::Lua => tr!("Lua number"),
        Kind::Py => tr!("Python number"),
        Kind::Smi => tr!("JavaScript whole number"),
        Kind::Bit(_) => tr!("on/off flag"),
        Kind::JsBool => tr!("JavaScript on/off flag"),
    }
}

/// A value as the window shows it: a number, or a flag's On/Off.
fn value_text(kind: crate::core::Kind, v: f64, decimals: Option<u32>) -> String {
    match kind.flag() {
        true if v != 0.0 => tr!("On"),
        true => tr!("Off"),
        false => crate::core::number_text(v, decimals),
    }
}

/// "at least 5", "between 1 and 9" (core's `limit_text`).
fn range_text(min: Option<f64>, max: Option<f64>) -> String {
    let (min, max) = (min.map(|v| crate::core::number_text(v, None)), max.map(|v| crate::core::number_text(v, None)));
    match (min, max) {
        (None, Some(max)) => tr!("at most {max}", max),
        (Some(min), None) => tr!("at least {min}", min),
        (Some(min), Some(max)) => tr!("between {min} and {max}", min, max),
        (None, None) => tr!("unlimited"),
    }
}

/// "one kind of place", "3 kinds of places" (core's `kinds_of_places`).
fn kinds_of_places(n: usize) -> String {
    ntr!("one kind of place", "{n} kinds of places", n)
}

/// A game's online features, one sentence each (none: it has none Ferret knows of).
fn online_features(g: &GameProcess) -> Vec<String> {
    let mut lines = Vec::new();
    if g.multiplayer {
        lines.push(tr!("It can be played with other people."));
    }
    if let Some(ac) = &g.ships {
        lines.push(tr!("It comes with {anticheat}, which isn't running now.", anticheat = ac));
    }
    if g.marks_accounts {
        lines.push(tr!(
            "Its publisher marks accounts whose saves were changed: a marked account may be kept off leaderboards, \
             events or co-op."
        ));
    }
    if g.leaderboards {
        lines.push(tr!("Its scores go on online leaderboards that other players see."));
    }
    if let Some(ac) = &g.resident {
        lines.push(tr!(
            "{anticheat} runs on your computer all the time, for other games. It may take Ferret for a cheat tool: quit \
             Ferret before you play those games.",
            anticheat = ac
        ));
    }
    lines
}
