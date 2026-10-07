// The main menus: sharing saved values (`.ferret` files, core::export / core::import) from
// the game page's, to a file or the clipboard (pasted in a chat), from a file or the
// clipboard; Send Feedback on both pages (feedback.rs).

use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};

use super::{listed, Event, Ui};
use crate::core::{Export, Imported};
use crate::i18n::{ntr, tr};
use crate::library::{self, Pack};

/// Where an export goes.
pub enum To {
    File,
    Clipboard,
    /// The shared library, after the player has read it.
    Library,
}

/// Runs a call that blocks (the server) on a thread of its own, then `done` with its result
/// on the GUI's thread.
fn in_background<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static, done: impl FnOnce(T) + 'static) {
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        tx.send_blocking(f()).ok();
    });
    glib::spawn_future_local(async move {
        if let Ok(r) = rx.recv().await {
            done(r);
        }
    });
}

/// The game page's menu.
pub fn menu_button() -> gtk::MenuButton {
    let library = gio::Menu::new();
    library.append(Some(&tr!("Browse Shared Values…")), Some("app.browse"));
    library.append(Some(&tr!("Share Your Values…")), Some("app.share-values"));
    let export = gio::Menu::new();
    export.append(Some(&tr!("Export Values…")), Some("app.export"));
    export.append(Some(&tr!("Copy Values")), Some("app.copy-values"));
    let import = gio::Menu::new();
    import.append(Some(&tr!("Import Values…")), Some("app.import"));
    import.append(Some(&tr!("Paste Values")), Some("app.paste-values"));
    let menu = gio::Menu::new();
    menu.append_section(None, &library);
    menu.append_section(None, &export);
    menu.append_section(None, &import);
    menu.append_section(None, &feedback_section());
    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text(tr!("Main Menu"))
        .menu_model(&menu)
        .primary(true)
        .build()
}

fn feedback_section() -> gio::Menu {
    let menu = gio::Menu::new();
    menu.append(Some(&tr!("Send Feedback…")), Some("app.feedback"));
    menu
}

/// The games page's menu: feedback only (sharing needs a game).
pub fn games_menu_button() -> gtk::MenuButton {
    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text(tr!("Main Menu"))
        .menu_model(&feedback_section())
        .primary(true)
        .build()
}

pub fn add_actions(app: &adw::Application, ui: &Rc<Ui>) {
    let add = |name: &str, f: fn(&Rc<Ui>)| {
        let a = gio::SimpleAction::new(name, None);
        let ui = ui.clone();
        a.connect_activate(move |_, _| f(&ui));
        app.add_action(&a);
    };
    add("export", |ui| ui.worker.run(|core| Event::Exported(core.export(), To::File)));
    add("copy-values", |ui| ui.worker.run(|core| Event::Exported(core.export(), To::Clipboard)));
    add("import", import_file);
    add("browse", |ui| ui.worker.run(|core| Event::Browse(core.library_key())));
    add("share-values", |ui| ui.worker.run(|core| Event::Exported(core.export(), To::Library)));
    add("feedback", super::feedback::open);
    add("paste-values", |ui| {
        let ui = ui.clone();
        ui.window.clipboard().read_text_async(gio::Cancellable::NONE, move |text| match text {
            Ok(Some(text)) => import_text(&ui, text.to_string()),
            _ => ui.toast(&tr!("Nothing to paste: copy a game's values first")),
        });
    });
}

fn import_file(ui: &Rc<Ui>) {
    let ours = gtk::FileFilter::new();
    ours.set_name(Some(&tr!("Ferret Files")));
    ours.add_pattern("*.ferret");
    let all = gtk::FileFilter::new();
    all.set_name(Some(&tr!("All Files")));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&ours);
    filters.append(&all);
    let dialog = gtk::FileDialog::builder().title(tr!("Import Values")).filters(&filters).modal(true).build();
    let ui = ui.clone();
    dialog.open(Some(&ui.window.clone()), gio::Cancellable::NONE, move |file| {
        // Closed without picking one.
        let Ok(file) = file else { return };
        // A big file picked by mistake would hold up the window while loading.
        let size = file.query_info("standard::size", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE).map_or(0, |i| i.size());
        if size > crate::share::MAX_BYTES as i64 {
            ui.toast(&tr!("This is too big to be a Ferret file"));
            return;
        }
        match file.load_contents(gio::Cancellable::NONE) {
            Ok((bytes, _)) => import_text(&ui, String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => ui.toast(&tr!("Could not read the file: {e}", e)),
        }
    });
}

fn import_text(ui: &Rc<Ui>, text: String) {
    ui.worker.run(move |core| Event::Imported(core.import(&text, None)));
}

