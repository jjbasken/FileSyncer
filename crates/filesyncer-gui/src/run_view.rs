use crate::plan_view::human_bytes;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Eta, Plan, Progress, Reporter, RunSummary, Session, approx_duration};
use gtk::glib;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

enum Message {
    Progress(Progress),
    Done(Result<RunSummary, String>),
}

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let profile = session.profile().clone();
    // An unreadable rate only costs the estimate until this run measures one.
    let estimate = session.estimate(&plan).ok().flatten();
    let mut eta = Eta::new(&plan, estimate, Instant::now());

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
    let remaining = gtk::Label::builder().label("").build();
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
    column.append(&remaining);
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
                let forward = |p| {
                    let _ = sender.send_blocking(Message::Progress(p));
                };
                worker_session
                    .run(
                        &plan,
                        &Reporter::new(&forward, &|| cancelled.load(Ordering::Relaxed)),
                    )
                    .map_err(|e| e.to_string())
            })();
            let _ = sender.send_blocking(Message::Done(result));
        });
    }

    let aw = aw.clone();
    glib::spawn_future_local(async move {
        while let Ok(message) = receiver.recv().await {
            match message {
                Message::Progress(progress) => {
                    eta.observe(&progress);
                    if let Progress::Item { rel_path, .. } = &progress {
                        current.set_label(rel_path);
                    }
                    show_remaining(&eta, &bar, &remaining);
                }
                Message::Done(result) => {
                    show_summary(&aw, result);
                    break;
                }
            }
        }
    });
}

/// Moves the bar by bytes when there are any, since one large file can be most
/// of a sync, and by files otherwise.
fn show_remaining(eta: &Eta, bar: &gtk::ProgressBar, label: &gtk::Label) {
    let now = Instant::now();
    let left = eta.remaining(now);
    let files = eta.total_items();
    let fraction = if eta.total_bytes() > 0 {
        1.0 - left.bytes as f64 / eta.total_bytes() as f64
    } else if files > 0 {
        1.0 - left.items as f64 / files as f64
    } else {
        1.0
    };
    bar.set_fraction(fraction);
    bar.set_text(Some(&format!("{} / {files} files", files - left.items)));

    let time = match left.time {
        Some(t) => format!("{} left", approx_duration(t)),
        None => "estimating time…".to_string(),
    };
    let mut text = format!(
        "{} files · {} · {time}",
        left.items,
        human_bytes(left.bytes)
    );
    if let Some(rate) = eta.rate(now).filter(|_| left.bytes > 0) {
        text.push_str(&format!(" · {}/s", human_bytes(rate as u64)));
    }
    label.set_label(&text);
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
                    .description(format!(
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
                        .title(format!("{} file(s) left untouched", summary.conflicts))
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
