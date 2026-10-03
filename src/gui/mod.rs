// The GTK4 + libadwaita interface. A worker thread owns the core, so slow work
// (scans, OCR, the window picker, tracing) never blocks the window; the
// interface sends it jobs and gets events back.

mod find;
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

pub enum Event {
    Log(String),
    /// The core could not start (for example the host helper is missing).
    Failed(String),
    Games(Vec<GameProcess>),
    /// The game's pid and program name.
    Attached(Result<(u32, String), String>),
    Values(Result<Vec<ValueRow>, String>),
    Numbers(Result<(PathBuf, Vec<Word>), String>),
    /// The read, and the watched area afterwards (it snaps to the number found).
    Read(Result<Option<(crate::ocr::Shown, bool)>, String>, Option<crate::ocr::Rect>),
    Auto(Result<AutoResult, String>),
    Typed(Result<AutoResult, String>),
    /// The name, and whether Ferret is sure to find it again after a restart.
    Saved(Result<(String, bool), String>),
    /// The search was cleared (the Find tab already shows it).
    Reset,
    /// The attached game's learned digits, 0 to 9.
    Digits(Vec<Vec<crate::font::DigitShape>>),
    /// Anything else: a message to show, or an error.
    Done(Result<String, String>),
    /// The places still matching, with their values (the player tries them out).
    Matches(Result<Vec<(crate::core::Loc, String)>, String>),
    /// The player picked one of them as the value.
    Chosen(Result<crate::core::Loc, String>),
    /// A frame the watched number was just read from, and the watched area: shown while
    /// searching (at most one a second).
    Frame(gtk::gdk::Texture, Option<crate::ocr::Rect>),
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

fn start_worker(cancel: Arc<AtomicBool>) -> (Worker, async_channel::Receiver<Event>) {
    let (jobs_tx, jobs_rx) = mpsc::channel::<Job>();
    let (events_tx, events_rx) = async_channel::unbounded();
    std::thread::spawn(move || {
        let log = events_tx.clone();
        let log = Box::new(move |msg: &str| {
            log.send_blocking(Event::Log(msg.to_owned())).ok();
        });
        let mut core = match Core::new(log) {
            Ok(core) => core,
            Err(e) => {
                events_tx.send_blocking(Event::Failed(e)).ok();
                return;
            }
        };
        core.cancel = cancel;
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
        for job in jobs_rx {
            if events_tx.send_blocking(job(&mut core)).is_err() {
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
    worker: Worker,
    /// The game Ferret is attached to.
    attached: Rc<Cell<Option<u32>>>,
}

impl Ui {
    fn toast(&self, msg: &str) {
        self.toasts.add_toast(adw::Toast::new(msg));
    }

    fn on_game_page(&self) -> bool {
        self.nav.visible_page().as_ref() == Some(&self.game_page)
    }

    fn show_games(&self, games: Vec<GameProcess>) {
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
            if let Some(ac) = &g.anti_cheat {
                row.set_subtitle(&format!("{ac} detected. Ferret won't attach to games with anti-cheat."));
                row.add_suffix(&gtk::Image::from_icon_name("action-unavailable-symbolic"));
            } else {
                row.set_activatable(true);
                row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
                let (worker, find, nav, page, attached) =
                    (self.worker.clone(), self.find.clone(), self.nav.clone(), self.game_page.clone(), self.attached.clone());
                let pid = g.pid;
                row.connect_activated(move |_| {
                    // Back to the same game: everything (a search in progress too) is still there.
                    if attached.get() == Some(pid) {
                        nav.push(&page);
                        return;
                    }
                    find.stop();
                    worker.run(move |core| Event::Attached(core.attach(pid).map(|exe| (pid, exe))));
                });
            }
            self.games.append(&row);
        }
    }

    fn handle(self: &Rc<Self>, event: Event) {
        if !matches!(event, Event::Log(_) | Event::Failed(_) | Event::Frame(..)) {
            self.worker.pending.set(self.worker.pending.get().saturating_sub(1));
        }
        // These can teach Ferret digits.
        if matches!(event, Event::Attached(Ok(_)) | Event::Auto(_) | Event::Typed(_)) {
            self.worker.run(|core| Event::Digits(core.digits()));
        }
        match event {
            Event::Log(msg) => self.find.log(&msg),
            Event::Failed(e) => {
                self.games_error.set_description(Some(&e));
                self.games_stack.set_visible_child_name("error");
            }
            Event::Games(games) => self.show_games(games),
            Event::Attached(Ok((pid, exe))) => {
                if self.attached.replace(Some(pid)) != Some(pid) {
                    self.find.new_game();
                }
                self.game_page.set_title(&exe);
                if !self.on_game_page() {
                    self.nav.push(&self.game_page);
                }
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::Values(Ok(values)) => self.values.update(values),
            Event::Values(Err(_)) => {}
            Event::Numbers(r) => self.find.numbers(r),
            Event::Read(r, area) => self.find.read(r, area),
            Event::Frame(t, area) => self.find.show_frame(t, area),
            Event::Matches(Ok(list)) => self.find.show_matches(list),
            Event::Chosen(Ok(loc)) => self.find.chosen(loc),
            Event::Matches(Err(e)) | Event::Chosen(Err(e)) => self.toast(&e),
            Event::Auto(r) => self.find.auto_done(r),
            Event::Typed(r) => self.find.typed_done(r),
            Event::Saved(Ok((name, confirmed))) => {
                self.toast(&if confirmed {
                    format!("Saved {name}. Ferret finds it again every time you attach.")
                } else {
                    format!("Saved {name}. If it's wrong after restarting the game, find it again and save it as {name}.")
                });
                self.find.saved();
                self.stack.set_visible_child_name("values");
                self.worker.run(|core| Event::Values(core.values()));
            }
            Event::Reset => {}
            Event::Digits(shapes) => self.find.show_digits(shapes),
            Event::Done(Ok(msg)) => self.toast(&msg),
            Event::Saved(Err(e)) => {
                self.find.save_failed(&e);
                self.toast(&e);
            }
            Event::Attached(Err(e)) | Event::Done(Err(e)) => self.toast(&e),
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
    action(
        "try",
        Box::new(|ui, arg| {
            if let Some((i, v)) = arg.split_once(',').and_then(|(i, v)| Some((i.trim().parse().ok()?, v.trim().to_owned()))) {
                ui.find.try_listed(i, v);
            }
        }),
    );
    action("use", Box::new(|ui, i| ui.find.use_listed(i.trim().parse().unwrap_or(usize::MAX))));
    action("stop", Box::new(|ui, _| ui.find.stop()));
    action("reset", Box::new(|ui, _| ui.find.start_over()));
    action(
        "type",
        Box::new(|ui, n| {
            ui.stack.set_visible_child_name("find");
            ui.find.root.set_visible_child_name("pick");
            ui.find.type_number(&n);
        }),
    );
    action("save", Box::new(|ui, name| ui.find.save_as(&name)));
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
         label.news.success.flash { background-color: alpha(@success_color, 0.5); transition: none; }",
    );
    if let Some(display) = gtk::gdk::Display::default() {
        gtk::style_context_add_provider_for_display(&display, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }
}

fn build(app: &adw::Application) {
    load_css();
    let cancel = Arc::new(AtomicBool::new(false));
    let (worker, events) = start_worker(cancel.clone());

    let stack = adw::ViewStack::new();
    let values = {
        let stack = stack.clone();
        values::ValuesView::new(worker.clone(), move || stack.set_visible_child_name("find"))
    };
    let find = find::FindView::new(worker.clone(), cancel);
    stack.add_titled_with_icon(&values.root, Some("values"), "Values", "view-list-symbolic");
    stack.add_titled_with_icon(&find.root, Some("find"), "Find Value", "edit-find-symbolic");

    let switcher = adw::ViewSwitcher::builder().stack(&stack).policy(adw::ViewSwitcherPolicy::Wide).build();
    let header = adw::HeaderBar::new();
    header.set_title_widget(Some(&switcher));
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

    let toasts = adw::ToastOverlay::new();
    toasts.set_child(Some(&nav));
    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("Ferret")
        .default_width(960)
        .default_height(720)
        .content(&toasts)
        .build();

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
        worker,
        attached: Rc::default(),
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
    // Keep the games list and the values fresh while nothing else is running.
    {
        let ui = ui.clone();
        glib::timeout_add_seconds_local(1, move || {
            if ui.worker.idle() {
                if ui.on_game_page() && ui.attached.get().is_some() {
                    ui.worker.run(|core| Event::Values(core.values()));
                } else if !ui.on_game_page() {
                    ui.worker.run(|core| Event::Games(core.games()));
                }
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
    ui.worker.run(|core| Event::Games(core.games()));
    add_debug_actions(app, &ui);
    ui.window.present();
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
