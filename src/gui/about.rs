// The About dialog (app.about, from either page's menu): the version, the choice about updates,
// who makes Ferret, what it can do on the computer and what it connects to. It has to stay true:
// a new server or a new power goes in here in the same change.

use std::cell::Cell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::Ui;
use crate::i18n::tr;
use crate::update::{self, Check};

const SOURCE: &str = "https://github.com/Petru101/ferret";
const RELEASES: &str = "https://github.com/Petru101/ferret/releases";

/// The line in the settings file (Linux has Flatpak's mask instead: update.rs).
#[cfg(windows)]
const UPDATES_OFF: &str = "updates off";

#[cfg(windows)]
pub fn automatic() -> bool {
    !super::tips::has_setting(UPDATES_OFF)
}

#[cfg(not(windows))]
pub fn automatic() -> bool {
    update::automatic()
}

#[cfg(windows)]
fn set_automatic(on: bool) -> Result<(), String> {
    match on {
        true => super::tips::remove_setting(UPDATES_OFF),
        false => super::tips::add_setting(UPDATES_OFF),
    }
    .map_err(|e| e.to_string())
}

#[cfg(not(windows))]
fn set_automatic(on: bool) -> Result<(), String> {
    update::set_automatic(on)
}

#[cfg(windows)]
fn automatic_text() -> String {
    tr!(
        "Each time it starts, Ferret looks for a new version and downloads it. It's installed when you press \
         Restart or the next time Ferret starts. Turned off, Ferret never looks by itself."
    )
}

#[cfg(not(windows))]
fn automatic_text() -> String {
    tr!(
        "New versions come when Flatpak or your software centre updates your apps. Turned off, Ferret stays on \
         this version until you install one here."
    )
}

#[cfg(windows)]
fn powers_text() -> String {
    tr!(
        "To change a game's numbers, Ferret reads and writes the memory of the game you pick, through a small \
         helper program with the same rights as you. So a bug or a bad update could reach your files, not only \
         the game. It refuses games with anti-cheat and online-only games. Ferret isn't signed with a \
         certificate, so Windows may warn about an unknown publisher, and an antivirus may flag the helper, \
         because some malware changes other programs' memory too."
    )
}

#[cfg(not(windows))]
fn powers_text() -> String {
    tr!(
        "To change a game's numbers, Ferret reads and writes the memory of the game you pick, through a small \
         helper program that runs outside Flatpak's sandbox with the same rights as you. So the sandbox doesn't \
         protect you from Ferret: a bug or a bad update could reach your files, not only the game. It refuses \
         games with anti-cheat and online-only games. On some systems it asks for your password so the helper \
         may open games, and the helper keeps only that one permission."
    )
}

/// Where Ferret's connections go, each with what goes there and when.
fn connections() -> Vec<(String, String)> {
    let mut rows = Vec::new();
    #[cfg(windows)]
    rows.push((
        tr!("Ferret's Update Site (petru101.github.io, on GitHub)"),
        tr!("Ferret asks it for new versions each time it starts, unless updates are off, and when you press Check Now. New versions download from there."),
    ));
    #[cfg(not(windows))]
    {
        rows.push((
            tr!("Ferret's Update Site (petru101.github.io, on GitHub)"),
            tr!("Flatpak or your software centre asks it for new versions when they look for updates, and Ferret does when you press Check Now. New versions download from there."),
        ));
        rows.push((tr!("Flathub"), tr!("Flatpak downloads the GNOME libraries Ferret runs on from there, and their updates.")));
    }
    rows.push((
        tr!("Send Feedback (ferret.petru101.workers.dev, on Cloudflare)"),
        tr!("Only when you press Send: your message, and the log and saved values if you leave them in. You can read all of it first."),
    ));
    rows.push((
        tr!("The Shared Library (the same server)"),
        tr!("When you browse it: the game's name and Steam id. When you share: the values you saved. When you keep or remove a value you took from it: whether it worked. Each request carries a random id made for this install."),
    ));
    rows
}

