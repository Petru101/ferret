// The Find tab: shows the captured game window with the numbers OCR found.
// Click one (or drag a box around a number), press Start, and play; Ferret
// narrows the scan down every time the number changes on screen. When it can't
// read the number, the player types it instead.

use std::cell::{Cell, RefCell};
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use adw::prelude::*;
use gtk::{gdk, glib};

use super::{Event, Worker};
use crate::core::{self, AutoResult};
use crate::font::DigitShape;
use crate::ocr::{self, Rect, Shown, Word};

pub struct FindView {
    pub root: gtk::Stack,
    picture: gtk::Picture,
    area: gtk::DrawingArea,
    texture: RefCell<Option<gdk::Texture>>,
    words: RefCell<Vec<Word>>,
    selection: RefCell<Option<Rect>>,
    /// A read the player hasn't confirmed yet (Tesseract's, not the learned digits').
    unconfirmed: RefCell<Option<Shown>>,
    /// Drag start and current point, in widget coordinates.
    drag: RefCell<Option<(f64, f64, f64, f64)>>,
    status: gtk::Label,
    crop: gtk::Picture,
    start: gtk::Button,
    stop: gtk::Button,
    start_over: gtk::Button,
    spinner: gtk::Spinner,
    log: gtk::TextView,
    result: gtk::Box,
    name: gtk::Entry,
    /// The places still matching when Ferret couldn't tell which is the value: the player
    /// changes them and watches the game.
    matches: gtk::Box,
    matches_list: gtk::ListBox,
    /// The places listed there, in order (for the D-Bus debug actions).
    listed: RefCell<Vec<core::Loc>>,
    typed_row: gtk::Box,
    typed: gtk::Entry,
    typed_go: gtk::Button,
    /// How many places matched after the last search, to show how far a search narrowed it.
    last_count: Cell<Option<usize>>,
    digits: gtk::Box,
    digits_hint: gtk::Label,
    worker: Worker,
    cancel: Arc<AtomicBool>,
}