/// A message in a toast, or in a dialog when it is too long for one.
fn tell(ui: &Ui, heading: &str, msg: &str) {
    match msg.chars().count() > 80 {
        true => ui.explain(heading, msg),
        false => ui.toast(msg),
    }
}

pub fn exported(ui: &Rc<Ui>, export: Result<Export, String>, to: To) {
    let export = match export {
        Ok(e) => e,
        Err(e) => return tell(ui, &tr!("Nothing Exported"), &e),
    };
    let n = export.count;
    // Values that stayed out are explained once the export is done.
    let left_out = {
        let ui = ui.clone();
        let lines = export.left_out.clone();
        move || {
            if !lines.is_empty() {
                ui.explain(&tr!("Some Values Stayed Out"), &lines.join("\n"));
            }
        }
    };
    match to {
        To::Library => share_dialog(ui, export),
        // A code block: chats show it as it is (names with "_" came out in italics).
        To::Clipboard => {
            ui.window.clipboard().set_text(&format!("```\n{}```", export.text));
            ui.toast(&ntr!("Copied {n} value: paste it anywhere", "Copied {n} values: paste them anywhere", n));
            left_out();
        }
        To::File => {
            let dialog = gtk::FileDialog::builder().title(tr!("Export Values")).initial_name(&export.file_name).modal(true).build();
            let ui = ui.clone();
            dialog.save(Some(&ui.window.clone()), gio::Cancellable::NONE, move |file| {
                let Ok(file) = file else { return };
                match file.replace_contents(
                    export.text.as_bytes(),
                    None,
                    false,
                    gio::FileCreateFlags::REPLACE_DESTINATION,
                    gio::Cancellable::NONE,
                ) {
                    Ok(_) => {
                        ui.toast(&ntr!("Exported {n} value", "Exported {n} values", n));
                        left_out();
                    }
                    Err(e) => ui.toast(&tr!("Could not export: {e}", e)),
                }
            });
        }
    }
}

pub fn imported(ui: &Rc<Ui>, result: Result<Imported, String>) {
    ui.phase.hide();
    let done = match result {
        Ok(done) => done,
        Err(e) => return tell(ui, &tr!("Nothing Imported"), &e),
    };
    let n = done.added.len();
    let heading = ntr!("Imported {n} Value", "Imported {n} Values", n);
    let mut text = ntr!(
        "{names}. Check it in the Values tab: if the game shows the number Ferret reads, say so there. Ferret won't change it until you do.",
        "{names}. Check each one in the Values tab: if the game shows the number Ferret reads, say so there. Ferret won't change them until you do.",
        n,
        names = listed(&done.added)
    );
    if done.other_build {
        text += "\n";
        text += &ntr!(
            "It comes from another version of the game, so it may not be found.",
            "They come from another version of the game, so some may not be found.",
            n
        );
    }
    if !done.skipped.is_empty() {
        text += &format!("\n{}\n{}", tr!("Not imported:"), done.skipped.join("\n"));
    }
    ui.stack.set_visible_child_name("values");
    ui.worker.run(|core| Event::Values(core.values()));
    ui.explain(&heading, &text);
}

/// "Share These Values?": the exact text that goes, then the upload.
fn share_dialog(ui: &Rc<Ui>, export: Export) {
    let intro = gtk::Label::builder()
        .label(tr!("Anyone using Ferret can download these for this game. Only values you confirmed are in it, and nothing about you or your computer."))
        .wrap(true)
        .xalign(0.0)
        .build();
    let view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(10)
        .right_margin(10)
        .build();
    view.buffer().set_text(&export.text);
    let scroll = gtk::ScrolledWindow::builder().child(&view).min_content_height(220).vexpand(true).css_classes(["card"]).build();
    let content = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(12).margin_top(12).margin_bottom(18).margin_start(18).margin_end(18).build();
    content.append(&intro);
    content.append(&scroll);
    if !export.left_out.is_empty() {
        let left = gtk::Label::builder()
            .label(tr!("Left out: {values}", values = export.left_out.join("; ")))
            .wrap(true)
            .xalign(0.0)
            .css_classes(["dim-label"])
            .build();
        content.append(&left);
    }
    let error = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["error"]).visible(false).build();
    content.append(&error);
    let send = gtk::Button::builder().label(tr!("Share")).css_classes(["suggested-action"]).build();
    let cancel = gtk::Button::with_label(&tr!("Cancel"));
    let header = adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
    header.pack_start(&cancel);
    header.pack_end(&send);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    let dialog = adw::Dialog::builder().title(tr!("Share These Values?")).content_width(560).content_height(520).child(&toolbar).build();
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }
    {
        let (ui, dialog) = (ui.clone(), dialog.clone());
        send.connect_clicked(move |send| {
            send.set_sensitive(false);
            send.set_label(&tr!("Sharing…"));
            error.set_visible(false);
            let text = export.text.clone();
            let (ui, dialog, error, send) = (ui.clone(), dialog.clone(), error.clone(), send.clone());
            in_background(
                move || library::upload(&text),
                move |r| match r {
                    Ok(id) => {
                        dialog.close();
                        ui.explain(
                            &tr!("Thanks for Sharing"),
                            &format!("{}\n  {id}", tr!("Players of this game find them in Browse Shared Values now. Their id:")),
                        );
                    }
                    Err(e) => {
                        error.set_label(&e);
                        error.set_visible(true);
                        send.set_label(&tr!("Share"));
                        send.set_sensitive(true);
                    }
                },
            );
        });
    }
    dialog.present(Some(&ui.window));
}

