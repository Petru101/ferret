// The Send Feedback dialog (app.feedback, from either page's menu): a message, an optional
// contact, Ferret's log and the game's saved values when left in, what is sent shown on
// request; sent on a thread of its own (the worker may be busy searching).

use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;

use super::Ui;
use crate::feedback::{self, Report};

pub fn open(ui: &Rc<Ui>) {
    // The attached game, if any: its saved values can go with the report.
    let game = ui.attached.get().map(|_| ui.game_page.title().to_string()).filter(|g| !g.is_empty());

    let message = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .accepts_tab(false)
        .top_margin(10)
        .bottom_margin(10)
        .left_margin(10)
        .right_margin(10)
        .build();
    let message_box = gtk::ScrolledWindow::builder()
        .child(&message)
        .min_content_height(140)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .css_classes(["card"])
        .build();
    let contact = adw::EntryRow::builder().title("How to Reach You (Optional)").build();
    let log = adw::SwitchRow::builder()
        .title("Include Ferret's Log")
        .subtitle("What Ferret did lately, with your home folder's path taken out. Helps most with bugs.")
        .active(true)
        .build();
    let saved = adw::SwitchRow::builder()
        .title("Include This Game's Saved Values")
        .subtitle(game.as_deref().unwrap_or_default())
        .visible(game.is_some())
        .build();
    let group = adw::PreferencesGroup::new();
    group.add(&contact);
    group.add(&log);
    group.add(&saved);
    let show = gtk::Button::builder().label("Show What's Sent").halign(gtk::Align::Start).css_classes(["flat"]).build();
    let error = gtk::Label::builder().wrap(true).xalign(0.0).css_classes(["error"]).visible(false).build();
    let intro = gtk::Label::builder()
        .label("What happened, or what would you like Ferret to do? It goes to Ferret's developer, who reads every one.")
        .wrap(true)
        .xalign(0.0)
        .build();
    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(18)
        .margin_start(18)
        .margin_end(18)
        .build();
    for w in [intro.upcast_ref::<gtk::Widget>(), message_box.upcast_ref(), group.upcast_ref(), show.upcast_ref(), error.upcast_ref()] {
        content.append(w);
    }
    let send = gtk::Button::builder().label("Send").css_classes(["suggested-action"]).sensitive(false).build();
    let header = adw::HeaderBar::builder().show_end_title_buttons(false).show_start_title_buttons(false).build();
    let cancel = gtk::Button::with_label("Cancel");
    header.pack_start(&cancel);
    header.pack_end(&send);
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    let dialog = adw::Dialog::builder().title("Send Feedback").content_width(520).child(&toolbar).build();

    message.buffer().connect_changed({
        let send = send.clone();
        move |b| send.set_sensitive(!b.text(&b.start_iter(), &b.end_iter(), false).trim().is_empty())
    });
    {
        let dialog = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
    }
    // The report as the fields make it now.
    let report = {
        let (message, contact, log, saved, game) = (message.clone(), contact.clone(), log.clone(), saved.clone(), game.clone());
        move || {
            let b = message.buffer();
            Report {
                message: b.text(&b.start_iter(), &b.end_iter(), false).to_string(),
                contact: contact.text().to_string(),
                profile: game.as_deref().filter(|_| saved.is_active()).and_then(feedback::profile_text),
                game: game.clone(),
                log: log.is_active().then(feedback::log_text).flatten(),
            }
        }
    };
    {
        let (report, dialog) = (report.clone(), dialog.clone());
        show.connect_clicked(move |_| show_sent(&dialog, &report().preview()));
    }
    {
        let (ui, dialog, error) = (ui.clone(), dialog.clone(), error.clone());
        send.connect_clicked(move |send| {
            let report = report();
            send.set_sensitive(false);
            send.set_label("Sending…");
            error.set_visible(false);
            let (tx, rx) = async_channel::bounded(1);
            std::thread::spawn(move || {
                tx.send_blocking(report.send()).ok();
            });
            let (ui, dialog, error, send) = (ui.clone(), dialog.clone(), error.clone(), send.clone());
            glib::spawn_future_local(async move {
                match rx.recv().await {
                    Ok(Ok(())) => {
                        dialog.close();
                        ui.toast("Thanks! Your feedback was sent");
                    }
                    Ok(Err(e)) => {
                        error.set_label(&e);
                        error.set_visible(true);
                        send.set_label("Send");
                        send.set_sensitive(true);
                    }
                    Err(_) => {}
                }
            });
        });
    }
    dialog.present(Some(&ui.window));
    message.grab_focus();
}

/// Everything the report would send, to read before sending.
fn show_sent(parent: &adw::Dialog, text: &str) {
    let view = gtk::TextView::builder()
        .editable(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    view.buffer().set_text(text);
    let scroll = gtk::ScrolledWindow::builder().child(&view).vexpand(true).build();
    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scroll));
    let dialog = adw::Dialog::builder().title("What's Sent").content_width(640).content_height(560).child(&toolbar).build();
    dialog.present(Some(parent));
}
