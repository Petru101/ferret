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
use gtk::gdk;

use super::{Event, Worker};
use crate::core::{self, AutoResult};
use crate::ocr::{self, Rect, Word};

pub struct FindView {
    pub root: gtk::Stack,
    picture: gtk::Picture,
    area: gtk::DrawingArea,
    texture: RefCell<Option<gdk::Texture>>,
    words: RefCell<Vec<Word>>,
    selection: RefCell<Option<Rect>>,
    /// A read the player hasn't confirmed yet (Tesseract's, not the learned digits').
    unconfirmed: Cell<Option<i64>>,
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
    typed_row: gtk::Box,
    typed: gtk::Entry,
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
        let found = gtk::Label::builder().label("Found it.").css_classes(["heading"]).build();
        let result = frame_box();
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

        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_bottom(12).build();
        page.append(&top);
        page.append(&scroll);
        page.append(&typed_row);
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
            unconfirmed: Cell::default(),
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
            typed_row,
            typed,
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
        *self.selection.borrow_mut() = Some(area);
        self.area.queue_draw();
        self.busy(true);
        self.status.set_label("Reading…");
        self.worker.run(move |core| {
            core.set_area(area);
            Event::Read(core.read_picked())
        });
    }

    pub fn read(&self, r: Result<Option<(i64, bool)>, String>) {
        self.busy(false);
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
        self.unconfirmed.set(None);
        self.start.set_label("Start");
        match r {
            Ok(Some((n, true))) => {
                self.status.set_label(&format!("Reads {n}. Press Start, then play until the number changes a couple of times."));
                self.start.set_sensitive(true);
            }
            // Tesseract's guess: ask, and learn the game's digits from the answer.
            Ok(Some((n, false))) => {
                self.status.set_label(&format!("Reads {n}. Is that what the game shows? If not, type the right number below."));
                self.unconfirmed.set(Some(n));
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
                core.confirm(n);
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
            Ok(AutoResult::Several(n)) => self.status.set_label(&format!(
                "{} places still match. Press Start to continue and let the number change a few more times.",
                grouped(n)
            )),
            Err(e) => self.status.set_label(&e),
        }
    }

    fn found(&self, loc: core::Loc) {
        self.status.set_label(&format!(
            "Found it at 0x{:x} ({}). Give it a name to keep it.",
            loc.addr,
            loc.kind.describe()
        ));
        self.result.set_visible(true);
        self.name.grab_focus();
    }

    pub fn type_number(&self, text: &str) {
        let Ok(n) = text.trim().parse::<i64>() else {
            self.status.set_label("Type the number as digits only, for example 1250.");
            return;
        };
        self.result.set_visible(false);
        self.typed_row.set_sensitive(false);
        self.unconfirmed.set(None);
        self.start.set_label("Start");
        self.busy(true);
        self.status.set_label(&format!("Looking for {n}…"));
        self.worker.run(move |core| Event::Typed(core.typed(n)));
    }

    pub fn typed_done(&self, r: Result<AutoResult, String>) {
        self.busy(false);
        self.typed_row.set_sensitive(true);
        self.typed.set_text("");
        match r {
            Ok(AutoResult::Found(loc)) => self.found(loc),
            Ok(AutoResult::Several(n)) => {
                self.status.set_label(&format!(
                    "{} places match. Change the number in the game, then type the new one.",
                    grouped(n)
                ));
                self.typed.grab_focus();
            }
            Err(e) => self.status.set_label(&e),
        }
    }

    /// Stops a running Start (the matches so far are kept).
    pub fn stop(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Forgets the matches so far; the picked number and the captured frame stay.
    pub fn start_over(&self) {
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
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.crop.set_paintable(None::<&gdk::Paintable>);
        *self.texture.borrow_mut() = None;
        self.words.borrow_mut().clear();
        *self.selection.borrow_mut() = None;
        self.unconfirmed.set(None);
        self.result.set_visible(false);
        self.typed.set_text("");
        self.start.set_label("Start");
        self.start.set_sensitive(false);
        self.status.set_label("Click a number, or drag a box around it.");
        self.root.set_visible_child_name("intro");
    }

    pub fn save_as(&self, name: &str) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.busy(true);
        self.status.set_label("Saving: finding how the game gets to it (takes about 10 seconds)…");
        self.worker.run(move |core| Event::Saved(core.save(&name).map(|confirmed| (name, confirmed))));
    }

    pub fn saved(&self) {
        self.busy(false);
        self.result.set_visible(false);
        self.name.set_text("");
        self.status.set_label("Saved. Pick another number to find more.");
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