/// "Worked for 3 players, not for 1 · Shared 2026-10-07 · Your version of the game"
fn pack_subtitle(p: &Pack, build: Option<&str>) -> String {
    let mut parts = vec![match (p.worked, p.failed) {
        (0, 0) => tr!("Nobody has said whether it works yet"),
        (w, 0) => ntr!("Worked for {n} player", "Worked for {n} players", w),
        (0, f) => ntr!("Didn't work for {n} player", "Didn't work for {n} players", f),
        (w, f) => ntr!("Worked for {n} player, not for {failed}", "Worked for {n} players, not for {failed}", w, failed = f),
    }];
    parts.push(tr!("Shared {day}", day = p.day));
    match (p.build.as_deref(), build) {
        (Some(a), Some(b)) if a == b => parts.push(tr!("Your version of the game")),
        (Some(_), Some(_)) => parts.push(tr!("Another version of the game")),
        _ => {}
    }
    if p.mine {
        parts.push(tr!("Shared by you"));
    }
    parts.join(" · ")
}

/// Browse Shared Values: other players' uploads for the open game, best first (this build's
/// first of all), each with Import, or Delete on the player's own.
pub fn browse(ui: &Rc<Ui>, key: Result<(String, Option<String>, Option<String>), String>) {
    let (game, steam, build) = match key {
        Ok(k) => k,
        Err(e) => return ui.toast(&e),
    };
    let stack = gtk::Stack::new();
    let spinner = adw::StatusPage::builder().title(tr!("Looking for Shared Values…")).child(&adw::Spinner::new()).build();
    let empty = adw::StatusPage::builder()
        .icon_name("system-search-symbolic")
        .title(tr!("Nothing Shared Yet"))
        .description(tr!("Nobody has shared values for this game yet. Once you find some, you could be the first."))
        .build();
    let failed = adw::StatusPage::builder().icon_name("network-offline-symbolic").title(tr!("Couldn't Look")).build();
    let group = adw::PreferencesGroup::new();
    let list = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&adw::Clamp::builder().child(&group).margin_top(12).margin_bottom(18).margin_start(12).margin_end(12).build())
        .build();
    stack.add_named(&spinner, Some("wait"));
    stack.add_named(&empty, Some("empty"));
    stack.add_named(&failed, Some("failed"));
    stack.add_named(&list, Some("list"));
    let share = gtk::Button::builder().label(tr!("Share Yours")).action_name("app.share-values").build();
    let header = adw::HeaderBar::new();
    header.pack_start(&share);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));
    let dialog = adw::Dialog::builder().title(tr!("Shared Values")).content_width(600).content_height(520).child(&toolbar).build();
    dialog.present(Some(&ui.window));

    // Fills the list from the server (again after a delete).
    let rows: Rc<std::cell::RefCell<Vec<adw::ActionRow>>> = Rc::default();
    let load: Rc<std::cell::RefCell<Option<Rc<dyn Fn()>>>> = Rc::default();
    let fill: Rc<dyn Fn()> = {
        let (ui, dialog, load) = (ui.clone(), dialog.clone(), load.clone());
        Rc::new(move || {
            stack.set_visible_child_name("wait");
            let (game, steam, build) = (game.clone(), steam.clone(), build.clone());
            let (ui, dialog, stack, group, rows, failed, empty, load) =
                (ui.clone(), dialog.clone(), stack.clone(), group.clone(), rows.clone(), failed.clone(), empty.clone(), load.clone());
            in_background(
                move || library::list(&game, steam.as_deref()),
                move |r| {
                    for row in rows.borrow_mut().drain(..) {
                        group.remove(&row);
                    }
                    let mut packs = match r {
                        Ok(p) => p,
                        Err(e) => {
                            failed.set_description(Some(&e));
                            return stack.set_visible_child_name("failed");
                        }
                    };
                    if packs.is_empty() {
                        empty.set_visible(true);
                        return stack.set_visible_child_name("empty");
                    }
                    // This build's first; the server's order (most worked) within each.
                    packs.sort_by_key(|p| !(p.build.is_some() && p.build == build));
                    for p in packs {
                        let row = adw::ActionRow::builder()
                            .title(&p.names)
                            .use_markup(false)
                            .subtitle(pack_subtitle(&p, build.as_deref()))
                            .subtitle_lines(2)
                            .build();
                        let button = match p.mine {
                            true => gtk::Button::builder().icon_name("user-trash-symbolic").tooltip_text(tr!("Delete")).css_classes(["flat"]).build(),
                            false => gtk::Button::with_label(&tr!("Import")),
                        };
                        button.set_valign(gtk::Align::Center);
                        // Others' uploads can be reported (rude names, junk).
                        if !p.mine {
                            let report = gtk::Button::builder().label(tr!("Report")).valign(gtk::Align::Center).css_classes(["flat"]).build();
                            row.add_suffix(&report);
                            let (ui, dialog, id) = (ui.clone(), dialog.clone(), p.id.clone());
                            report.connect_clicked(move |report| ask_report(&ui, &dialog, &id, report));
                        }
                        row.add_suffix(&button);
                        let (ui, dialog, load, id) = (ui.clone(), dialog.clone(), load.clone(), p.id.clone());
                        button.connect_clicked(move |button| match p.mine {
                            true => ask_delete(&dialog, &id, load.borrow().clone()),
                            false => {
                                button.set_sensitive(false);
                                let (ui, dialog, id, button) = (ui.clone(), dialog.clone(), id.clone(), button.clone());
                                in_background(
                                    {
                                        let id = id.clone();
                                        move || library::download(&id)
                                    },
                                    move |r| match r {
                                        Ok(text) => {
                                            dialog.close();
                                            ui.worker.run(move |core| Event::Imported(core.import(&text, Some(&id))));
                                        }
                                        Err(e) => {
                                            button.set_sensitive(true);
                                            ui.toast(&e);
                                        }
                                    },
                                );
                            }
                        });
                        group.add(&row);
                        rows.borrow_mut().push(row);
                    }
                    stack.set_visible_child_name("list");
                },
            );
        })
    };
    load.replace(Some(fill.clone()));
    fill();
}