/// Automatic Updates, shown once Flatpak has said; a change that fails goes back and says why.
fn switch_row() -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().title(tr!("Automatic Updates")).subtitle(automatic_text()).use_markup(false).sensitive(false).build();
    let shown = row.clone();
    glib::spawn_future_local(async move {
        let on = gio::spawn_blocking(automatic).await.unwrap_or(true);
        shown.set_active(on);
        shown.set_sensitive(true);
        let undoing = Rc::new(Cell::new(false));
        shown.connect_active_notify(move |row| {
            if undoing.replace(false) {
                return;
            }
            let on = row.is_active();
            let (row, undoing) = (row.clone(), undoing.clone());
            glib::spawn_future_local(async move {
                let done = gio::spawn_blocking(move || set_automatic(on)).await.unwrap_or_else(|_| Err("it stopped".to_owned()));
                match done {
                    Ok(()) => row.set_subtitle(&automatic_text()),
                    Err(e) => {
                        undoing.set(true);
                        row.set_active(!on);
                        row.set_subtitle(&tr!("Ferret couldn't change this: {e}", e));
                    }
                }
            });
        });
    });
    row
}

/// What the Look for a New Version row's button does next.
#[derive(Clone, Copy)]
enum Next {
    Check,
    #[cfg(not(windows))]
    Install,
    Restart,
    /// Waiting for an answer: clicks do nothing.
    Busy,
}

/// The action that restarts into a new build (Windows installs it after the window closes).
#[cfg(windows)]
const RESTART: &str = "app.update";
#[cfg(not(windows))]
const RESTART: &str = "app.restart";

/// Look for a New Version: one button, Check Now, then Install (Linux) or Restart when there's
/// something. Never greyed out or swapped for another: a button under a pointer that hasn't moved
/// since then ignores the next click, so Install followed by Restart did nothing.
fn check_row() -> adw::ActionRow {
    let version = env!("CARGO_PKG_VERSION");
    let row = adw::ActionRow::builder().title(tr!("Look for a New Version")).subtitle(tr!("You have {version}.", version)).use_markup(false).build();
    let button = gtk::Button::builder().label(tr!("Check Now")).valign(gtk::Align::Center).build();
    row.add_suffix(&button);
    let next = Rc::new(Cell::new(Next::Check));
    let offer = |button: &gtk::Button, next: &Cell<Next>, step: Next, label: String| {
        next.set(step);
        button.set_label(&label);
        button.add_css_class("suggested-action");
    };
    let (shown, clicked) = (row.clone(), next);
    button.connect_clicked(move |button| {
        let (row, next, button) = (shown.clone(), clicked.clone(), button.clone());
        match next.replace(Next::Busy) {
            Next::Busy => {}
            Next::Restart => {
                next.set(Next::Restart);
                button.activate_action(RESTART, None).ok();
            }
            Next::Check => {
                row.set_subtitle(&tr!("Asking…"));
                glib::spawn_future_local(async move {
                    let found = gio::spawn_blocking(update::check).await.unwrap_or_else(|_| Err("it stopped".to_owned()));
                    next.set(Next::Check);
                    match found {
                        Ok(Check::Newest) | Ok(Check::NotInstalled) => row.set_subtitle(&tr!("You have the newest version, {version}.", version)),
                        #[cfg(not(windows))]
                        Ok(Check::Available(new)) => {
                            offer(&button, &next, Next::Install, tr!("Install"));
                            row.set_subtitle(&match new {
                                Some(new) => tr!("Ferret {new} is out.", new),
                                None => tr!("A new version of Ferret is out."),
                            });
                        }
                        #[cfg(not(windows))]
                        Ok(Check::Restart) => {
                            offer(&button, &next, Next::Restart, tr!("Restart"));
                            row.set_subtitle(&tr!("A newer version of Ferret is installed"));
                        }
                        #[cfg(windows)]
                        Ok(Check::Ready(new)) => {
                            offer(&button, &next, Next::Restart, tr!("Restart"));
                            row.set_subtitle(&tr!("Ferret {version} is ready to install", version = new));
                        }
                        Err(e) => row.set_subtitle(&tr!("Ferret couldn't ask for new versions: {e}", e)),
                    }
                });
            }
            #[cfg(not(windows))]
            Next::Install => {
                row.set_subtitle(&tr!("Installing…"));
                glib::spawn_future_local(async move {
                    match gio::spawn_blocking(update::install).await.unwrap_or_else(|_| Err("it stopped".to_owned())) {
                        Ok(()) => {
                            offer(&button, &next, Next::Restart, tr!("Restart"));
                            row.set_subtitle(&tr!("Installed. Restart Ferret to use it."));
                        }
                        Err(e) => {
                            next.set(Next::Install);
                            row.set_subtitle(&tr!("Ferret couldn't install it: {e}", e));
                        }
                    }
                });
            }
        }
    });
    row
}

