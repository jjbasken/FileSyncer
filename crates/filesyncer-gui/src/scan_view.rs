use crate::plan_view;
use crate::profile_page;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Compare, Phase, Plan, Progress, Reporter, Session};
use gtk::glib;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// How often the worker may forward an `Item`. A local walk can visit thousands
/// of files a second, which would flood the channel without changing what the
/// eye can read.
const ITEM_INTERVAL: Duration = Duration::from_millis(100);

enum Message {
    Progress(Progress),
    /// `Ok(None)` means the scan was cancelled.
    Done(Result<Option<Plan>, String>),
}

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, compare: Compare) {
    let profile = session.profile().clone();

    let heading = gtk::Label::builder()
        .label(scanning_label(Phase::ScanLocal))
        .css_classes(vec!["title-2".to_string()])
        .build();
    let count = gtk::Label::builder()
        .label("")
        .css_classes(vec!["dim-label".to_string()])
        .build();
    let current = gtk::Label::builder()
        .label("")
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(vec!["dim-label".to_string()])
        .build();
    let bar = gtk::ProgressBar::new();
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
    column.append(&count);
    column.append(&current);
    column.append(&cancel);

    aw.show_content(&adw::Clamp::builder().child(&column).build(), "Scanning");

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
    // Sync, so the main thread's Session cannot cross the boundary (see run_view).
    {
        let sender = sender.clone();
        let cancelled = cancelled.clone();
        let profile = profile.clone();
        std::thread::spawn(move || {
            let last_item = std::cell::Cell::new(None::<Instant>);
            let forward = |p: Progress| {
                if let Progress::Item { .. } = p {
                    let now = Instant::now();
                    if last_item
                        .get()
                        .is_some_and(|t| now.duration_since(t) < ITEM_INTERVAL)
                    {
                        return;
                    }
                    last_item.set(Some(now));
                }
                let _ = sender.send_blocking(Message::Progress(p));
            };
            let result = (|| -> Result<Option<Plan>, String> {
                let worker = Session::open(&profile).map_err(|e| e.to_string())?;
                worker
                    .prepare(
                        false,
                        compare,
                        &Reporter::new(&forward, &|| cancelled.load(Ordering::Relaxed)),
                    )
                    .map_err(|e| e.to_string())
            })();
            let _ = sender.send_blocking(Message::Done(result));
        });
    }

    let aw = aw.clone();
    glib::spawn_future_local(async move {
        let mut total = None;
        while let Ok(message) = receiver.recv().await {
            match message {
                Message::Progress(Progress::Started { phase, total: n }) => {
                    total = n;
                    heading.set_label(scanning_label(phase));
                    count.set_label("");
                    current.set_label("");
                    bar.set_fraction(0.0);
                }
                Message::Progress(Progress::Item {
                    index, rel_path, ..
                }) => {
                    let done = index + 1;
                    match total {
                        // A walk does not know its size, so the bar pulses. The
                        // worker's throttle sets the rhythm.
                        None => {
                            bar.pulse();
                            count.set_label(&format!("{done} files"));
                        }
                        Some(n) => {
                            if n > 0 {
                                bar.set_fraction(done as f64 / n as f64);
                            }
                            count.set_label(&format!("{done} / {n}"));
                        }
                    }
                    current.set_label(&rel_path);
                }
                Message::Progress(Progress::Finished { .. }) => {
                    if total.is_some() {
                        bar.set_fraction(1.0);
                    }
                }
                // Scanning and comparing move no file content.
                Message::Progress(Progress::Bytes { .. }) => {}
                Message::Done(Ok(Some(plan))) => {
                    plan_view::show(&aw, session, plan);
                    break;
                }
                // Nothing was written, so there is nothing to report: go back
                // to where the user pressed Scan.
                Message::Done(Ok(None)) => {
                    profile_page::show(&aw, &profile);
                    break;
                }
                Message::Done(Err(message)) => {
                    profile_page::show(&aw, &profile);
                    profile_page::show_error(&aw, &message);
                    break;
                }
            }
        }
    });
}

fn scanning_label(phase: Phase) -> &'static str {
    match phase {
        Phase::ScanLocal => "Scanning local files…",
        Phase::ScanRemote => "Scanning remote files…",
        Phase::Compare => "Comparing…",
        Phase::Apply => "Applying…",
    }
}
