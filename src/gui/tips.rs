// Tips shown once something happens (a save, ...), above the game page's content.
// Each has its own "Don't Show Again", kept in the settings file as `hide-tip <id>`.

use std::cell::Cell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use crate::i18n::tr;

#[derive(Clone, Copy, PartialEq)]
pub enum Tip {
    /// On opening a game.
    SaveFirst,
    /// After saving a value.
    Slots,
}

impl Tip {
    fn id(self) -> &'static str {
        match self {
            Tip::SaveFirst => "save-first",
            Tip::Slots => "slots",
        }
    }

    fn text(self) -> String {
        match self {
            Tip::SaveFirst => tr!(
                "Tip: Save your game before changing values. Finding a value briefly changes it, and a wrong \
                 change the game saves can't be undone by loading."
            ),
            Tip::Slots => tr!(
                "Tip: In many games, inventory values are stored per slot, not per item. If you rearrange, sort, \
                 use up, or pick up items, the address you found may now point to a different item. Re-scan if \
                 values start changing unexpectedly."
            ),
        }
    }
}

/// Ferret's settings, one per line; lines this build doesn't know are kept.
fn settings_path() -> PathBuf {
    glib::user_config_dir().join("settings")
}

/// Whether the settings file has this line.
pub fn has_setting(line: &str) -> bool {
    std::fs::read_to_string(settings_path()).unwrap_or_default().lines().any(|l| l.trim() == line)
}

pub fn add_setting(line: &str) -> std::io::Result<()> {
    let path = settings_path();
    let mut text = std::fs::read_to_string(&path).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push('\n');
    }
    text.push_str(line);
    text.push('\n');
    std::fs::create_dir_all(path.parent().unwrap_or(&path))?;
    std::fs::write(&path, text)
}

fn hidden(tip: Tip) -> bool {
    has_setting(&format!("hide-tip {}", tip.id()))
}

fn hide(tip: Tip) -> std::io::Result<()> {
    add_setting(&format!("hide-tip {}", tip.id()))
}

pub struct Tips {
    pub root: gtk::Revealer,
    label: gtk::Label,
    shown: Cell<Option<Tip>>,
}

impl Tips {
    pub fn new() -> Rc<Self> {
        let label = gtk::Label::builder().wrap(true).xalign(0.0).hexpand(true).build();
        let never = gtk::Button::builder().label(tr!("Don't Show Again")).valign(gtk::Align::Center).build();
        let close = gtk::Button::builder()
            .icon_name("window-close-symbolic")
            .tooltip_text(tr!("Close"))
            .valign(gtk::Align::Center)
            .css_classes(["flat", "circular"])
            .build();
        let bar = gtk::Box::builder().spacing(12).css_classes(["tip"]).build();
        bar.append(&label);
        bar.append(&never);
        bar.append(&close);
        let root = gtk::Revealer::builder().child(&bar).build();
        let tips = Rc::new(Tips { root, label, shown: Cell::default() });
        {
            let tips = tips.clone();
            close.connect_clicked(move |_| tips.close(false));
        }
        {
            let tips = tips.clone();
            never.connect_clicked(move |_| tips.close(true));
        }
        tips
    }

    /// `never`: Don't Show Again.
    pub fn close(&self, never: bool) {
        if let (true, Some(tip)) = (never, self.shown.get()) {
            if let Err(e) = hide(tip) {
                eprintln!("settings: {e}");
            }
        }
        self.root.set_reveal_child(false);
    }

    pub fn show(&self, tip: Tip) {
        if hidden(tip) {
            return;
        }
        self.label.set_label(&tip.text());
        self.shown.set(Some(tip));
        self.root.set_reveal_child(true);
    }
}