/// Asks before reporting another player's upload, then sends it; the button says it's done.
fn ask_report(ui: &Rc<Ui>, parent: &adw::Dialog, id: &str, button: &gtk::Button) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Report This Upload?")),
        Some(&tr!(
            "For rude names, or values that are clearly junk. Reports from three players take it off the list until Ferret's developer looks at it."
        )),
    );
    dialog.add_responses(&[("cancel", &tr!("Cancel")), ("report", &tr!("Report"))]);
    dialog.set_response_appearance("report", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let (ui, id, button) = (ui.clone(), id.to_owned(), button.clone());
    dialog.connect_response(Some("report"), move |_, _| {
        button.set_sensitive(false);
        let (ui, id, button) = (ui.clone(), id.clone(), button.clone());
        in_background(
            move || library::report(&id),
            move |r| match r {
                Ok(()) => button.set_label(&tr!("Reported")),
                Err(e) => {
                    button.set_sensitive(true);
                    ui.toast(&e);
                }
            },
        );
    });
    dialog.present(Some(parent));
}

/// Asks before deleting one of the player's uploads, then lists again.
fn ask_delete(parent: &adw::Dialog, id: &str, reload: Option<Rc<dyn Fn()>>) {
    let dialog = adw::AlertDialog::new(
        Some(&tr!("Delete Your Upload?")),
        Some(&tr!("Players who imported it keep what they have; nobody else can find it anymore.")),
    );
    dialog.add_responses(&[("cancel", &tr!("Cancel")), ("delete", &tr!("Delete"))]);
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_default_response(Some("cancel"));
    dialog.set_close_response("cancel");
    let id = id.to_owned();
    let parent_ = parent.clone();
    dialog.connect_response(Some("delete"), move |_, _| {
        let id = id.clone();
        let (reload, parent) = (reload.clone(), parent_.clone());
        in_background(
            move || library::delete(&id),
            move |r| {
                if let Err(e) = r {
                    let alert = adw::AlertDialog::new(Some(&tr!("Couldn't Delete It")), Some(&e));
                    alert.add_response("ok", &tr!("OK"));
                    alert.present(Some(&parent));
                }
                if let Some(reload) = reload {
                    reload();
                }
            },
        );
    });
    dialog.present(Some(parent));
}
