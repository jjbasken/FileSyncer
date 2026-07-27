use crate::plan_view;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Plan, Profile, Session};
use gtk::glib;
use std::rc::Rc;

pub fn show(aw: &Rc<AppWindow>, profile: &Profile) {
    let group = adw::PreferencesGroup::builder()
        .title(&profile.name)
        .build();
    group.add(&detail_row("Local", &profile.local.display().to_string()));
    group.add(&detail_row("Remote", &profile.remote.display().to_string()));

    let status_row = adw::ActionRow::builder().title("Last synced").build();
    group.add(&status_row);

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
            let profile = profile.clone();
            let aw2 = aw.clone();
            scan_button.connect_clicked(move |button| {
                button.set_sensitive(false);
                button.set_label("Scanning…");

                // Scanning walks both trees, so run it off the main loop to keep
                // the window responsive. The worker opens its own Session because
                // rusqlite connections are Send but not Sync (see run_view).
                let (sender, receiver) = async_channel::unbounded::<Result<Plan, String>>();
                {
                    let profile = profile.clone();
                    std::thread::spawn(move || {
                        let result = (|| -> Result<Plan, String> {
                            let worker = Session::open(&profile).map_err(|e| e.to_string())?;
                            worker.prepare(false).map_err(|e| e.to_string())
                        })();
                        let _ = sender.send_blocking(result);
                    });
                }

                let aw2 = aw2.clone();
                let session = session.clone();
                let button = button.clone();
                glib::spawn_future_local(async move {
                    match receiver.recv().await {
                        Ok(Ok(plan)) => plan_view::show(&aw2, session, plan),
                        Ok(Err(e)) => show_error(&aw2, &e),
                        Err(_) => show_error(&aw2, "Scan worker stopped unexpectedly."),
                    }
                    button.set_label("Scan");
                    button.set_sensitive(true);
                });
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
