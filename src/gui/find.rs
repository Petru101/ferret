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
use gtk::{gdk, gio, glib};

use super::guide::{self, Guide, Look};
use super::{Event, Worker};
use crate::bar::Kind as GaugeKind;
use crate::core::{self, AutoResult, Kind};
use crate::font::DigitShape;
use crate::ocr::{self, Rect, Shown, Word};

pub struct FindView {
    pub root: gtk::Stack,
    picture: gtk::Picture,
    /// Around the picture: scrolls it while zoomed in.
    scroll: gtk::ScrolledWindow,
    /// How much the picture is zoomed (pixels on screen per pixel of the frame); 0 = fitted to
    /// the window (never above actual size). Scroll to zoom: game numbers were too small to
    /// pick at the fitted size.
    zoom: Cell<f64>,
    /// Where the pointer is over `scroll`: zooming keeps that spot under it.
    pointer: Cell<(f64, f64)>,
    /// Right-dragging the picture: the scroll positions when the drag began.
    pan: Cell<Option<(f64, f64)>>,
    area: gtk::DrawingArea,
    texture: RefCell<Option<gdk::Texture>>,
    words: RefCell<Vec<Word>>,
    selection: RefCell<Option<Rect>>,
    /// Over the game picture while no number is picked (the user, typing numbers, forgot to
    /// pick the only number on screen and wondered why nothing read it).
    pick_hint: gtk::Label,
    /// The selection was made by Ferret (the only number found), not the player.
    /// What the last read of the box saw, drawn next to it (None inside: no number there).
    seen: RefCell<Option<Option<String>>>,
    /// A read the player hasn't confirmed yet (PaddleOCR's, not the learned digits'), and the
    /// dialog asking about it. Start waits for the answer.
    unconfirmed: RefCell<Option<Shown>>,
    question: RefCell<Option<adw::AlertDialog>>,
    /// Drag start and current point, in widget coordinates.
    drag: RefCell<Option<(f64, f64, f64, f64)>>,
    crop: gtk::Picture,
    /// Forgets the picked box (`unpick`).
    unpick: gtk::Button,
    start: gtk::Button,
    stop: gtk::Button,
    start_over: gtk::Button,
    undo: gtk::Button,
    redo: gtk::Button,
    /// The value types new searches look for (`TYPE_CHOICES`).
    types: gtk::DropDown,
    /// The instruction bar: what Ferret is doing and what the player should do now.
    guide: Rc<Guide>,
    log: gtk::TextView,
    /// The last log line and how many times in a row it came: repeats update that line.
    last_log: RefCell<(String, usize)>,
    /// The save row (name + Save), in the instruction bar once a value is found.
    result: gtk::Box,
    name: gtk::Entry,
    /// Under the save row: what's saved for this game already, so a name isn't reused by
    /// mistake (Prey: the shotgun's ammo was saved as "ammo", over the pistol's, and took its
    /// 15-15 limit with it).
    saved_box: gtk::Box,
    saved_grid: RefCell<gtk::Grid>,
    /// The saved values as listed (name, value, limit), to redraw only on a change.
    saved_rows: RefCell<Vec<(String, String, String)>>,
    replaces: gtk::Label,
    /// "Searches look first in places shaped like earlier finds" + Forget Them: a shape kept
    /// pointing Prey's shotgun searches at the pistol's kind of place.
    shapes_row: gtk::Box,
    shapes_label: gtk::Label,
    save: gtk::Button,
    /// The places still matching when Ferret couldn't tell which is the value: the player
    /// changes them and watches the game.
    matches: gtk::Box,
    matches_list: gtk::ListBox,
    /// The places listed there, in order (for the D-Bus debug actions).
    listed: RefCell<Vec<core::Loc>>,
    /// Their rows, updated in place while the same places are listed (a value being typed into
    /// one stays).
    match_rows: RefCell<Vec<adw::ActionRow>>,
    /// The places left as last listed (even while the list is hidden): trying them one at a
    /// time starts from these.
    last_list: RefCell<Vec<core::Match>>,
    /// Trying the places one at a time: the places (best guesses first) and which one now.
    one_by_one: RefCell<Option<(Vec<core::Match>, usize)>>,
    /// The number written to the place tried now.
    trying: RefCell<String>,
    /// The bar's buttons when Ferret can't tell which place it is: Try Them One by One, Keep
    /// Going, Show the List.
    call_box: gtk::Box,
    keep_going: gtk::Button,
    show_list: gtk::ToggleButton,
    /// Trying a place: the number to write, Try It, Skip, Stop Trying.
    try_box: gtk::Box,
    try_entry: gtk::Entry,
    /// After a try: Yes, That's It / No, Next Place.
    ask_box: gtk::Box,
    typed: gtk::Entry,
    typed_go: gtk::Button,
    /// The last number typed: Scan Again offers it when there's no box to read.
    last_typed: RefCell<String>,
    /// How many places matched after the last search, to show how far a search narrowed it.
    last_count: Cell<Option<usize>>,
    digits: gtk::Box,
    digits_hint: gtk::Label,
    worker: Worker,
    cancel: Arc<AtomicBool>,
    scan_now: Arc<AtomicBool>,
    again: gtk::Button,
}

/// A number to try on a place: half what it holds (easy to spot, and under any cap the game
/// puts on it: a capped number set higher is put back at once, which rules the place out).
fn test_number(value: &str) -> String {
    match value.trim().parse::<f64>() {
        Ok(v) if v.abs() < 2.0 => format!("{}", v.round() as i64 + 10),
        Ok(v) if v.fract() == 0.0 => format!("{}", (v / 2.0).floor() as i64),
        Ok(v) => format!("{:.1}", v / 2.0),
        Err(_) => String::new(),
    }
}

/// "1 place", "7 places".
fn places_text(n: usize) -> String {
    format!("{} {}", grouped(n), if n == 1 { "place" } else { "places" })
}

