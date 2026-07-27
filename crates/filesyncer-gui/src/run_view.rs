use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Plan, Progress, RunSummary, Session};
use gtk::glib;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

enum Message {
    Progress(Progress),
    Done(Result<RunSummary, String>),
}

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let profile = session.profile().clone();

    let heading = gtk::Label::builder()
        .label("Syncing…")
        .css_classes(vec!["title-2".to_string()])
        .build();
    let current = gtk::Label::builder()
        .label("")
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(vec!["dim-label".to_string()])
        .build();
    let bar = gtk::ProgressBar::builder().show_text(true).build();
    let cancel = gtk::Button::builder()
        .label("Cancel")
        .halign(gtk::Align::Center)
        .build();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .valign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&heading);
    column.append(&bar);
    column.append(&current);
    column.append(&cancel);

    aw.show_content(&adw::Clamp::builder().child(&column).build(), "Syncing");

    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let cancelled = cancelled.clone();
        cancel.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Cancelling…");
            cancelled.store(true, Ordering::Relaxed);
        });
    }

    let (sender, receiver) = async_channel::unbounded::<Message>();

    // The worker opens its own Session: rusqlite connections are Send but not
    // Sync, so the main thread's Session cannot cross the boundary.
    {
        let sender = sender.clone();
        let cancelled = cancelled.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<RunSummary, String> {
                let worker_session = Session::open(&profile).map_err(|e| e.to_string())?;
                worker_session
                    .run(
                        &plan,
                        &|p| {
                            let _ = sender.send_blocking(Message::Progress(p));
                        },
                        &|| cancelled.load(Ordering::Relaxed),
                    )
                    .map_err(|e| e.to_string())
            })();
            let _ = sender.send_blocking(Message::Done(result));
        });
    }

    let aw = aw.clone();
    glib::spawn_future_local(async move {
        let mut total = 0usize;
        while let Ok(message) = receiver.recv().await {
            match message {
                Message::Progress(Progress::Started { total: n }) => {
                    total = n;
                    bar.set_fraction(0.0);
                    bar.set_text(Some(&format!("0 / {n}")));
                }
                Message::Progress(Progress::Item {
                    index, rel_path, ..
                }) => {
                    let done = index + 1;
                    if total > 0 {
                        bar.set_fraction(done as f64 / total as f64);
                    }
                    bar.set_text(Some(&format!("{done} / {total}")));
                    current.set_label(&rel_path);
                }
                Message::Progress(Progress::Finished) => {
                    bar.set_fraction(1.0);
                }
                Message::Done(result) => {
                    show_summary(&aw, result);
                    break;
                }
            }
        }
    });
}

fn show_summary(aw: &Rc<AppWindow>, result: Result<RunSummary, String>) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    match result {
        Err(message) => {
            column.append(
                &adw::StatusPage::builder()
                    .icon_name("dialog-error-symbolic")
                    .title("Sync failed")
                    .description(&message)
                    .build(),
            );
        }
        Ok(summary) => {
            let title = if summary.cancelled {
                "Sync cancelled"
            } else if summary.errors.is_empty() {
                "Sync complete"
            } else {
                "Sync finished with errors"
            };
            column.append(
                &adw::StatusPage::builder()
                    .icon_name(if summary.errors.is_empty() {
                        "object-select-symbolic"
                    } else {
                        "dialog-warning-symbolic"
                    })
                    .title(title)
                    .description(&format!(
                        "{} copied, {} moved to trash.",
                        summary.copied, summary.trashed
                    ))
                    .build(),
            );

            if summary.conflicts > 0 {
                let group = adw::PreferencesGroup::builder()
                    .title("Unresolved conflicts")
                    .description("These will be reported again on the next scan.")
                    .build();
                group.add(
                    &adw::ActionRow::builder()
                        .title(&format!("{} file(s) left untouched", summary.conflicts))
                        .build(),
                );
                column.append(&group);
            }

            // Errors are listed individually rather than collapsed into a count.
            if !summary.errors.is_empty() {
                let group = adw::PreferencesGroup::builder().title("Errors").build();
                for (path, message) in &summary.errors {
                    group.add(
                        &adw::ActionRow::builder()
                            .title(path)
                            .subtitle(message)
                            .build(),
                    );
                }
                column.append(&group);
            }
        }
    }

    let done = gtk::Button::builder()
        .label("Done")
        .halign(gtk::Align::Center)
        .css_classes(vec!["pill".to_string()])
        .build();
    {
        let aw = aw.clone();
        done.connect_clicked(move |_| aw.show_placeholder());
    }
    column.append(&done);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&adw::Clamp::builder().child(&column).build())
        .vexpand(true)
        .build();
    aw.show_content(&scrolled, "Result");
}
