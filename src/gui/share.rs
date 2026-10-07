// Sharing saved values (`.ferret` files, core::export / core::import) from the game page's
// menu: to a file or the clipboard (pasted in a chat), from a file or the clipboard.

use std::rc::Rc;

use adw::prelude::*;
use gtk::gio;

use super::{listed, Event, Ui};
use crate::core::{Export, Imported};

/// Where an export goes.
pub enum To {
    File,
    Clipboard,
}

/// The game page's menu.
pub fn menu_button() -> gtk::MenuButton {
    let export = gio::Menu::new();
    export.append(Some("Export Values…"), Some("app.export"));
    export.append(Some("Copy Values"), Some("app.copy-values"));
    let import = gio::Menu::new();
    import.append(Some("Import Values…"), Some("app.import"));
    import.append(Some("Paste Values"), Some("app.paste-values"));
    let menu = gio::Menu::new();
    menu.append_section(None, &export);
    menu.append_section(None, &import);
    gtk::MenuButton::builder()
        .icon_name("open-menu-symbolic")
        .tooltip_text("Share Values")
        .menu_model(&menu)
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
    add("paste-values", |ui| {
        let ui = ui.clone();
        ui.window.clipboard().read_text_async(gio::Cancellable::NONE, move |text| match text {
            Ok(Some(text)) => import_text(&ui, text.to_string()),
            _ => ui.toast("Nothing to paste: copy a game's values first"),
        });
    });
}

fn import_file(ui: &Rc<Ui>) {
    let ours = gtk::FileFilter::new();
    ours.set_name(Some("Ferret Files"));
    ours.add_pattern("*.ferret");
    let all = gtk::FileFilter::new();
    all.set_name(Some("All Files"));
    all.add_pattern("*");
    let filters = gio::ListStore::new::<gtk::FileFilter>();
    filters.append(&ours);
    filters.append(&all);
    let dialog = gtk::FileDialog::builder().title("Import Values").filters(&filters).modal(true).build();
    let ui = ui.clone();
    dialog.open(Some(&ui.window.clone()), gio::Cancellable::NONE, move |file| {
        // Closed without picking one.
        let Ok(file) = file else { return };
        // A big file picked by mistake would hold up the window while loading.
        let size = file.query_info("standard::size", gio::FileQueryInfoFlags::NONE, gio::Cancellable::NONE).map_or(0, |i| i.size());
        if size > crate::share::MAX_BYTES as i64 {
            ui.toast("This is too big to be a Ferret file");
            return;
        }
        match file.load_contents(gio::Cancellable::NONE) {
            Ok((bytes, _)) => import_text(&ui, String::from_utf8_lossy(&bytes).into_owned()),
            Err(e) => ui.toast(&format!("Could not read the file: {e}")),
        }
    });
}

fn import_text(ui: &Rc<Ui>, text: String) {
    ui.worker.run(move |core| Event::Imported(core.import(&text)));
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
        Err(e) => return tell(ui, "Nothing Exported", &e),
    };
    let what = match export.count {
        1 => "1 value".to_owned(),
        n => format!("{n} values"),
    };
    // Values that stayed out are explained once the export is done.
    let left_out = {
        let ui = ui.clone();
        let lines = export.left_out.clone();
        move || {
            if !lines.is_empty() {
                ui.explain("Some Values Stayed Out", &lines.join("\n"));
            }
        }
    };
    match to {
        // A code block: chats show it as it is (names with "_" came out in italics).
        To::Clipboard => {
            ui.window.clipboard().set_text(&format!("```\n{}```", export.text));
            ui.toast(&format!("Copied {what}: paste them anywhere"));
            left_out();
        }
        To::File => {
            let dialog = gtk::FileDialog::builder().title("Export Values").initial_name(&export.file_name).modal(true).build();
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
                        ui.toast(&format!("Exported {what}"));
                        left_out();
                    }
                    Err(e) => ui.toast(&format!("Could not export: {e}")),
                }
            });
        }
    }
}

pub fn imported(ui: &Rc<Ui>, result: Result<Imported, String>) {
    ui.phase.hide();
    let done = match result {
        Ok(done) => done,
        Err(e) => return tell(ui, "Nothing Imported", &e),
    };
    let heading = match done.added.len() {
        1 => "Imported 1 Value".to_owned(),
        n => format!("Imported {n} Values"),
    };
    let mut text = format!(
        "{}. Check each one in the Values tab: if the game shows the number Ferret reads, say so there. Ferret won't change them until you do.",
        listed(&done.added)
    );
    if done.other_build {
        text += "\nThey come from another version of the game, so some may not be found.";
    }
    if !done.skipped.is_empty() {
        text += &format!("\nNot imported:\n{}", done.skipped.join("\n"));
    }
    ui.stack.set_visible_child_name("values");
    ui.worker.run(|core| Event::Values(core.values()));
    ui.explain(&heading, &text);
}