/// 5040383 -> "5,040,383".
fn grouped(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Groups the digits of match counts in a log line ("5040383 -> 1200 matches (1200 i32, ...)"),
/// leaving addresses and game values alone.
fn group_counts(msg: &str) -> String {
    let words: Vec<&str> = msg.split(' ').collect();
    let is_count = |i: usize| {
        let next = words.get(i + 1).copied().unwrap_or("");
        next == "->"
            || next.starts_with("matches")
            || ["i32", "f32", "f64", "xor"].iter().any(|k| next.trim_end_matches([',', ')']) == *k)
            || (i > 0 && words[i - 1] == "->")
    };
    words
        .iter()
        .enumerate()
        .map(|(i, w)| {
            let (open, rest) = w.strip_prefix('(').map_or(("", *w), |r| ("(", r));
            match rest.parse::<usize>() {
                Ok(n) if is_count(i) => format!("{open}{}", grouped(n)),
                _ => w.to_string(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Draws a learned digit, one square per cell, in the text colour.
fn draw_shape(area: &gtk::DrawingArea, cr: &gtk::cairo::Context, w: i32, h: i32, s: &DigitShape) {
    let c = area.color();
    cr.set_source_rgba(c.red() as f64, c.green() as f64, c.blue() as f64, c.alpha() as f64);
    let cell = (w as f64 / s.w as f64).min(h as f64 / s.h as f64);
    let (ox, oy) = ((w as f64 - cell * s.w as f64) / 2.0, (h as f64 - cell * s.h as f64) / 2.0);
    for (i, _) in s.cells.iter().enumerate().filter(|(_, on)| **on) {
        let (x, y) = (i as u32 % s.w, i as u32 / s.w);
        cr.rectangle(ox + x as f64 * cell, oy + y as f64 * cell, cell, cell);
    }
    cr.fill().ok();
}

fn frame_box() -> gtk::Box {
    gtk::Box::builder().spacing(12).margin_start(12).margin_end(12).build()
}

impl FindView {
    pub fn new(worker: Worker, cancel: Arc<AtomicBool>) -> Rc<Self> {
        let picture = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::ScaleDown)
            .can_shrink(true)
            .hexpand(true)
            .vexpand(true)
            .build();
        let area = gtk::DrawingArea::new();
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&picture));
        overlay.add_overlay(&area);
        let scroll = gtk::ScrolledWindow::builder().child(&overlay).vexpand(true).build();

        let capture = gtk::Button::builder().icon_name("camera-photo-symbolic").tooltip_text("Capture again").build();
        let zoom = gtk::ToggleButton::builder().icon_name("zoom-original-symbolic").tooltip_text("Actual size").build();
        let status = gtk::Label::builder()
            .label("Click a number, or drag a box around it.")
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        // Highlighted only while it says how a search ended: any new message clears it.
        status.connect_label_notify(|l| {
            for c in ["success", "news", "flash"] {
                l.remove_css_class(c);
            }
        });
        let crop = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::ScaleDown)
            .height_request(40)
            .width_request(120)
            .tooltip_text("What Ferret reads")
            .build();
        // A picture asks for its image's size: without a cap, a tall box drawn around a number
        // made the whole top row that tall and squeezed the game picture.
        let crop_box = adw::Clamp::builder()
            .orientation(gtk::Orientation::Vertical)
            .maximum_size(40)
            .valign(gtk::Align::Center)
            .child(&adw::Clamp::builder().maximum_size(160).child(&crop).build())
            .build();
        let spinner = gtk::Spinner::new();
        let stop = gtk::Button::builder().label("Stop").css_classes(["destructive-action"]).visible(false).build();
        let start = gtk::Button::builder().label("Start").css_classes(["suggested-action"]).sensitive(false).build();
        let start_over = gtk::Button::builder()
            .label("Start Over")
            .tooltip_text("Forget the matches so far and search from scratch")
            .build();
        let top = frame_box();
        top.set_margin_top(12);
        for w in [capture.upcast_ref::<gtk::Widget>(), zoom.upcast_ref(), status.upcast_ref(), crop_box.upcast_ref(), spinner.upcast_ref(), start_over.upcast_ref(), stop.upcast_ref(), start.upcast_ref()] {
            top.append(w);
        }

        let log = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(6)
            .bottom_margin(6)
            .left_margin(6)
            .right_margin(6)
            .build();
        let log_scroll = gtk::ScrolledWindow::builder()
            .child(&log)
            .height_request(110)
            .margin_start(12)
            .margin_end(12)
            .css_classes(["card"])
            .build();

        let name = gtk::Entry::builder().placeholder_text("Name, for example gems").hexpand(true).build();
        let save = gtk::Button::builder().label("Save").css_classes(["suggested-action"]).build();
        let found = gtk::Label::builder().label("Found it!").css_classes(["heading", "success"]).build();
        let result = frame_box();
        // Green, so it can't be missed (the user didn't notice a find, more than once).
        result.add_css_class("found-row");
        result.set_visible(false);
        result.append(&found);
        result.append(&name);
        result.append(&save);

        let typed_label = gtk::Label::builder()
            .label("Can't read it? Type the number the game shows:")
            .xalign(0.0)
            .hexpand(true)
            .wrap(true)
            .build();
        let typed = gtk::Entry::builder()
            .placeholder_text("Number")
            .input_purpose(gtk::InputPurpose::Number)
            .width_chars(10)
            .build();
        let typed_go = gtk::Button::builder().label("Search").build();
        let typed_row = frame_box();
        typed_row.append(&typed_label);
        typed_row.append(&typed);
        typed_row.append(&typed_go);

        let digits = gtk::Box::builder().spacing(2).build();
        let digits_hint = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).css_classes(["dim-label"]).build();
        let digits_row = frame_box();
        digits_row.append(&gtk::Label::new(Some("Learned digits:")));
        digits_row.append(&digits);
        digits_row.append(&digits_hint);

        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_bottom(12).build();
        page.append(&top);
        page.append(&scroll);
        page.append(&typed_row);
        page.append(&digits_row);
        page.append(&result);
        page.append(&log_scroll);

        let begin = gtk::Button::builder()
            .label("Show the Game Window")
            .halign(gtk::Align::Center)
            .css_classes(["pill", "suggested-action"])
            .build();
        let type_instead = gtk::Button::builder()
            .label("Type the Number Instead")
            .halign(gtk::Align::Center)
            .css_classes(["pill"])
            .build();
        let intro = adw::StatusPage::builder()
            .icon_name("edit-find-symbolic")
            .title("Find a Value")
            .description("Ferret looks at the game window, you pick the number, and then you just play. Only the game window is captured; the first time, your desktop asks which window to share.")
            .child(&{
                let buttons = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
                buttons.append(&begin);
                buttons.append(&type_instead);
                buttons
            })
            .build();
        let matches_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::None).css_classes(["boxed-list"]).build();
        let matches = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .visible(false)
            .build();
        matches.append(
            &gtk::Label::builder()
                .label("Ferret can't tell which of these is the value. Type a new value into one and press Try, then look at the game: if it shows that value, press Use This One.")
                .wrap(true)
                .xalign(0.0)
                .build(),
        );
        matches.append(&gtk::ScrolledWindow::builder().child(&matches_list).max_content_height(220).propagate_natural_height(true).build());
        page.append(&matches);
        page.reorder_child_after(&matches, Some(&result));
        let root = gtk::Stack::new();
        root.add_named(&intro, Some("intro"));
        root.add_named(&page, Some("pick"));

        let view = Rc::new(Self {
            root,
            picture,
            area,
            texture: RefCell::default(),
            words: RefCell::default(),
            selection: RefCell::default(),
            unconfirmed: RefCell::default(),
            drag: RefCell::default(),
            status,
            crop,
            start,
            stop,
            start_over,
            spinner,
            log,
            result,
            name,
            matches,
            matches_list,
            listed: RefCell::default(),
            typed_row,
            typed,
            typed_go: typed_go.clone(),
            last_count: Cell::default(),
            digits,
            digits_hint,
            worker,
            cancel,
        });

        for b in [&begin, &capture] {
            let view = view.clone();
            b.connect_clicked(move |_| view.capture());
        }
        {
            let view_ = view.clone();
            type_instead.connect_clicked(move |_| {
                view_.root.set_visible_child_name("pick");
                view_.status.set_label(if view_.selection.borrow().is_some() {
                    "Type the number the game shows, change it in the game, type the new one. Ferret learns the game's digits from it."
                } else {
                    "Click the number or drag a box around it, so Ferret can learn the game's digits. Then type the number the game shows, change it in the game, type the new one."
                });
                view_.typed.grab_focus();
            });
            let view_ = view.clone();
            typed_go.connect_clicked(move |_| view_.type_number(&view_.typed.text()));
            let view_ = view.clone();
            view.typed.connect_activate(move |e| view_.type_number(&e.text()));
        }
        {
            let view_ = view.clone();
            zoom.connect_toggled(move |z| {
                view_.picture.set_can_shrink(!z.is_active());
                view_.area.queue_draw();
            });
        }
        {
            let view_ = view.clone();
            view.start.connect_clicked(move |_| view_.start());
        }
        {
            let view_ = view.clone();
            view.stop.connect_clicked(move |_| view_.stop());
            let view_ = view.clone();
            view.start_over.connect_clicked(move |_| view_.start_over());
        }
        {
            let view_ = view.clone();
            save.connect_clicked(move |_| view_.save_as(&view_.name.text()));
            let view_ = view.clone();
            view.name.connect_activate(move |e| view_.save_as(&e.text()));
        }
        {
            let weak = Rc::downgrade(&view);
            view.area.set_draw_func(move |_, cr, _, _| {
                if let Some(view) = weak.upgrade() {
                    view.draw(cr);
                }
            });
        }
        let drag = gtk::GestureDrag::new();
        {
            let view_ = view.clone();
            drag.connect_drag_begin(move |_, x, y| *view_.drag.borrow_mut() = Some((x, y, x, y)));
            let view_ = view.clone();
            drag.connect_drag_update(move |_, dx, dy| {
                if let Some(d) = view_.drag.borrow_mut().as_mut() {
                    d.2 = d.0 + dx;
                    d.3 = d.1 + dy;
                }
                view_.area.queue_draw();
            });
            let view_ = view.clone();
            drag.connect_drag_end(move |_, _, _| view_.drag_end());
        }
        view.area.add_controller(drag);
        view
    }

    /// Scale and offset of the frame inside the picture widget.
    fn layout(&self) -> Option<(f64, f64, f64)> {
        let texture = self.texture.borrow();
        let t = texture.as_ref()?;
        let (iw, ih) = (t.width() as f64, t.height() as f64);
        let (w, h) = (self.picture.width() as f64, self.picture.height() as f64);
        let s = (w / iw).min(h / ih).min(1.0);
        Some((s, (w - iw * s) / 2.0, (h - ih * s) / 2.0))
    }

    fn to_widget(&self, r: Rect) -> Option<(f64, f64, f64, f64)> {
        let (s, ox, oy) = self.layout()?;
        Some((ox + r.x as f64 * s, oy + r.y as f64 * s, r.w as f64 * s, r.h as f64 * s))
    }

    fn to_image(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (s, ox, oy) = self.layout()?;
        Some(((x - ox) / s, (y - oy) / s))
    }

    fn draw(&self, cr: &gtk::cairo::Context) {
        let rect = |r: (f64, f64, f64, f64)| cr.rectangle(r.0 - 2.0, r.1 - 2.0, r.2 + 4.0, r.3 + 4.0);
        cr.set_line_width(1.5);
        cr.set_source_rgba(0.21, 0.52, 0.89, 0.8);
        for w in self.words.borrow().iter() {
            if let Some(r) = self.to_widget(w.rect) {
                rect(r);
            }
        }
        cr.stroke().ok();
        cr.set_line_width(3.0);
        cr.set_source_rgba(0.9, 0.38, 0.0, 1.0);
        if let Some(r) = self.selection.borrow().and_then(|r| self.to_widget(r)) {
            rect(r);
            cr.stroke().ok();
        }
        if let Some((x0, y0, x1, y1)) = *self.drag.borrow() {
            cr.rectangle(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
            cr.stroke().ok();
        }
    }

    fn drag_end(&self) {
        let Some((x0, y0, x1, y1)) = self.drag.borrow_mut().take() else { return };
        let (Some(a), Some(b)) = (self.to_image(x0.min(x1), y0.min(y1)), self.to_image(x0.max(x1), y0.max(y1))) else {
            return;
        };
        if (x1 - x0).abs() < 4.0 && (y1 - y0).abs() < 4.0 {
            // A click: pick the detected number under the pointer.
            let hit = self.words.borrow().iter().find(|w| {
                let r = w.rect;
                let pad = 4.0;
                a.0 >= r.x as f64 - pad && a.0 <= (r.x + r.w) as f64 + pad && a.1 >= r.y as f64 - pad && a.1 <= (r.y + r.h) as f64 + pad
            }).map(|w| ocr::watch_area(w.rect));
            match hit {
                Some(area) => self.select(area),
                None => self.area.queue_draw(),
            }
            return;
        }
        let area = Rect {
            x: a.0.max(0.0) as u32,
            y: a.1.max(0.0) as u32,
            w: (b.0 - a.0).max(1.0) as u32,
            h: (b.1 - a.1).max(1.0) as u32,
        };
        self.select(area);
    }

    /// A log line in bold green, for finds.
    fn log_found(&self, msg: &str) {
        let buffer = self.log.buffer();
        if buffer.tag_table().lookup("found").is_none() {
            buffer.tag_table().add(&gtk::TextTag::builder().name("found").foreground("#2ec27e").weight(700).build());
        }
        buffer.insert_with_tags_by_name(&mut buffer.end_iter(), &format!("{msg}\n"), &["found"]);
        let mark = buffer.create_mark(None, &buffer.end_iter(), false);
        self.log.scroll_mark_onscreen(&mark);
        buffer.delete_mark(&mark);
    }

    pub fn log(&self, msg: &str) {
        let buffer = self.log.buffer();
        buffer.insert(&mut buffer.end_iter(), &format!("{}\n", group_counts(msg)));
        let mark = buffer.create_mark(None, &buffer.end_iter(), false);
        self.log.scroll_mark_onscreen(&mark);
        buffer.delete_mark(&mark);
    }

    fn busy(&self, busy: bool) {
        self.spinner.set_spinning(busy);
    }

    /// How a search ended, highlighted (green for a find) and flashed brightly at first, so the
    /// player sees it changed even when watching the game.
    fn announce(&self, msg: &str, found: bool) {
        self.status.set_label(msg);
        self.status.add_css_class("news");
        if found {
            self.status.add_css_class("success");
        }
        self.status.add_css_class("flash");
        let status = self.status.clone();
        glib::timeout_add_local_once(Duration::from_millis(700), move || status.remove_css_class("flash"));
    }

    /// "1,200 places match", or "1,200 -> 35 places match" when an earlier search had more.
    fn count_text(&self, n: usize) -> String {
        let text = match self.last_count.replace(Some(n)) {
            Some(before) if before != n => format!("{} \u{2192} {}", grouped(before), grouped(n)),
            Some(_) => format!("Still {}", grouped(n)),
            None => grouped(n),
        };
        format!("{text} {}", if n == 1 { "place matches" } else { "places match" })
    }

    /// The typed number stays readable while it's searched for, in the accent colour.
    fn typed_searching(&self, searching: bool) {
        self.typed.set_editable(!searching);
        self.typed_go.set_sensitive(!searching);
        if searching {
            self.typed.add_css_class("searching");
        } else {
            self.typed.remove_css_class("searching");
        }
    }

    pub fn capture(&self) {
        self.busy(true);
        self.status.set_label("Capturing the game window…");
        self.worker.run(|core| Event::Numbers(core.numbers()));
    }

    pub fn numbers(&self, r: Result<(PathBuf, Vec<Word>), String>) {
        self.busy(false);
        match r.and_then(|(frame, words)| {
            gdk::Texture::from_filename(&frame).map(|t| (t, words)).map_err(|e| e.to_string())
        }) {
            Ok((texture, words)) => {
                self.picture.set_paintable(Some(&texture));
                *self.texture.borrow_mut() = Some(texture);
                self.log(&format!("{} numbers found on screen", words.len()));
                *self.words.borrow_mut() = words;
                self.root.set_visible_child_name("pick");
                self.status.set_label("Click a number, or drag a box around it.");
                self.area.queue_draw();
            }
            Err(e) => {
                self.status.set_label(&format!("Could not capture the window: {e}"));
                self.log(&e);
            }
        }
    }

    pub fn select(&self, area: Rect) {
        self.matches.set_visible(false);
        *self.selection.borrow_mut() = Some(area);
        self.area.queue_draw();
        self.busy(true);
        self.status.set_label("Reading…");
        self.worker.run(move |core| {
            core.set_area(area);
            let read = core.read_picked();
            Event::Read(read, core.watched())
        });
    }

    pub fn read(&self, r: Result<Option<(Shown, bool)>, String>, area: Option<Rect>) {
        self.busy(false);
        // Show where Ferret now watches: the box snaps to the number it found.
        if area.is_some() {
            *self.selection.borrow_mut() = area;
        }
        if let Ok(t) = gdk::Texture::from_filename(core::cache_dir().join("area.png")) {
            self.crop.set_paintable(Some(&t));
        }
        // The read comes from a fresh frame: show that one, the game may have changed since the
        // capture (Forager's furnace used up ore while the player picked numbers).
        if let Ok(t) = gdk::Texture::from_filename(core::cache_dir().join("picked.png")) {
            self.picture.set_paintable(Some(&t));
            *self.texture.borrow_mut() = Some(t);
            self.area.queue_draw();
        }
        self.unconfirmed.replace(None);
        self.start.set_label("Start");
        match r {
            Ok(Some((n, true))) => {
                self.status.set_label(&format!("Reads {n}. Press Start, then play until the number changes a couple of times."));
                self.start.set_sensitive(true);
            }
            // Tesseract's guess: ask, and learn the game's digits from the answer.
            Ok(Some((n, false))) => {
                self.status.set_label(&format!("Reads {n}. Is that what the game shows? If not, type the right number below."));
                self.unconfirmed.replace(Some(n));
                self.start.set_label("Yes, Start");
                self.start.set_sensitive(true);
            }
            Ok(None) => {
                self.status.set_label("Can't read a number there. Type the number the game shows below; Ferret learns the game's digits from it. Or try a tighter box.");
                self.start.set_sensitive(false);
            }
            Err(e) => self.status.set_label(&e),
        }
    }

    pub fn start(&self) {
        self.matches.set_visible(false);
        self.result.set_visible(false);
        self.typed_row.set_sensitive(false);
        self.start.set_visible(false);
        self.start_over.set_visible(false);
        self.stop.set_visible(true);
        self.busy(true);
        self.status.set_label("Watching the number. Play normally; every change narrows it down.");
        self.start.set_label("Start");
        let confirmed = self.unconfirmed.take();
        self.worker.run(move |core| {
            if let Some(n) = confirmed {
                core.confirm(&n);
            }
            Event::Auto(core.auto(Duration::from_secs(600)))
        });
    }

    pub fn auto_done(&self, r: Result<AutoResult, String>) {
        self.busy(false);
        self.stop.set_visible(false);
        self.start.set_visible(true);
        self.start_over.set_visible(true);
        self.typed_row.set_sensitive(true);
        match r {
            Ok(AutoResult::Found(loc)) => self.found(loc),
            Ok(AutoResult::Several(n)) => {
                self.announce(
                    &format!("{}. Press Start to continue and let the number change a few more times.", self.count_text(n)),
                    false,
                );
                self.list_matches(n);
            }
            Err(e) => self.status.set_label(&e),
        }
    }

    fn found(&self, loc: core::Loc) {
        self.matches.set_visible(false);
        let at = format!("0x{:x} ({})", loc.addr, loc.kind.describe());
        self.last_count.set(None);
        self.announce(&format!("Found it! It's at {at}. Give it a name below to keep it."), true);
        self.log_found(&format!("Found it: {at}"));
        self.result.set_visible(true);
        self.name.grab_focus();
    }

    pub fn type_number(&self, text: &str) {
        if !self.typed.is_editable() {
            return;
        }
        // The D-Bus `type` action doesn't go through the entry.
        if self.typed.text() != text {
            self.typed.set_text(text);
        }
        self.matches.set_visible(false);
        let Some(n) = Shown::parse(text) else {
            self.status.set_label("Type the number as the game shows it, for example 1250, 1.5 or 3:17.");
            return;
        };
        self.result.set_visible(false);
        self.typed_searching(true);
        self.unconfirmed.replace(None);
        self.start.set_label("Start");
        self.busy(true);
        self.status.set_label(&format!("Looking for {n}…"));
        self.worker.run(move |core| Event::Typed(core.typed(n)));
    }

    pub fn typed_done(&self, r: Result<AutoResult, String>) {
        self.busy(false);
        self.typed_searching(false);
        self.typed.set_text("");
        match r {
            Ok(AutoResult::Found(loc)) => self.found(loc),
            Ok(AutoResult::Several(n)) => {
                self.announce(&format!("{}. Change the number in the game, then type the new one.", self.count_text(n)), false);
                self.list_matches(n);
                self.typed.grab_focus();
            }
            Err(e) => self.status.set_label(&e),
        }
    }

    /// Shows the attached game's learned digits (index = digit); click one to forget it.
    pub fn show_digits(&self, shapes: Vec<Vec<DigitShape>>) {
        while let Some(c) = self.digits.first_child() {
            self.digits.remove(&c);
        }
        let missing: Vec<String> = (0..shapes.len()).filter(|&d| shapes[d].is_empty()).map(|d| d.to_string()).collect();
        for (d, shapes) in shapes.into_iter().enumerate() {
            let Some(first) = shapes.first().cloned() else {
                let unknown = gtk::Label::builder()
                    .label("?")
                    .width_chars(2)
                    .css_classes(["dim-label"])
                    .tooltip_text(format!("{d} isn't learned yet"))
                    .build();
                self.digits.append(&unknown);
                continue;
            };
            let area = gtk::DrawingArea::builder().content_width(14).content_height(20).build();
            area.set_draw_func(move |a, cr, w, h| draw_shape(a, cr, w, h, &first));
            let n = shapes.len();
            let what = format!("{n} learned shape{} of {d}", if n == 1 { "" } else { "s" });
            let forget = gtk::Button::builder().label(format!("Forget {d}")).css_classes(["destructive-action"]).build();
            let content = gtk::Box::builder()
                .orientation(gtk::Orientation::Vertical)
                .spacing(8)
                .margin_top(6)
                .margin_bottom(6)
                .margin_start(6)
                .margin_end(6)
                .build();
            content.append(&gtk::Label::new(Some(&format!("{what}.\nForget it if it reads numbers wrong,\nthen type a number with a {d} to learn it again."))));
            content.append(&forget);
            let popover = gtk::Popover::builder().child(&content).build();
            let button = gtk::MenuButton::builder().child(&area).popover(&popover).css_classes(["flat"]).tooltip_text(&what).build();
            let worker = self.worker.clone();
            forget.connect_clicked(move |_| {
                popover.popdown();
                worker.run(move |core| {
                    core.forget_digit(d as u8).ok();
                    Event::Digits(core.digits())
                });
            });
            self.digits.append(&button);
        }
        self.digits_hint.set_label(&match missing.len() {
            10 => "None yet: Tesseract reads the numbers. Typing the number the game shows teaches Ferret its digits.".to_owned(),
            0 => "All ten: while searching, Ferret only trusts reads made with these.".to_owned(),
            _ => format!(
                "Missing {}: Tesseract fills in, checked against the known ones. Type a number that has {} once.",
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" }
            ),
        });
    }

    /// Stops a running Start (the matches so far are kept).
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Forgets the matches so far; the picked number and the captured frame stay.
    pub fn start_over(&self) {
        self.last_count.set(None);
        self.matches.set_visible(false);
        self.result.set_visible(false);
        self.typed.set_text("");
        self.status.set_label(if self.selection.borrow().is_some() {
            "Starting over. Press Start, or type the number the game shows."
        } else {
            "Starting over. Click a number, or type the number the game shows."
        });
        self.worker.run(|core| {
            core.reset();
            Event::Reset
        });
    }

    /// Attached to another game: nothing picked or found in the last one applies.
    pub fn new_game(&self) {
        self.last_count.set(None);
        self.matches.set_visible(false);
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.crop.set_paintable(None::<&gdk::Paintable>);
        *self.texture.borrow_mut() = None;
        self.words.borrow_mut().clear();
        *self.selection.borrow_mut() = None;
        self.unconfirmed.replace(None);
        self.result.set_visible(false);
        self.typed.set_text("");
        self.start.set_label("Start");
        self.start.set_sensitive(false);
        self.status.set_label("Click a number, or drag a box around it.");
        self.root.set_visible_child_name("intro");
    }

    /// Lists the places still matching, each with Try (write a value) and Use This One.
    pub fn show_matches(&self, list: Vec<(core::Loc, String)>) {
        while let Some(row) = self.matches_list.first_child() {
            self.matches_list.remove(&row);
        }
        self.matches.set_visible(!list.is_empty());
        *self.listed.borrow_mut() = list.iter().map(|(l, _)| *l).collect();
        for (loc, value) in list {
            let row = adw::ActionRow::builder().title(&value).subtitle(format!("0x{:x}, {}", loc.addr, loc.kind.describe())).build();
            let entry = gtk::Entry::builder().placeholder_text("New value").width_chars(8).valign(gtk::Align::Center).build();
            let try_it = gtk::Button::builder().label("Try").valign(gtk::Align::Center).build();
            let pick = gtk::Button::builder().label("Use This One").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
            row.add_suffix(&entry);
            row.add_suffix(&try_it);
            row.add_suffix(&pick);
            let apply = {
                let (worker, entry) = (self.worker.clone(), entry.clone());
                move || {
                    let v = entry.text().to_string();
                    if !v.trim().is_empty() {
                        worker.run(move |core| Event::Matches(core.try_match(loc, &v)));
                    }
                }
            };
            {
                let apply = apply.clone();
                try_it.connect_clicked(move |_| apply());
            }
            entry.connect_activate(move |_| apply());
            let worker = self.worker.clone();
            pick.connect_clicked(move |_| worker.run(move |core| Event::Chosen(core.choose(loc))));
            self.matches_list.append(&row);
        }
    }

    /// With few places left, lists them: the player can try them out when Ferret can't tell.
    fn list_matches(&self, n: usize) {
        if n <= 20 {
            self.worker.run(|core| Event::Matches(Ok(core.matches())));
        } else {
            self.matches.set_visible(false);
        }
    }

    /// Debug actions: Try / Use This One on the listed place at `i`.
    pub fn try_listed(&self, i: usize, value: String) {
        if let Some(&loc) = self.listed.borrow().get(i) {
            self.worker.run(move |core| Event::Matches(core.try_match(loc, &value)));
        }
    }

    pub fn use_listed(&self, i: usize) {
        if let Some(&loc) = self.listed.borrow().get(i) {
            self.worker.run(move |core| Event::Chosen(core.choose(loc)));
        }
    }

    /// The player picked the value among the matches.
    pub fn chosen(&self, loc: core::Loc) {
        self.matches.set_visible(false);
        self.found(loc);
    }

    /// The game as the search last saw it, with the watched box where it is now.
    pub fn show_frame(&self, texture: gdk::Texture, area: Option<Rect>) {
        self.picture.set_paintable(Some(&texture));
        *self.texture.borrow_mut() = Some(texture);
        if area.is_some() {
            *self.selection.borrow_mut() = area;
        }
        self.area.queue_draw();
    }

    pub fn save_as(&self, name: &str) {
        let name = crate::core::one_word(name);
        // One save at a time: a second Enter or click while it runs would save it all over again.
        if name.is_empty() || !self.result.is_sensitive() {
            return;
        }
        self.result.set_sensitive(false);
        self.busy(true);
        self.status.set_label("Saving: finding how the game gets to it (takes about 10 seconds)…");
        self.worker.run(move |core| Event::Saved(core.save(&name).map(|confirmed| (name, confirmed))));
    }

    pub fn saved(&self) {
        self.busy(false);
        self.result.set_sensitive(true);
        self.result.set_visible(false);
        self.name.set_text("");
        self.status.set_label("Saved. Pick another number to find more.");
    }

    pub fn save_failed(&self, e: &str) {
        self.busy(false);
        self.result.set_sensitive(true);
        self.status.set_label(&format!("Not saved: {e}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn groups_match_counts_only() {
        assert_eq!(grouped(0), "0");
        assert_eq!(grouped(999), "999");
        assert_eq!(grouped(5040383), "5,040,383");
        assert_eq!(
            group_counts("screen shows 1250: 5040383 -> 12000 matches (12000 i32, 0 f32, 1500 f64, 0 xor)"),
            "screen shows 1250: 5,040,383 -> 12,000 matches (12,000 i32, 0 f32, 1,500 f64, 0 xor)"
        );
        assert_eq!(
            group_counts("typed 1037: 1234567 matches (1234567 i32, 0 f32, 0 f64, 0 xor) in 1200 MiB (0 MiB unreadable) in 1500 ms"),
            "typed 1037: 1,234,567 matches (1,234,567 i32, 0 f32, 0 f64, 0 xor) in 1200 MiB (0 MiB unreadable) in 1500 ms"
        );
        assert_eq!(group_counts("wrote 20000, reads back 0x00000e4ff978:f64 = 20000"), "wrote 20000, reads back 0x00000e4ff978:f64 = 20000");
    }
}
