use crate::scan_view;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Compare, Profile, Session};
use std::rc::Rc;

pub fn show(aw: &Rc<AppWindow>, profile: &Profile) {
    let group = adw::PreferencesGroup::builder()
        .title(&profile.name)
        .build();
    group.add(&detail_row("Local", &profile.local.display().to_string()));
    group.add(&detail_row("Remote", &profile.remote.display().to_string()));

    let status_row = adw::ActionRow::builder().title("Last synced").build();
    group.add(&status_row);

    let verify_row = adw::SwitchRow::builder()
        .title("Verify contents")
        .subtitle("Read paired and tracked files in full, even when size and date are unchanged")
        .build();
    group.add(&verify_row);

    let scan_button = gtk::Button::builder()
        .label("Scan")
        .css_classes(vec!["suggested-action".to_string(), "pill".to_string()])
        .halign(gtk::Align::Center)
        .build();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&group);
    column.append(&scan_button);

    let clamp = adw::Clamp::builder().child(&column).build();

    // Opening the session verifies both root markers, so a dropped mount
    // surfaces here rather than after the user presses Scan.
    match Session::open(profile) {
        Ok(session) => {
            let subtitle = match session.last_sync() {
                Ok(Some(ts)) => format!("unix {ts}"),
                Ok(None) => "never".to_string(),
                Err(e) => e.to_string(),
            };
            status_row.set_subtitle(&subtitle);

            let session = Rc::new(session);
            let aw2 = aw.clone();
            // The scan page is the feedback, so the button needs no busy state
            // of its own: it is gone from the screen the moment it is pressed.
            scan_button.connect_clicked(move |_| {
                let compare = if verify_row.is_active() {
                    Compare::Verify
                } else {
                    Compare::Quick
                };
                scan_view::show(&aw2, session.clone(), compare)
            });
        }
        Err(e) => {
            scan_button.set_sensitive(false);
            status_row.set_subtitle(&e.to_string());
            status_row.add_css_class("error");
        }
    }

    aw.show_content(&clamp, &profile.name);
}

fn detail_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .css_classes(vec!["property".to_string()])
        .build()
}

pub fn show_error(aw: &Rc<AppWindow>, message: &str) {
    let dialog = adw::AlertDialog::new(Some("Sync cannot continue"), Some(message));
    dialog.add_response("ok", "OK");
    dialog.present(Some(&aw.window));
}
