// A big card in the middle of the window while a game opens: Ferret finds its saved values
// again (seconds; the games list stays behind it). Searches say what they do in the instruction
// bar instead (`guide.rs`).

use adw::prelude::*;

pub struct PhaseCard {
    pub root: gtk::Box,
    title: gtk::Label,
    bar: gtk::ProgressBar,
    hint: gtk::Label,
}

impl PhaseCard {
    pub fn new() -> Self {
        let title = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["title-1"]).build();
        let bar = gtk::ProgressBar::builder().show_text(true).width_request(360).build();
        let hint = gtk::Label::builder().wrap(true).justify(gtk::Justification::Center).css_classes(["title-3"]).build();
        let root = gtk::Box::builder()
            .orientation(gtk::Orientation::Vertical)
            .spacing(14)
            .halign(gtk::Align::Center)
            .valign(gtk::Align::Center)
            .margin_start(24)
            .margin_end(24)
            .margin_bottom(24)
            .css_classes(["phase-card"])
            .can_target(false)
            .visible(false)
            .build();
        root.append(&title);
        root.append(&bar);
        root.append(&hint);
        Self { root, title, bar, hint }
    }

    pub fn hide(&self) {
        self.root.set_visible(false);
    }

    pub fn restoring(&self, game: &str, name: &str, i: usize, n: usize) {
        self.title.set_label(&format!("Opening {game}…"));
        self.bar.set_fraction(i as f64 / n as f64);
        self.hint.set_label(&format!("Finding your saved values: {name} ({} of {n})", i + 1));
        self.root.set_visible(true);
    }
}