/// 5040383 -> "5,040,383".
pub(super) fn grouped(n: usize) -> String {
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

/// The value types a search can be narrowed to, as Cheat Engine offers them (none: all).
const TYPE_CHOICES: [(&str, &[Kind]); 6] = [
    ("All Types", &[]),
    ("2 Bytes", &[Kind::U16]),
    ("4 Bytes", &[Kind::I32]),
    ("Float", &[Kind::F32]),
    ("Double", &[Kind::F64]),
    ("Encoded 4 Bytes", &[Kind::Xor]),
];

fn frame_box() -> gtk::Box {
    gtk::Box::builder().spacing(12).margin_start(12).margin_end(12).build()
}

/// Forgets the shapes of earlier finds (the Forget Them button, and the D-Bus `forget-shapes`).
pub fn forget_shapes(worker: &Worker) {
    worker.run(|core| {
        let n = core.forget_shapes();
        Event::Done(n.map(|n| format!("Forgot {}: searches look everywhere", core::kinds_of_places(n))))
    });
    worker.run(|core| Event::Shapes(core.shape_count()));
}

/// Asks before ruling out one of the places still matching (only Start Over brings it back).
fn ask_drop(worker: &Worker, loc: core::Loc, value: &str, parent: &impl IsA<gtk::Widget>) {
    let dialog = adw::AlertDialog::new(
        Some("Rule Out This Place?"),
        Some(&format!(
            "0x{:x} ({}, now {value}) is dropped from the matches. Undo brings it back.",
            loc.addr,
            loc.kind.describe()
        )),
    );
    dialog.add_responses(&[("cancel", "Cancel"), ("drop", "Rule Out")]);
    dialog.set_response_appearance("drop", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let worker = worker.clone();
    dialog.connect_response(Some("drop"), move |_, _| worker.run(move |core| Event::Matches(core.drop_match(loc))));
    dialog.present(Some(parent));
}

impl FindView {
    pub fn new(worker: Worker, cancel: Arc<AtomicBool>, scan_now: Arc<AtomicBool>, guide: Rc<Guide>) -> Rc<Self> {
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
        let pick_hint = gtk::Label::builder()
            .label("Click the number you want to change, or drag a box around it")
            .wrap(true)
            .justify(gtk::Justification::Center)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Start)
            .margin_top(12)
            .margin_start(12)
            .margin_end(12)
            .can_target(false)
            .visible(false)
            .css_classes(["pick-hint"])
            .build();
        overlay.add_overlay(&pick_hint);
        let scroll = gtk::ScrolledWindow::builder().child(&overlay).vexpand(true).build();

        // The menu: the window picked by mistake (the choice is remembered across restarts).
        let window_menu = gio::Menu::new();
        // The app's actions all take a string (D-Bus test actions): without one the item is off.
        let repick = gio::MenuItem::new(Some("Pick Another Window…"), None);
        repick.set_action_and_target_value(Some("app.repick"), Some(&"".to_variant()));
        window_menu.append_item(&repick);
        let capture = adw::SplitButton::builder()
            .icon_name("camera-photo-symbolic")
            .tooltip_text("Capture again")
            .dropdown_tooltip("Watch another window")
            .menu_model(&window_menu)
            .build();
        let zoom = gtk::Button::builder()
            .icon_name("zoom-fit-best-symbolic")
            .tooltip_text("Fit the picture in the window (scroll over it to zoom, drag with the right button to move it)")
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
        let unpick = gtk::Button::builder()
            .icon_name("edit-clear-symbolic")
            .tooltip_text("Forget the picked box, to search for a number the game doesn't show by typing it")
            .css_classes(["flat"])
            .valign(gtk::Align::Center)
            .visible(false)
            .build();
        // Start and Stop sit in the instruction bar, next to what it says to do.
        let stop = gtk::Button::builder().label("Stop").css_classes(["destructive-action", "guide-button"]).visible(false).build();
        let start = gtk::Button::builder().label("Start").css_classes(["suggested-action", "guide-button"]).sensitive(false).build();
        let start_over = gtk::Button::builder()
            .label("Start Over")
            .tooltip_text("Forget the matches so far and search from scratch")
            .build();
        let again = gtk::Button::builder()
            .label("Scan Again")
            .tooltip_text("Keep only the places that hold the number the game shows now, even if it didn't change")
            .visible(false)
            .build();
        let undo = gtk::Button::builder()
            .icon_name("edit-undo-symbolic")
            .tooltip_text("Undo the last step of the search: a number, a ruled-out place or Start Over")
            .build();
        let redo = gtk::Button::builder()
            .icon_name("edit-redo-symbolic")
            .tooltip_text("Redo the last step Undo took back")
            .build();
        let types = gtk::DropDown::builder()
            .model(&gtk::StringList::new(&TYPE_CHOICES.iter().map(|(label, _)| *label).collect::<Vec<_>>()))
            .tooltip_text("What new searches look for. Fewer types leave fewer places to narrow down.")
            .valign(gtk::Align::Center)
            .build();

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
        // Next to the picture, always in view: the player reads it to see what Ferret does.
        let log_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).width_request(300).css_classes(["card"]).build();
        log_box.append(
            &gtk::Label::builder()
                .label("What Ferret is doing")
                .xalign(0.0)
                .margin_start(10)
                .margin_top(6)
                .margin_bottom(6)
                .css_classes(["caption-heading", "dim-label"])
                .build(),
        );
        log_box.append(&gtk::ScrolledWindow::builder().child(&log).vexpand(true).build());

        let name = gtk::Entry::builder().placeholder_text("Name it, for example gems").width_chars(24).build();
        let save = gtk::Button::builder().label("Save").css_classes(["suggested-action", "guide-button"]).build();
        let result = gtk::Box::builder().spacing(8).visible(false).build();
        result.append(&name);
        result.append(&save);
        guide.actions.append(&result);
        guide.actions.append(&stop);
        // Start shows only while it's the player's pick (a greyed Start sat next to every other
        // step); the slot hides it, so `start.is_visible()` still says whether it could run.
        let start_slot = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        start_slot.append(&start);
        guide.actions.append(&start_slot);
        guide.on_show(move |look| start_slot.set_visible(look == Look::Pick));

        let one_by_one = gtk::Button::builder().label("Try Them One by One").css_classes(["suggested-action", "guide-button"]).build();
        let keep_going = gtk::Button::builder()
            .label("Keep Going")
            .tooltip_text("Change the number in the game again: often the quicker way")
            .css_classes(["guide-button"])
            .build();
        let show_list = gtk::ToggleButton::builder().label("Show the List").css_classes(["flat"]).build();
        let call_box = gtk::Box::builder().spacing(8).visible(false).build();
        call_box.append(&show_list);
        call_box.append(&keep_going);
        call_box.append(&one_by_one);
        guide.actions.append(&call_box);

        let try_entry = gtk::Entry::builder()
            .input_purpose(gtk::InputPurpose::Number)
            .width_chars(8)
            .valign(gtk::Align::Center)
            .tooltip_text("The number to write there")
            .build();
        let try_it = gtk::Button::builder().label("Try It").css_classes(["suggested-action", "guide-button"]).build();
        let skip = gtk::Button::builder().label("Skip").css_classes(["guide-button"]).build();
        let stop_trying = gtk::Button::builder().label("Stop Trying").css_classes(["flat"]).build();
        let try_box = gtk::Box::builder().spacing(8).visible(false).build();
        try_box.append(&stop_trying);
        try_box.append(&skip);
        try_box.append(&try_entry);
        try_box.append(&try_it);
        guide.actions.append(&try_box);

        let no = gtk::Button::builder().label("No, Next Place").css_classes(["guide-button"]).build();
        let yes = gtk::Button::builder().label("Yes, That's It").css_classes(["suggested-action", "guide-button"]).build();
        let ask_box = gtk::Box::builder().spacing(8).visible(false).build();
        ask_box.append(&no);
        ask_box.append(&yes);
        guide.actions.append(&ask_box);

        let typed_label = gtk::Label::builder().label("Can't read it?").build();
        let typed = gtk::Entry::builder()
            .placeholder_text("Type the number")
            .input_purpose(gtk::InputPurpose::Number)
            .width_chars(14)
            .valign(gtk::Align::Center)
            .build();
        let typed_go = gtk::Button::builder().label("Search").valign(gtk::Align::Center).build();
        let more_button = gtk::ToggleButton::builder()
            .label("More")
            .tooltip_text("Value types, the game's learned digits and the places of earlier finds")
            .valign(gtk::Align::Center)
            .build();
        let tools = frame_box();
        for w in [capture.upcast_ref::<gtk::Widget>(), zoom.upcast_ref(), crop_box.upcast_ref(), unpick.upcast_ref(), undo.upcast_ref(), redo.upcast_ref(), again.upcast_ref(), start_over.upcast_ref()] {
            tools.append(w);
        }
        tools.append(&gtk::Box::builder().hexpand(true).build());
        for w in [typed_label.upcast_ref::<gtk::Widget>(), typed.upcast_ref(), typed_go.upcast_ref(), more_button.upcast_ref()] {
            tools.append(w);
        }

        // What the player rarely needs (they never used them): behind More.
        let types_row = frame_box();
        types_row.append(&gtk::Label::builder().label("Value types new searches look for:").xalign(0.0).hexpand(true).build());
        types_row.append(&types);

        let digits = gtk::Box::builder().spacing(2).build();
        let digits_hint = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).css_classes(["dim-label"]).build();
        let digits_row = frame_box();
        digits_row.append(&gtk::Label::new(Some("Learned digits:")));
        digits_row.append(&digits);
        digits_row.append(&digits_hint);

        let shapes_label = gtk::Label::builder().xalign(0.0).hexpand(true).wrap(true).build();
        let forget_shapes = gtk::Button::builder()
            .label("Forget Them")
            .tooltip_text("Search everywhere instead, until the next value found")
            .valign(gtk::Align::Center)
            .build();
        let shapes_row = frame_box();
        shapes_row.set_visible(false);
        shapes_row.append(&shapes_label);
        shapes_row.append(&forget_shapes);

        let more = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).build();
        more.append(&types_row);
        more.append(&digits_row);
        more.append(&shapes_row);
        let more_reveal = gtk::Revealer::builder().child(&more).build();
        more_button.bind_property("active", &more_reveal, "reveal-child").sync_create().build();

        let middle = gtk::Box::builder().spacing(12).margin_start(12).margin_end(12).vexpand(true).build();
        middle.append(&scroll);
        middle.append(&log_box);

        let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(6).margin_bottom(12).build();
        let saved_grid = gtk::Grid::builder().column_spacing(24).row_spacing(4).build();
        let replaces = gtk::Label::builder().xalign(0.0).wrap(true).css_classes(["warning"]).visible(false).build();
        let saved_box = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(6)
            .margin_start(12)
            .margin_end(12)
            .visible(false)
            .build();
        saved_box.append(&replaces);
        saved_box.append(&gtk::Label::builder().label("Already saved for this game:").xalign(0.0).css_classes(["heading"]).build());
        saved_box.append(&saved_grid);
        page.append(&saved_box);

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
        page.append(&middle);
        page.append(&tools);
        page.append(&more_reveal);
        let root = gtk::Stack::new();
        root.add_named(&intro, Some("intro"));
        root.add_named(&page, Some("pick"));

        let view = Rc::new(Self {
            root,
            picture,
            scroll: scroll.clone(),
            zoom: Cell::new(0.0),
            pointer: Cell::new((0.0, 0.0)),
            pan: Cell::new(None),
            area,
            texture: RefCell::default(),
            words: RefCell::default(),
            selection: RefCell::default(),
            pick_hint,
            seen: RefCell::default(),
            unconfirmed: RefCell::default(),
            question: RefCell::default(),
            drag: RefCell::default(),
            crop,
            unpick,
            start,
            stop,
            start_over,
            undo,
            redo,
            types: types.clone(),
            guide,
            log,
            last_log: RefCell::default(),
            result,
            name,
            saved_box,
            saved_grid: RefCell::new(saved_grid),
            saved_rows: RefCell::default(),
            replaces,
            shapes_row,
            shapes_label,
            save: save.clone(),
            matches,
            matches_list,
            listed: RefCell::default(),
            match_rows: RefCell::default(),
            last_list: RefCell::default(),
            one_by_one: RefCell::default(),
            trying: RefCell::default(),
            call_box,
            keep_going,
            show_list,
            try_box,
            try_entry,
            ask_box,
            typed,
            typed_go: typed_go.clone(),
            last_typed: RefCell::default(),
            last_count: Cell::default(),
            digits,
            digits_hint,
            worker,
            cancel,
            scan_now,
            again,
        });

        {
            let view = view.clone();
            begin.connect_clicked(move |_| view.capture());
        }
        {
            let view = view.clone();
            capture.connect_clicked(move |_| view.capture());
        }
        {
            let view_ = view.clone();
            type_instead.connect_clicked(move |_| {
                view_.root.set_visible_child_name("pick");
                match view_.selection.borrow().is_some() {
                    true => view_.guide.pick(
                        "Type the number the game shows",
                        "Below the picture. Then change it in the game and type the new one. Ferret learns the game's digits from it.",
                    ),
                    false => view_.guide.pick(
                        "Type the number the game shows",
                        "Below the picture. Then change it in the game and type the new one. Click the number in the picture first, and Ferret learns the game's digits from it.",
                    ),
                }
                view_.typed.grab_focus();
            });
            let view_ = view.clone();
            typed_go.connect_clicked(move |_| view_.type_number(&view_.typed.text()));
            let view_ = view.clone();
            view.typed.connect_activate(move |e| view_.type_number(&e.text()));
        }
        {
            let view_ = view.clone();
            zoom.connect_clicked(move |_| view_.fit());
        }
        {
            // Scroll wheel: zoom around the pointer (instead of scrolling).
            let motion = gtk::EventControllerMotion::new();
            let view_ = view.clone();
            motion.connect_motion(move |_, x, y| view_.pointer.set((x, y)));
            scroll.add_controller(motion);
            let wheel = gtk::EventControllerScroll::new(gtk::EventControllerScrollFlags::VERTICAL);
            wheel.set_propagation_phase(gtk::PropagationPhase::Capture);
            let view_ = view.clone();
            wheel.connect_scroll(move |_, _, dy| {
                view_.zoom_by(1.25f64.powf(-dy));
                glib::Propagation::Stop
            });
            scroll.add_controller(wheel);
            // Right button: drag the picture around.
            let pan = gtk::GestureDrag::builder().button(gdk::BUTTON_SECONDARY).build();
            let view_ = view.clone();
            pan.connect_drag_begin(move |_, _, _| {
                view_.pan.set(Some((view_.scroll.hadjustment().value(), view_.scroll.vadjustment().value())));
                view_.scroll.set_cursor_from_name(Some("grabbing"));
            });
            let view_ = view.clone();
            pan.connect_drag_update(move |_, dx, dy| {
                if let Some((h, v)) = view_.pan.get() {
                    view_.scroll.hadjustment().set_value(h - dx);
                    view_.scroll.vadjustment().set_value(v - dy);
                }
            });
            let view_ = view.clone();
            pan.connect_drag_end(move |_, _, _| {
                view_.pan.set(None);
                view_.scroll.set_cursor_from_name(None);
            });
            scroll.add_controller(pan);
        }
        {
            let view_ = view.clone();
            view.start.connect_clicked(move |_| view_.start());
            let worker = view.worker.clone();
            types.connect_selected_notify(move |d| {
                let kinds = TYPE_CHOICES.get(d.selected() as usize).map_or(Vec::new(), |(_, k)| k.to_vec());
                worker.run(move |core| {
                    core.scan_kinds = kinds;
                    Event::Idle
                });
            });
        }
        {
            let view_ = view.clone();
            view.stop.connect_clicked(move |_| view_.stop());
            let view_ = view.clone();
            view.start_over.connect_clicked(move |_| view_.start_over());
            let view_ = view.clone();
            view.unpick.connect_clicked(move |_| view_.unpick());
            let view_ = view.clone();
            view.undo.connect_clicked(move |_| view_.undo());
            let view_ = view.clone();
            view.redo.connect_clicked(move |_| view_.redo());
            let view_ = view.clone();
            view.again.connect_clicked(move |_| view_.scan_again());
        }
        {
            let view_ = view.clone();
            save.connect_clicked(move |_| view_.save_as(&view_.name.text()));
            let view_ = view.clone();
            view.name.connect_activate(move |e| view_.save_as(&e.text()));
            let view_ = view.clone();
            view.name.connect_changed(move |_| view_.show_replaces());
            let view_ = view.clone();
            // Save takes Start's place in the bar while a find waits for its name.
            view.result.connect_visible_notify(move |r| {
                view_.show_saved_box();
                view_.start.set_visible(!r.is_visible() && !view_.stop.is_visible());
            });
            let view_ = view.clone();
            forget_shapes.connect_clicked(move |b| view_.ask_forget_shapes(b));
        }
        {
            let view_ = view.clone();
            one_by_one.connect_clicked(move |_| view_.try_one_by_one());
            let view_ = view.clone();
            view.keep_going.connect_clicked(move |_| view_.keep_going());
            let view_ = view.clone();
            view.show_list.connect_toggled(move |b| {
                view_.matches.set_visible(b.is_active() && !view_.last_list.borrow().is_empty());
                if b.is_active() {
                    view_.worker.run(|core| Event::Matches(Ok(core.matches())));
                }
            });
            let view_ = view.clone();
            try_it.connect_clicked(move |_| view_.try_place());
            let view_ = view.clone();
            view.try_entry.connect_activate(move |_| view_.try_place());
            let view_ = view.clone();
            skip.connect_clicked(move |_| view_.next_place(""));
            let view_ = view.clone();
            stop_trying.connect_clicked(move |_| view_.stop_trying());
            let view_ = view.clone();
            yes.connect_clicked(move |_| view_.its_this_one());
            let view_ = view.clone();
            no.connect_clicked(move |_| view_.not_this_one());
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
        let s = (w / iw).min(h / ih);
        // Fitted: never above actual size (the picture scales down only).
        let s = if self.zoom.get() > 0.0 { s } else { s.min(1.0) };
        Some((s, (w - iw * s) / 2.0, (h - ih * s) / 2.0))
    }

    /// Shows a new frame of the game; a zoomed picture stays zoomed as much.
    fn set_frame(&self, texture: gdk::Texture) {
        let z = self.zoom.get();
        if z > 0.0 {
            self.picture.set_size_request((texture.width() as f64 * z).round() as i32, (texture.height() as f64 * z).round() as i32);
        }
        self.picture.set_paintable(Some(&texture));
        *self.texture.borrow_mut() = Some(texture);
    }

    /// Debug actions: shows an image file as the game's frame (no capture), and zooms around
    /// the middle of the picture (`factor`; 0 = fit).
    pub fn debug_frame(&self, path: &str) {
        if let Ok(t) = gdk::Texture::from_filename(path) {
            self.root.set_visible_child_name("pick");
            self.set_frame(t);
            self.area.queue_draw();
        }
    }

    pub fn debug_zoom(&self, factor: f64) {
        if factor <= 0.0 {
            self.fit();
            return;
        }
        self.pointer.set((self.scroll.width() as f64 / 2.0, self.scroll.height() as f64 / 2.0));
        self.zoom_by(factor);
    }

    /// Back to the picture fitted in the window.
    fn fit(&self) {
        self.zoom.set(0.0);
        self.picture.set_size_request(-1, -1);
        self.picture.set_content_fit(gtk::ContentFit::ScaleDown);
        self.area.queue_draw();
    }

    /// Zooms by `factor`, keeping the spot under the pointer where it is.
    fn zoom_by(&self, factor: f64) {
        let Some((s, ox, oy)) = self.layout() else { return };
        let Some((iw, ih)) = self.texture.borrow().as_ref().map(|t| (t.width() as f64, t.height() as f64)) else { return };
        let (hadj, vadj) = (self.scroll.hadjustment(), self.scroll.vadjustment());
        let (vw, vh) = (self.scroll.width() as f64, self.scroll.height() as f64);
        let (px, py) = self.pointer.get();
        // The frame's pixel under the pointer.
        let (fx, fy) = ((px + hadj.value() - ox) / s, (py + vadj.value() - oy) / s);
        let fitted = (vw / iw).min(vh / ih).min(1.0);
        let z = (s * factor).min(8.0);
        if z <= fitted * 1.001 {
            self.fit();
            return;
        }
        self.zoom.set(z);
        let (w, h) = (iw * z, ih * z);
        self.picture.set_content_fit(gtk::ContentFit::Contain);
        self.picture.set_size_request(w.round() as i32, h.round() as i32);
        // The scroll range grows on the next layout; set it now so the new position isn't cut to
        // the old range.
        let (nox, noy) = (((vw - w) / 2.0).max(0.0), ((vh - h) / 2.0).max(0.0));
        hadj.set_upper(w.max(vw));
        vadj.set_upper(h.max(vh));
        hadj.set_value(fx * z + nox - px);
        vadj.set_value(fy * z + noy - py);
        self.area.queue_draw();
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
        // Bolder while nothing is picked: they're what to click.
        let none = self.selection.borrow().is_none();
        cr.set_line_width(if none { 3.0 } else { 1.5 });
        cr.set_source_rgba(0.21, 0.52, 0.89, if none { 1.0 } else { 0.8 });
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
            if let Some(seen) = self.seen.borrow().as_ref() {
                self.draw_seen(cr, r, seen.as_deref());
            }
        }
        if let Some((x0, y0, x1, y1)) = *self.drag.borrow() {
            cr.rectangle(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
            cr.stroke().ok();
        }
    }

    /// "reads 50" under the box (above it near the bottom), so a misread shows while it happens.
    fn draw_seen(&self, cr: &gtk::cairo::Context, r: (f64, f64, f64, f64), seen: Option<&str>) {
        let text = seen.map_or("no number".to_owned(), |n| format!("reads {n}"));
        cr.select_font_face("Sans", gtk::cairo::FontSlant::Normal, gtk::cairo::FontWeight::Bold);
        cr.set_font_size(15.0);
        let Ok(ext) = cr.text_extents(&text) else { return };
        let (pad, h) = (6.0, 15.0 + 10.0);
        let w = ext.width() + 2.0 * pad;
        let x = r.0.min(self.area.width() as f64 - w).max(0.0);
        let below = r.1 + r.3 + 6.0;
        let y = if below + h <= self.area.height() as f64 { below } else { (r.1 - 6.0 - h).max(0.0) };
        match seen {
            Some(_) => cr.set_source_rgba(0.9, 0.38, 0.0, 0.95),
            None => cr.set_source_rgba(0.3, 0.3, 0.3, 0.9),
        }
        cr.rectangle(x, y, w, h);
        cr.fill().ok();
        cr.set_source_rgb(1.0, 1.0, 1.0);
        cr.move_to(x + pad - ext.x_bearing(), y + h / 2.0 - ext.y_bearing() - ext.height() / 2.0);
        cr.show_text(&text).ok();
    }

    /// What the last read of the box saw.
    pub fn seen(&self, n: Option<String>) {
        self.seen.replace(Some(n));
        self.area.queue_draw();
    }

    /// Makes the button to press next pulse (none: nothing to press, or the entry has the focus).
    fn nudge(&self, next: Option<&gtk::Button>) {
        for b in [&self.start, &self.typed_go, &self.save] {
            b.remove_css_class("nudge");
        }
        if let Some(b) = next {
            b.add_css_class("nudge");
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
        self.last_log.take();
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
        let line = group_counts(msg);
        // The same line again (a search waiting on the same reading): count it on that line
        // instead of filling the log with it.
        let mut last = self.last_log.borrow_mut();
        if last.0 == line {
            last.1 += 1;
            let mut start = buffer.end_iter();
            start.backward_line();
            if !start.starts_line() {
                start.set_line_offset(0);
            }
            buffer.delete(&mut start, &mut buffer.end_iter());
            buffer.insert(&mut buffer.end_iter(), &format!("{line} (×{})\n", last.1));
        } else {
            *last = (line.clone(), 1);
            buffer.insert(&mut buffer.end_iter(), &format!("{line}\n"));
        }
        let mark = buffer.create_mark(None, &buffer.end_iter(), false);
        self.log.scroll_mark_onscreen(&mark);
        buffer.delete_mark(&mark);
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
        // The intro page has no instruction bar: errors would go unseen there.
        self.root.set_visible_child_name("pick");
        self.guide.wait(guide::PICK, "Capturing the game window…", "The first time, your desktop asks which window to share.");
        self.worker.run(|core| Event::Numbers(core.numbers()));
    }

    pub fn numbers(&self, r: Result<(PathBuf, Vec<Word>), String>) {
        match r.and_then(|(frame, words)| {
            gdk::Texture::from_filename(&frame).map(|t| (t, words)).map_err(|e| e.to_string())
        }) {
            Ok((texture, words)) => {
                self.set_frame(texture);
                self.log(&format!("{} numbers found on screen", words.len()));
                *self.words.borrow_mut() = words;
                self.root.set_visible_child_name("pick");
                // Nothing is picked for the player, even with one number on screen: they may want
                // a bar there, and a pick they didn't make read like Ferret doing things on its own.
                self.guide.pick(
                    "Click the number you want to find",
                    "In the picture of your game below. Or drag a box around it, around a bar, or around a row of icons such as hearts.",
                );
                self.pick_hint.set_visible(self.selection.borrow().is_none());
                self.area.queue_draw();
            }
            Err(e) => {
                self.guide.pick("Could not capture the game window", &e);
                self.log(&e);
            }
        }
    }

    pub fn select(&self, area: Rect) {
        *self.selection.borrow_mut() = Some(area);
        self.unpick.set_visible(true);
        self.pick_hint.set_visible(false);
        self.seen.replace(None);
        self.nudge(None);
        self.area.queue_draw();
        self.guide.wait(guide::PICK, "Reading the number…", "");
        self.guide.follows.set(None);
        self.worker.run(move |core| {
            let kept = core.set_area(area);
            let read = core.read_picked();
            // No number there: maybe a bar or icons.
            let bar = matches!(read, Ok(None)).then(|| core.pick_bar());
            Event::Read(read, core.watched(), kept, bar)
        });
    }

    pub fn read(self: &Rc<Self>, r: Result<Option<(Shown, bool)>, String>, area: Option<Rect>, kept: Option<usize>, bar: Option<Result<GaugeKind, String>>) {
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
            self.set_frame(t);
            self.area.queue_draw();
        }
        self.unconfirmed.replace(None);
        let refused = match &bar {
            Some(Err(e)) if !e.contains("long and thin") => Some(e.clone()),
            _ => None,
        };
        match r {
            Ok(Some((n, true))) => {
                self.ready_to_start(&n);
            }
            // PaddleOCR's read: ask, and learn the game's digits from the answer. Not on Start: a
            // player who wanted to start confirmed a misread (YAW's heart icon learned as a 0).
            Ok(Some((n, false))) => {
                self.guide.pick(&format!("Reads {n}?"), "Answer the question first.");
                self.start.set_sensitive(false);
                self.ask_read(n);
            }
            Ok(None) if matches!(bar, Some(Ok(_))) => {
                let kind = bar.and_then(Result::ok);
                self.guide.follows.set(kind);
                match kind {
                    Some(GaugeKind::Icons) => self.guide.pick(
                        "Those are icons: click Start when you're ready",
                        "Ferret counts the full ones (halves too). Leave them alone until this turns green, then play until the count goes down or up a couple of times.",
                    ),
                    _ => self.guide.pick(
                        "That's a bar: click Start when you're ready",
                        "Ferret goes by how full it is. Leave it alone until this turns green, then play until it goes down or up a couple of times.",
                    ),
                }
                self.start.set_sensitive(true);
                self.nudge(Some(&self.start));
            }
            // Long and thin like a bar, but Ferret can't measure it: say why.
            Ok(None) if refused.is_some() => {
                self.guide.pick(
                    "Can't read a number there",
                    &format!("Can't measure it as a bar either: {}", refused.unwrap_or_default()),
                );
                self.start.set_sensitive(false);
            }
            Ok(None) => {
                self.guide.pick(
                    "Can't read a number there",
                    "Type the number the game shows below; Ferret learns the game's digits from it. Or try a tighter box.",
                );
                self.start.set_sensitive(false);
                self.typed.grab_focus();
            }
            Err(e) => self.guide.pick("Can't read it", &e),
        }
        if let Some(n) = kept {
            self.ask_keep(n);
        }
    }

    /// A number is read and confirmed: Start can go.
    fn ready_to_start(&self, n: &Shown) {
        self.guide.pick(
            &format!("Reads {n}: click Start when you're ready"),
            "Leave the number alone until this turns green. Then play until it changes a couple of times.",
        );
        self.start.set_sensitive(true);
        self.nudge(Some(&self.start));
    }

    /// Shows the box next to the number read from it and asks whether they're the same.
    fn ask_read(self: &Rc<Self>, n: Shown) {
        let dialog = adw::AlertDialog::new(
            Some("Is This the Number?"),
            Some("Compare the picture of your box with what Ferret read. Ferret learns how the game draws its digits from your answer."),
        );
        let crop = gtk::Picture::builder()
            .content_fit(gtk::ContentFit::Contain)
            .height_request(96)
            .width_request(240)
            .build();
        if let Ok(t) = gdk::Texture::from_filename(core::cache_dir().join("area.png")) {
            crop.set_paintable(Some(&t));
        }
        let read = gtk::Label::builder().label(n.to_string()).css_classes(["read-number"]).build();
        let side = |title: &str, w: &gtk::Widget| {
            let b = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).hexpand(true).build();
            b.append(&gtk::Label::builder().label(title).css_classes(["caption-heading", "dim-label"]).build());
            b.append(w);
            b
        };
        let both = gtk::Box::builder().spacing(18).build();
        both.append(&side("Your box in the game", crop.upcast_ref()));
        both.append(&side("Ferret reads", read.upcast_ref()));
        dialog.set_extra_child(Some(&both));
        dialog.add_responses(&[("no", "No, I'll Type It"), ("yes", &format!("Yes, It Shows {n}"))]);
        dialog.set_response_appearance("yes", adw::ResponseAppearance::Suggested);
        // No default: Enter mustn't answer for the player.
        dialog.set_close_response("no");
        self.unconfirmed.replace(Some(n));
        let view = self.clone();
        dialog.connect_response(None, move |_, r| {
            view.question.replace(None);
            view.answer_read(r == "yes");
        });
        self.question.replace(Some(dialog.clone()));
        dialog.present(Some(&self.root));
    }

    /// The player's answer to `ask_read` (also the D-Bus `confirm yes|no` action).
    pub fn answer_read(&self, yes: bool) {
        // Taken first: closing the dialog answers it with its close response ("no").
        let n = self.unconfirmed.take();
        if let Some(q) = self.question.take() {
            q.force_close();
        }
        let Some(n) = n else { return };
        if yes {
            self.ready_to_start(&n);
            self.worker.run(move |core| {
                core.confirm(&n);
                Event::Digits(core.digits())
            });
        } else {
            self.guide.pick(
                "Type the number the game shows",
                "Below the picture; Ferret learns the game's digits from it. Or try a tighter box.",
            );
            self.typed.grab_focus();
        }
    }

    /// A different number was picked during a search: it may show the same value (a total and
    /// a stack) or another one, which only the player knows.
    fn ask_keep(self: &Rc<Self>, n: usize) {
        let dialog = adw::AlertDialog::new(
            Some("Keep the Matches?"),
            Some(&format!(
                "You picked a different number while {} {} still {} the last one. Keep them if this \
                 number shows the same value (a total and a stack of it, say); start over if it's another value.",
                grouped(n),
                if n == 1 { "place" } else { "places" },
                if n == 1 { "matches" } else { "match" }
            )),
        );
        dialog.add_responses(&[("start-over", "Start Over"), ("keep", "Keep Them")]);
        dialog.set_response_appearance("keep", adw::ResponseAppearance::Suggested);
        dialog.set_default_response(Some("keep"));
        dialog.set_close_response("keep");
        let view = self.clone();
        dialog.connect_response(Some("start-over"), move |_, _| view.start_over());
        dialog.present(Some(&self.root));
    }

    /// Something the player has to do now (the search waits for it).
    pub fn ask(&self, msg: &str) {
        self.guide.note(msg);
        // The search waits for the player (the probe): it can be given up, typed or not.
        if !self.typed.is_editable() || self.stop.is_visible() {
            self.stop.set_visible(true);
        }
    }

    /// The places listed stay while a typed number is searched for (it updates them), but can't
    /// be tried then. While Start runs they can: trying one stops it first.
    fn searching(&self, searching: bool) {
        self.matches_list.set_sensitive(!searching);
    }

    pub fn start(&self) {
        // The D-Bus `find` action doesn't go through the button.
        if self.unconfirmed.borrow().is_some() {
            return;
        }
        self.one_by_one.replace(None);
        self.result.set_visible(false);
        self.nudge(None);
        self.start.set_visible(false);
        self.start_over.set_visible(false);
        self.undo.set_visible(false);
        self.redo.set_visible(false);
        self.stop.set_label("Stop");
        self.stop.set_visible(true);
        self.again.set_visible(true);
        // Hands off at once: the core takes a moment to read the screen and start scanning, and
        // the player changed the number meanwhile when nothing said to wait yet.
        self.guide.wait(guide::HANDS_OFF, "Reading the number on screen…", "Don't change the number in the game yet. This turns green when it's your turn.");
        self.worker.run(move |core| Event::Auto(core.auto(Duration::from_secs(600))));
    }

    pub fn auto_done(&self, r: Result<AutoResult, String>) {
        self.stop.set_visible(false);
        self.start.set_visible(true);
        self.start_over.set_visible(true);
        self.undo.set_visible(true);
        self.redo.set_visible(true);
        // A typed number stopped it: its search runs next and tells how it went.
        if !self.typed.is_editable() {
            return;
        }
        self.stop.set_label("Stop");
        self.again.set_sensitive(true);
        self.searching(false);
        match r {
            Ok(AutoResult::Found(loc)) => self.found(loc),
            Ok(AutoResult::Unsure(why)) => self.unsure(&why),
            Ok(AutoResult::Several(n)) => {
                let text = self.count_text(n);
                match n <= 20 {
                    true => {
                        self.guide.ended(
                            Look::Call,
                            guide::CHECK,
                            &format!("{text}: Ferret can't tell which one it is"),
                            "Try them one by one, or Keep Going: let the number change in the game again.",
                            "Switch to Ferret to try them, or press Start there and let the number change again.",
                        );
                        self.offer_places(true);
                    }
                    false => self.guide.ended(
                        Look::Pick,
                        guide::CHANGE,
                        &text,
                        "Click Start to go on, and let the number change a few more times.",
                        "Press Start in Ferret to go on, and let the number change a few more times.",
                    ),
                }
                self.list_matches(n);
                self.nudge(Some(&self.start));
            }
            Err(e) => self.stopped(&e),
        }
    }

    /// One place left that Ferret doubts (`why`): listed with Try and Use This One, since the
    /// player may know it's the value (the screen showed it another way).
    fn unsure(&self, why: &str) {
        self.last_count.set(Some(1));
        self.guide.ended(
            Look::Call,
            guide::CHECK,
            "One place left, but Ferret isn't sure",
            &format!("{why} Try it: Ferret writes a number there and asks whether the game shows it."),
            &format!("{why} Switch to Ferret to try it."),
        );
        self.offer_places(true);
        self.list_matches(1);
    }

    /// A search that ended without an answer: why, in the status line and on the card.
    fn stopped(&self, why: &str) {
        let mut why = why.to_owned();
        if let Some(c) = why.get(..1) {
            why.replace_range(..1, &c.to_uppercase());
        }
        self.guide.ended(Look::Pick, guide::PICK, "The search stopped", &why, &why);
        self.refresh_matches();
    }

    /// What a search is doing, from the core. Ready and Watching only while Start runs: a typed
    /// number's search says how it went when it ends.
    pub fn phase(&self, p: core::Phase) {
        let start_runs = self.typed.is_editable() && self.stop.is_visible() && !self.cancel.load(Ordering::Relaxed);
        match p {
            core::Phase::Scanning(n, done) => self.guide.scanning(&n, done),
            core::Phase::Ready(n) if start_runs => {
                self.last_count.set(Some(n));
                self.guide.ready(n);
            }
            core::Phase::Watching(n) if start_runs => {
                self.last_count.set(Some(n));
                self.guide.watching(n);
            }
            core::Phase::ScannedAgain(before, after) if start_runs => {
                self.last_count.set(Some(after));
                self.guide.scanned_again(before, after);
            }
            core::Phase::Ready(_) | core::Phase::Watching(_) | core::Phase::ScannedAgain(..) => {}
            core::Phase::Checking(n, done) => self.guide.checking(n, done),
            core::Phase::YourTurn(n, secs) => {
                // Stop gives up waiting (and lists the places to try instead).
                self.stop.set_label("Stop Waiting");
                self.guide.your_turn(n, secs);
            }
            core::Phase::SaveTurn => self.guide.save_turn(),
            core::Phase::Restoring(..) => {}
            core::Phase::Copying(done) => self.guide.copying(done),
            core::Phase::BarReady if start_runs => self.guide.bar_ready(),
            core::Phase::BarReady => {}
            core::Phase::BoxChanging => self.guide.ended(
                Look::Pick,
                guide::PICK,
                "The box keeps changing",
                "It may take in something that moves or blinks. Pick the number again with a box around its digits only.",
                "It may take in something that moves or blinks. Pick the number again with a box around its digits only.",
            ),
        }
    }

    /// A job panicked: whatever was running ends, so the page doesn't wait for it forever.
    pub fn bug(&self, msg: &str) {
        self.typed_searching(false);
        self.auto_done(Err(msg.to_owned()));
        // Not "The search stopped": it may not have been a search (a pick crashed once, and the
        // player thought Ferret had started one on its own).
        self.guide.ended(Look::Pick, guide::PICK, "Ferret hit a bug", msg, msg);
    }

    fn found(&self, loc: core::Loc) {
        self.matches.set_visible(false);
        self.again.set_visible(false);
        let at = format!("0x{:x} ({})", loc.addr, loc.kind.describe());
        self.last_count.set(None);
        self.guide.ended(
            Look::Found,
            guide::SAVE,
            "Found it!",
            &format!("It's at {at}. Give it a name and save it, so Ferret finds it again next time."),
            "Switch to Ferret to give it a name and keep it.",
        );
        self.log_found(&format!("Found it: {at}"));
        self.result.set_visible(true);
        self.name.grab_focus();
        self.nudge(Some(&self.save));
    }

    pub fn type_number(&self, text: &str) {
        if !self.typed.is_editable() {
            return;
        }
        // The D-Bus `type` action doesn't go through the entry.
        if self.typed.text() != text {
            self.typed.set_text(text);
        }
        let Some(n) = Shown::parse(text) else {
            self.guide.pick("Type the number as the game shows it", "For example 1250, 1.5 or 3:17.");
            return;
        };
        interrupt(&self.cancel, &self.stop);
        self.one_by_one.replace(None);
        self.root.set_visible_child_name("pick");
        self.last_typed.replace(text.trim().to_owned());
        self.nudge(None);
        self.result.set_visible(false);
        self.typed_searching(true);
        self.searching(true);
        self.unconfirmed.replace(None);
        self.guide.wait(guide::HANDS_OFF, &format!("Looking for {n}…"), "Don't change the number in the game yet. This turns green when it's your turn.");
        self.worker.run(move |core| Event::Typed(core.typed(n)));
    }

    pub fn typed_done(&self, r: Result<AutoResult, String>) {
        self.typed.set_text("");
        self.narrowed(r, "Change the number in the game, then type the new one.", true);
        self.typed.grab_focus();
    }

    /// Keeps only the places holding the number on screen now: at once while Start runs (it
    /// goes on), else with one read of the box.
    pub fn scan_again(&self) {
        if !self.again.is_visible() || !self.again.is_sensitive() {
            return;
        }
        if self.stop.is_visible() {
            self.scan_now.store(true, Ordering::Relaxed);
            self.guide.note("Scanning again with the number on screen…");
            return;
        }
        // Nothing to read: the number stayed the same, so search the last one typed again.
        if self.selection.borrow().is_none() {
            let last = self.last_typed.borrow().clone();
            if last.is_empty() {
                self.typed.grab_focus();
                self.nudge(Some(&self.typed_go));
                self.guide.pick("Type the number the game shows", "No number is picked in the picture, so Ferret can't read it.");
            } else {
                self.type_number(&last);
            }
            return;
        }
        self.result.set_visible(false);
        self.again.set_sensitive(false);
        self.typed_searching(true);
        self.searching(true);
        self.guide.wait(guide::HANDS_OFF, "Scanning again…", "Ferret keeps the places that hold the number on screen now. Don't change it in the game yet.");
        self.worker.run(|core| Event::ScannedAgain(core.scan_again()));
    }

    pub fn scanned_again(&self, r: Result<AutoResult, String>) {
        self.narrowed(r, "Press Scan Again whenever the number stays the same, or Start to follow its changes.", false);
    }

    /// The end of a step taken with a number (typed, or read by Scan Again).
    /// `change`: the next step is changing the number in the game (typed numbers): the card
    /// blinks and says so.
    fn narrowed(&self, r: Result<AutoResult, String>, next: &str, change: bool) {
        self.stop.set_visible(false);
        self.stop.set_label("Stop");
        self.typed_searching(false);
        self.searching(false);
        self.again.set_sensitive(true);
        match r {
            Ok(AutoResult::Found(loc)) => self.found(loc),
            Ok(AutoResult::Unsure(why)) => {
                self.unsure(&why);
                self.again.set_visible(true);
            }
            Ok(AutoResult::Several(n)) => {
                let text = self.count_text(n);
                if change {
                    self.guide.change_now(&text);
                } else {
                    self.guide.ended(Look::Pick, guide::CHANGE, &text, next, next);
                }
                // Few enough to try one by one; changing the number again stays the main way.
                if n <= 20 {
                    self.offer_places(!change);
                }
                self.list_matches(n);
                self.again.set_visible(true);
            }
            Err(e) => self.stopped(&e),
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
            10 => "None yet: the general reader reads the numbers. Confirming or typing the number the game shows teaches Ferret its digits.".to_owned(),
            0 => "All ten: Ferret reads the game's numbers with these first.".to_owned(),
            _ => format!(
                "Missing {}: the general reader fills in. Type a number that has {} once.",
                missing.join(", "),
                if missing.len() == 1 { "it" } else { "them" }
            ),
        });
    }

    /// Stops a running Start (the matches so far are kept).
    pub fn stop(&self) {
        if self.stop.is_visible() && !self.cancel.load(Ordering::Relaxed) {
            self.guide.wait(guide::PICK, "Stopping…", "Ferret puts back any test values it wrote first.");
        }
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// The search hotkey: Stop while a search runs (Stop Waiting too), else Start, as the
    /// buttons would; why not when neither can.
    pub fn hotkey_search(&self) -> Option<&'static str> {
        if self.stop.is_visible() {
            self.stop();
        } else if self.start.is_visible() && self.start.is_sensitive() && self.typed.is_editable() {
            self.start();
        } else {
            return Some("Pick the number in Ferret's Find Value tab first.");
        }
        None
    }

    /// The Scan Again hotkey; why not when the button isn't there.
    pub fn hotkey_again(&self) -> Option<&'static str> {
        if !self.again.is_visible() || !self.again.is_sensitive() || (self.selection.borrow().is_none() && self.last_typed.borrow().is_empty()) {
            return Some("Scan Again works once a search has places to check and a number is picked or typed.");
        }
        self.scan_again();
        None
    }

    /// Forgets the matches so far; the picked number and the captured frame stay.
    pub fn start_over(&self) {
        self.one_by_one.replace(None);
        self.last_count.set(None);
        self.again.set_visible(false);
        self.matches.set_visible(false);
        self.result.set_visible(false);
        self.typed.set_text("");
        match self.selection.borrow().is_some() {
            true => self.guide.pick("Starting over", "Click Start, or type the number the game shows."),
            false => self.guide.pick("Starting over", "Click a number in the picture, or type the number the game shows."),
        }
        self.worker.run(|core| {
            core.reset();
            Event::Reset
        });
    }

    /// Takes back the last step of the search (not while Start runs: the button is hidden).
    pub fn undo(&self) {
        if self.stop.is_visible() {
            return;
        }
        self.worker.run(|core| Event::Undone(core.undo(), "Undid"));
    }

    /// Takes the last step Undo took back again (not while Start runs either).
    pub fn redo(&self) {
        if self.stop.is_visible() {
            return;
        }
        self.worker.run(|core| Event::Undone(core.redo(), "Redid"));
    }

    /// After Undo or Redo (`done` says which).
    pub fn undone(&self, r: Result<(usize, String), String>, done: &str) {
        match r {
            Ok((n, what)) => {
                self.result.set_visible(false);
                self.last_count.set((n > 0).then_some(n));
                self.again.set_visible(n > 0);
                let left = match n {
                    0 => "no matches yet".to_owned(),
                    1 => "1 place matches".to_owned(),
                    n => format!("{} places match", grouped(n)),
                };
                self.guide.pick(&format!("{done} {what}"), &format!("{left}. Click Start, or type the number the game shows."));
                self.list_matches(n);
            }
            Err(e) => {
                let mut c = e.chars();
                self.guide.pick(&c.next().map_or(String::new(), |f| f.to_uppercase().chain(c).collect()), "");
            }
        }
    }

    /// Attached to another game: nothing picked or found in the last one applies.
    /// Forgets the window Ferret watches and asks the desktop again (the wrong one was picked).
    pub fn repick(&self) {
        if self.stop.is_visible() {
            self.stop();
        }
        *self.selection.borrow_mut() = None;
        self.unpick.set_visible(false);
        self.crop.set_paintable(None::<&gdk::Paintable>);
        self.start.set_sensitive(false);
        self.nudge(None);
        self.root.set_visible_child_name("pick");
        self.guide.wait(guide::PICK, "Pick the game window in your desktop's window picker…", "");
        self.worker.run(|core| Event::Numbers(core.change_window()));
    }

    /// Forgets the picked box; the matches stay. Typed numbers go on without screen reads.
    pub fn unpick(&self) {
        if self.stop.is_visible() {
            self.stop();
        }
        *self.selection.borrow_mut() = None;
        self.unpick.set_visible(false);
        self.crop.set_paintable(None::<&gdk::Paintable>);
        self.start.set_sensitive(false);
        self.nudge(None);
        self.pick_hint.set_visible(self.texture.borrow().is_some());
        self.area.queue_draw();
        self.guide.pick("No box picked", "Type the number the game holds below, or click one in the picture.");
        self.worker.run(|core| {
            core.clear_area();
            Event::Idle
        });
    }

    pub fn new_game(&self) {
        self.one_by_one.replace(None);
        self.last_count.set(None);
        self.again.set_visible(false);
        self.matches.set_visible(false);
        self.fit();
        self.picture.set_paintable(None::<&gdk::Paintable>);
        self.crop.set_paintable(None::<&gdk::Paintable>);
        *self.texture.borrow_mut() = None;
        self.words.borrow_mut().clear();
        *self.selection.borrow_mut() = None;
        self.unpick.set_visible(false);
        self.pick_hint.set_visible(false);
        self.seen.replace(None);
        self.nudge(None);
        self.unconfirmed.replace(None);
        self.result.set_visible(false);
        self.typed.set_text("");
        self.start.set_sensitive(false);
        self.guide.pick("Click the number you want to find", "In the picture of your game below, or drag a box around it.");
        self.root.set_visible_child_name("intro");
    }

    /// Lists the places still matching, each with Try (write a value) and Use This One.
    /// Whether the places still matching are listed (their values are refreshed every second).
    pub fn matches_shown(&self) -> bool {
        self.matches.is_visible()
    }

    pub fn show_matches(&self, list: Vec<core::Match>) {
        self.last_list.replace(list.clone());
        // Found: a late list (the probe re-sends it while it waits) mustn't bring it back.
        if self.result.is_visible() {
            return;
        }
        // What a place is, when Ferret can tell: an inventory stack stands out from a statistic.
        let subtitle = |m: &core::Match| {
            let at = format!("0x{:x}, {}", m.loc.addr, m.loc.kind.describe());
            m.about.as_ref().map_or(at.clone(), |about| format!("{about} · {at}"))
        };
        let locs: Vec<core::Loc> = list.iter().map(|m| m.loc).collect();
        if !list.is_empty() && *self.listed.borrow() == locs {
            for (row, m) in self.match_rows.borrow().iter().zip(&list) {
                row.set_title(&m.value);
                row.set_subtitle(&subtitle(m));
            }
            return;
        }
        while let Some(row) = self.matches_list.first_child() {
            self.matches_list.remove(&row);
        }
        self.match_rows.borrow_mut().clear();
        self.matches.set_visible(!list.is_empty() && self.show_list.is_active());
        *self.listed.borrow_mut() = locs;
        for m in list {
            let loc = m.loc;
            // One line: a huge value cut short instead of stretching the window.
            let row = adw::ActionRow::builder().title(&m.value).title_lines(1).subtitle(subtitle(&m)).build();
            self.match_rows.borrow_mut().push(row.clone());
            let entry = gtk::Entry::builder().placeholder_text("New value").width_chars(8).valign(gtk::Align::Center).build();
            let try_it = gtk::Button::builder().label("Try").valign(gtk::Align::Center).build();
            let pick = gtk::Button::builder().label("Use This One").valign(gtk::Align::Center).css_classes(["suggested-action"]).build();
            row.add_suffix(&entry);
            row.add_suffix(&try_it);
            row.add_suffix(&pick);
            let apply = {
                let (worker, entry, cancel, stop) = (self.worker.clone(), entry.clone(), self.cancel.clone(), self.stop.clone());
                move || {
                    let v = entry.text().to_string();
                    if !v.trim().is_empty() {
                        interrupt(&cancel, &stop);
                        worker.run(move |core| Event::Tried(core.try_match(loc, &v)));
                    }
                }
            };
            {
                let apply = apply.clone();
                try_it.connect_clicked(move |_| apply());
            }
            entry.connect_activate(move |_| apply());
            let drop = gtk::Button::builder()
                .icon_name("user-trash-symbolic")
                .tooltip_text("Not this one")
                .valign(gtk::Align::Center)
                .css_classes(["flat"])
                .build();
            row.add_suffix(&drop);
            let (worker, cancel, stop) = (self.worker.clone(), self.cancel.clone(), self.stop.clone());
            pick.connect_clicked(move |_| {
                interrupt(&cancel, &stop);
                worker.run(move |core| Event::Chosen(core.choose(loc)));
            });
            let (worker, root, shown, cancel, stop) =
                (self.worker.clone(), self.root.clone(), row.clone(), self.cancel.clone(), self.stop.clone());
            drop.connect_clicked(move |_| {
                interrupt(&cancel, &stop);
                ask_drop(&worker, loc, &shown.title(), &root);
            });
            self.matches_list.append(&row);
        }
    }

    /// With few places left, lists them: the player can try them out when Ferret can't tell.
    fn list_matches(&self, n: usize) {
        if n <= 20 {
            self.worker.run(|core| Event::Matches(Ok(core.matches())));
        } else {
            self.last_list.borrow_mut().clear();
            self.matches.set_visible(false);
        }
    }

    /// Try ended: the places left (one the game put back is ruled out) and what happened.
    pub fn tried(&self, list: Vec<core::Match>, msg: &str) {
        let now = self.one_by_one.borrow().as_ref().and_then(|(places, i)| places.get(*i).map(|m| m.loc));
        let kept = now.is_some_and(|loc| list.iter().any(|m| m.loc == loc));
        self.show_matches(list);
        match now {
            // The game kept the number: only the player can see whether it shows it.
            Some(_) if kept => {
                let v = self.trying.borrow().clone();
                self.guide.show(
                    Look::Call,
                    guide::CHECK,
                    &format!("Does the game show {v} now?"),
                    "Look at it in the game. Some games only redraw a number when they change it themselves: \
                     if it still shows the old number, change it in the game once and look again.",
                    None,
                );
                self.guide.offer(&self.ask_box);
            }
            // Ruled out by Ferret: the game put its own number back at once.
            Some(_) => self.next_place(&format!("{msg} ")),
            None => self.guide.note(msg),
        }
    }

    /// Ferret can't tell which of the places left is the value: the player tries them one at a
    /// time (or goes on searching).
    fn offer_places(&self, keep_going: bool) {
        self.keep_going.set_visible(keep_going);
        self.guide.offer(&self.call_box);
    }

    fn try_one_by_one(&self) {
        let mut places = self.last_list.borrow().clone();
        if places.is_empty() {
            return;
        }
        core::Core::try_order(&mut places);
        self.one_by_one.replace(Some((places, 0)));
        self.show_place("");
    }

    /// The place to try now; `before` = how the last one went.
    fn show_place(&self, before: &str) {
        let place = self.one_by_one.borrow().as_ref().map(|(places, i)| (places.get(*i).cloned(), *i, places.len()));
        let Some((place, i, n)) = place else { return };
        let Some(m) = place else {
            self.one_by_one.replace(None);
            self.guide.show(
                Look::Call,
                guide::CHECK,
                &format!("None of the {n} places showed it"),
                &format!(
                    "{before}{} still {}. Some games only redraw a number when they change it themselves: try them again, \
                     or Keep Going and let the number change in the game again.",
                    places_text(self.last_list.borrow().len()),
                    if self.last_list.borrow().len() == 1 { "matches" } else { "match" }
                ),
                None,
            );
            self.offer_places(true);
            return;
        };
        let what = m.about.clone().unwrap_or_else(|| format!("0x{:x}, {}", m.loc.addr, m.loc.kind.describe()));
        self.guide.show(
            Look::Call,
            guide::CHECK,
            &format!("Place {} of {n}: try a number there", i + 1),
            &format!(
                "{before}It holds {} now ({what}). Ferret writes the number you try there and asks whether the game shows it.",
                m.value
            ),
            None,
        );
        self.try_entry.set_text(&test_number(&m.value));
        self.guide.offer(&self.try_box);
    }

    fn next_place(&self, before: &str) {
        if let Some((_, i)) = self.one_by_one.borrow_mut().as_mut() {
            *i += 1;
        }
        self.show_place(before);
    }

    fn try_place(&self) {
        let now = self.one_by_one.borrow().as_ref().and_then(|(places, i)| places.get(*i).map(|m| (m.loc, *i)));
        let Some((loc, i)) = now else { return };
        let v = self.try_entry.text().trim().to_owned();
        if Shown::parse(&v).is_none() {
            self.guide.note("Type the number to write there, for example 500.");
            return;
        }
        self.trying.replace(v.clone());
        self.guide.wait(
            guide::CHECK,
            &format!("Trying {v} at place {}", i + 1),
            "Ferret checks that the game keeps it. If the game puts its own number back at once, that place isn't it.",
        );
        interrupt(&self.cancel, &self.stop);
        self.worker.run(move |core| Event::Tried(core.try_match(loc, &v)));
    }

    /// A Try that failed (the toast says why): the same place again.
    pub fn try_failed(&self) {
        self.show_place("");
    }

    fn its_this_one(&self) {
        let now = self.one_by_one.take().and_then(|(places, i)| places.get(i).map(|m| m.loc));
        let Some(loc) = now else { return };
        self.guide.wait(guide::SAVE, "Taking it…", "");
        self.worker.run(move |core| Event::Chosen(core.choose(loc)));
    }

    fn not_this_one(&self) {
        self.guide.wait(guide::CHECK, "Putting its old number back…", "");
        self.worker.run(|core| Event::PutBack(core.put_back_try()));
    }

    /// After "No, Next Place": the old number is back; on to the next place.
    pub fn put_back(&self, list: Vec<core::Match>) {
        self.show_matches(list);
        self.next_place("");
    }

    fn stop_trying(&self) {
        let n = self.one_by_one.take().map_or(0, |(places, _)| places.len());
        self.guide.show(
            Look::Call,
            guide::CHECK,
            &format!("{} left: Ferret can't tell which one it is", places_text(n)),
            "Try them one by one, or Keep Going: let the number change in the game again.",
            None,
        );
        self.offer_places(true);
    }

    /// Debug action: a step of trying the places one at a time, as its button would.
    pub fn place_step(&self, step: &str) {
        match step {
            "start" => self.try_one_by_one(),
            "try" => self.try_place(),
            "yes" => self.its_this_one(),
            "no" => self.not_this_one(),
            "skip" => self.next_place(""),
            "keep" => self.keep_going(),
            _ => self.stop_trying(),
        }
    }

    /// Changing the number in the game again instead of trying the places.
    fn keep_going(&self) {
        self.one_by_one.replace(None);
        if self.selection.borrow().is_some() && self.start.is_sensitive() {
            self.start();
        } else {
            self.guide.show(Look::Go, guide::CHANGE, "Now change the number in the game", "Then type the new number below.", None);
            self.typed.grab_focus();
        }
    }

    /// After a search that failed: the places listed may be gone (it started over) or not.
    fn refresh_matches(&self) {
        if self.matches.is_visible() {
            self.worker.run(|core| Event::Matches(Ok(core.matches())));
        }
    }

    /// Debug actions: Try / Use This One on the listed place at `i`.
    pub fn try_listed(&self, i: usize, value: String) {
        if let Some(&loc) = self.listed.borrow().get(i) {
            self.worker.run(move |core| Event::Tried(core.try_match(loc, &value)));
        }
    }

    pub fn drop_listed(&self, i: usize) {
        if let Some(&loc) = self.listed.borrow().get(i) {
            self.worker.run(move |core| Event::Matches(core.drop_match(loc)));
        }
    }

    /// Debug action: picks `TYPE_CHOICES[i]`.
    pub fn pick_types(&self, i: u32) {
        self.types.set_selected(i);
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
        self.set_frame(texture);
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
        self.guide.wait(guide::SAVE, &format!("Saving {name}"), "Ferret is working out how the game gets to it (about 10 seconds). Don't change it in the game yet.");
        self.worker.run(move |core| {
            let saved = core.save(&name);
            Event::Saved(saved.map(|confirmed| {
                let msg = if core.java() {
                    format!("Saved {name} for this run. Java game: Ferret only shows it, and can't find it again after a restart yet.")
                } else if confirmed {
                    format!("Saved {name}. Ferret finds it again every time you attach.")
                } else {
                    format!("Saved {name}. If it's wrong after restarting the game, find it again and save it as {name}.")
                };
                (name, msg)
            }))
        });
    }

    pub fn saved(&self, name: &str) {
        self.result.set_sensitive(true);
        self.result.set_visible(false);
        self.name.set_text("");
        self.nudge(None);
        // Back to picking: the next step (and the bar leaves the Values tab, where the value is).
        self.guide.pick(
            &format!("Saved {name}"),
            "It's in the Values tab: set it or keep it in a range there. Click another number here to find more.",
        );
    }

    pub fn show_shapes(&self, n: usize) {
        self.shapes_row.set_visible(n > 0);
        self.shapes_label.set_label(&format!(
            "New searches look first in places shaped like earlier finds ({}). If they keep landing on the wrong kind of place, forget them.",
            core::kinds_of_places(n)
        ));
    }

    fn ask_forget_shapes(&self, parent: &impl IsA<gtk::Widget>) {
        let dialog = adw::AlertDialog::new(
            Some("Forget Where Earlier Finds Were?"),
            Some("New searches then look everywhere. Ferret learns again from the next values it finds. Saved values aren't affected."),
        );
        dialog.add_responses(&[("cancel", "Cancel"), ("forget", "Forget")]);
        dialog.set_response_appearance("forget", adw::ResponseAppearance::Destructive);
        dialog.set_default_response(Some("cancel"));
        dialog.set_close_response("cancel");
        let worker = self.worker.clone();
        dialog.connect_response(Some("forget"), move |_, _| forget_shapes(&worker));
        dialog.present(Some(parent));
    }

    /// Types into the save row's name field (the D-Bus `name` action, for tests).
    pub fn type_name(&self, name: &str) {
        self.name.set_text(name);
    }

    /// The values saved for this game (from the 1 s refresh), listed under the save row.
    pub fn saved_values(&self, values: &[core::ValueRow]) {
        let rows: Vec<(String, String, String)> = values
            .iter()
            .map(|v| {
                let text = |n: f64| core::number_text(n, v.decimals);
                let value = v.value.map_or("not found now".into(), text);
                let limit = match (v.min, v.max) {
                    (Some(a), Some(b)) if a == b => format!("kept at {}", text(a)),
                    (Some(a), Some(b)) => format!("kept between {} and {}", text(a), text(b)),
                    (Some(a), None) => format!("kept at least {}", text(a)),
                    (None, Some(b)) => format!("kept at most {}", text(b)),
                    (None, None) => String::new(),
                };
                (v.name.clone(), value, limit)
            })
            .collect();
        if *self.saved_rows.borrow() == rows {
            return;
        }
        let grid = gtk::Grid::builder().column_spacing(24).row_spacing(4).build();
        for (i, (name, value, limit)) in rows.iter().enumerate() {
            for (col, text) in [name, value, limit].into_iter().enumerate() {
                let label = gtk::Label::builder()
                    .label(text.as_str())
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .max_width_chars(24)
                    .tooltip_text(text.as_str())
                    .build();
                if col == 0 {
                    label.add_css_class("heading");
                } else {
                    label.add_css_class("dim-label");
                }
                grid.attach(&label, col as i32, i as i32, 1, 1);
            }
        }
        let old = self.saved_grid.replace(grid.clone());
        self.saved_box.remove(&old);
        self.saved_box.append(&grid);
        self.saved_rows.replace(rows);
        self.show_saved_box();
        self.show_replaces();
    }

    fn show_saved_box(&self) {
        self.saved_box.set_visible(self.result.is_visible() && !self.saved_rows.borrow().is_empty());
    }

    /// Says so when the name typed is already saved: saving replaces it and keeps its limit.
    fn show_replaces(&self) {
        let name = crate::core::one_word(&self.name.text());
        let rows = self.saved_rows.borrow();
        let Some((_, value, limit)) = rows.iter().find(|(n, ..)| *n == name) else {
            self.replaces.set_visible(false);
            return;
        };
        let keeps = if limit.is_empty() { String::new() } else { format!(" Its limit stays and applies to this one ({limit}).") };
        self.replaces.set_label(&format!("“{name}” is already saved (now {value}): saving replaces it.{keeps}"));
        self.replaces.set_visible(true);
    }

    pub fn save_failed(&self, e: &str) {
        self.result.set_sensitive(true);
        self.guide.show(Look::Found, guide::SAVE, "Not saved", e, None);
    }
}

/// Stops a running Start so that what the player asked for runs next (the worker does one
/// thing at a time).
fn interrupt(cancel: &AtomicBool, stop: &gtk::Button) {
    if stop.is_visible() {
        cancel.store(true, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suggests_numbers_to_try() {
        assert_eq!(test_number("1000"), "500");
        assert_eq!(test_number("7"), "3");
        assert_eq!(test_number("1"), "11");
        assert_eq!(test_number("0"), "10");
        assert_eq!(test_number("12.5"), "6.2");
        assert_eq!(test_number("x"), "");
    }

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