fn link_row(title: String, subtitle: String, label: String, uri: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().title(title).subtitle(subtitle).use_markup(false).build();
    row.add_suffix(&gtk::LinkButton::builder().label(label).uri(uri).valign(gtk::Align::Center).build());
    row
}

pub fn open(ui: &Rc<Ui>) {
    let page = adw::PreferencesPage::new();

    let top = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).halign(gtk::Align::Center).build();
    top.append(&gtk::Image::builder().icon_name("io.github.Petru101.Ferret").pixel_size(96).build());
    top.append(&gtk::Label::builder().label("Ferret").css_classes(["title-1"]).build());
    top.append(&gtk::Label::builder().label(tr!("Find and change values in your games")).wrap(true).justify(gtk::Justification::Center).build());
    let version = env!("CARGO_PKG_VERSION");
    top.append(&gtk::Label::builder().label(tr!("Version {version}", version)).css_classes(["dim-label"]).build());
    let heading = adw::PreferencesGroup::new();
    heading.add(&top);
    page.add(&heading);

    let updates = adw::PreferencesGroup::builder().title(tr!("Updates")).build();
    if update::installed() {
        updates.add(&switch_row());
        updates.add(&check_row());
    } else {
        updates.set_description(Some(&tr!("This copy wasn't installed the usual way, so it doesn't update: it stays on this version.")));
    }
    page.add(&updates);

    let who = adw::PreferencesGroup::builder()
        .title(tr!("Who Makes It"))
        .description(tr!(
            "Ferret is made by one person (Petru101 on GitHub), not a company. Most of its code was written by Claude \
             (Anthropic's AI), directed and tested on real games by that person. Nobody independent has checked it. \
             Its code is public, so anyone can read it or build it themselves. New versions are built on GitHub's \
             servers from that code, then signed with its maker's key, and updates without that signature are \
             refused. On Windows, you can check where a download came from: gh attestation verify FILE -R \
             Petru101/ferret (GitHub's gh tool, signed in). That shows where the file came from, not that the code \
             is safe."
        ))
        .build();
    who.add(&link_row(tr!("The Code"), tr!("Open source, under the GPL (version 3 or later)."), tr!("On GitHub"), SOURCE));
    who.add(&link_row(tr!("What Changed"), tr!("Every version, with what's new in it."), tr!("Releases"), RELEASES));
    page.add(&who);

    page.add(&adw::PreferencesGroup::builder().title(tr!("What It Can Do on Your Computer")).description(powers_text()).build());

    let connects = adw::PreferencesGroup::builder()
        .title(tr!("What It Connects To"))
        .description(tr!(
            "Nothing about you or what you play goes to Ferret's maker unless you send it or use the shared library. \
             Ferret has no tracking or usage statistics, and the game pictures it reads numbers from stay on this \
             computer. Each site below sees your internet address."
        ))
        .build();
    for (what, why) in connections() {
        connects.add(&adw::ActionRow::builder().title(what).subtitle(why).use_markup(false).build());
    }
    page.add(&connects);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&page));
    let dialog = adw::Dialog::builder().title(tr!("About Ferret")).content_width(600).content_height(720).child(&toolbar).build();
    dialog.present(Some(&ui.window));
}
