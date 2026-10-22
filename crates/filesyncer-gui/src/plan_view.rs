use crate::run_view;
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::eta::is_work;
use filesyncer_core::{
    Action, Conflict, ConflictKind, Plan, Session, Side, SkipReason, approx_duration,
};
use std::cell::RefCell;
use std::rc::Rc;

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let plan = Rc::new(RefCell::new(plan));
    render(aw, session, plan);
}

/// Rebuilt wholesale after each conflict resolution. At the scale this tool
/// targets that is cheaper than surgically updating rows, and it keeps the
/// displayed plan and the real plan impossible to desynchronise.
fn render(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Rc<RefCell<Plan>>) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    {
        let p = plan.borrow();

        let files = p.actions.iter().filter(|a| is_work(a)).count();
        if files > 0 {
            let bytes = p.copy_bytes(Side::Local) + p.copy_bytes(Side::Remote);
            // An unreadable rate only costs the estimate, not the review.
            let time = match session.estimate(&p).ok().flatten() {
                Some(t) => approx_duration(t),
                None => "time unknown until a sync has measured the speed".to_string(),
            };
            let group = adw::PreferencesGroup::new();
            group.add(
                &adw::ActionRow::builder()
                    .title("Total")
                    .subtitle(format!("{files} files · {} · {time}", human_bytes(bytes)))
                    .build(),
            );
            column.append(&group);
        }

        for (side, label) in [
            (Side::Remote, "Copy to remote"),
            (Side::Local, "Copy to local"),
        ] {
            let paths: Vec<String> = p
                .actions
                .iter()
                .filter_map(|a| match a {
                    Action::Copy { rel_path, from, .. } if from.other() == side => {
                        Some(rel_path.clone())
                    }
                    _ => None,
                })
                .collect();
            if !paths.is_empty() {
                column.append(&expander(
                    label,
                    &format!("{} files, {}", paths.len(), human_bytes(p.copy_bytes(side))),
                    paths.iter().map(|s| plain_row(s)).collect(),
                    false,
                ));
            }
        }

        let trashed: Vec<String> = p
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Trash { rel_path, side } => {
                    Some(format!("{rel_path} (on {})", side_name(*side)))
                }
                _ => None,
            })
            .collect();
        if !trashed.is_empty() {
            column.append(&expander(
                "Move to trash",
                &format!("{} files", trashed.len()),
                trashed.iter().map(|s| plain_row(s)).collect(),
                false,
            ));
        }

        if !p.conflicts.is_empty() {
            let rows: Vec<adw::ActionRow> = p
                .conflicts
                .iter()
                .map(|c| conflict_row(aw, &session, &plan, c))
                .collect();
            column.append(&expander(
                "Conflicts",
                &format!("{} files", p.conflicts.len()),
                rows,
                true,
            ));
        }

        if !p.skips.is_empty() {
            let rows: Vec<adw::ActionRow> = p
                .skips
                .iter()
                .map(|s| match &s.reason {
                    SkipReason::Symlink => labelled_row(&s.rel_path, "symlink"),
                    SkipReason::Unreadable(why) => labelled_row(&s.rel_path, why),
                })
                .collect();
            column.append(&expander(
                "Skipped",
                &format!("{} entries", rows.len()),
                rows,
                false,
            ));
        }

        if p.is_noop() && p.conflicts.is_empty() {
            column.append(
                &adw::StatusPage::builder()
                    .icon_name("object-select-symbolic")
                    .title("Everything is in sync")
                    .description(if p.actions.is_empty() {
                        "No changes are needed."
                    } else {
                        "Save sync state to remember this comparison for the next scan."
                    })
                    .build(),
            );
        }
    }

    let cancel = gtk::Button::with_label("Cancel");
    let state_only = plan.borrow().is_noop();
    let apply = gtk::Button::builder()
        .label(if state_only {
            "Save sync state"
        } else {
            "Apply"
        })
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    apply.set_sensitive(!plan.borrow().actions.is_empty());

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&apply);
    column.append(&buttons);

    {
        let aw = aw.clone();
        cancel.connect_clicked(move |_| aw.show_placeholder());
    }
    {
        let aw = aw.clone();
        let session = session.clone();
        let plan = plan.clone();
        apply.connect_clicked(move |_| {
            run_view::show(&aw, session.clone(), plan.borrow().clone());
        });
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&adw::Clamp::builder().child(&column).build())
        .vexpand(true)
        .build();
    aw.show_content(&scrolled, "Review plan");
}

fn conflict_row(
    aw: &Rc<AppWindow>,
    session: &Rc<Session>,
    plan: &Rc<RefCell<Plan>>,
    conflict: &Conflict,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&conflict.rel_path)
        .subtitle(describe(conflict.kind))
        .build();

    for (side, label) in [(Side::Local, "Keep local"), (Side::Remote, "Keep remote")] {
        let button = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build();
        let aw = aw.clone();
        let session = session.clone();
        let plan = plan.clone();
        let rel_path = conflict.rel_path.clone();
        button.connect_clicked(move |_| {
            // Resolution rewrites the conflict into an ordinary action before
            // apply ever runs; the engine needs no special path for it.
            if plan.borrow_mut().resolve_conflict(&rel_path, side).is_ok() {
                render(&aw, session.clone(), plan.clone());
            }
        });
        row.add_suffix(&button);
    }
    row
}

fn describe(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothEdited => "edited on both sides",
        ConflictKind::BothNew => "created on both sides with different contents",
        ConflictKind::EditedLocalDeletedRemote => "edited locally, deleted on remote",
        ConflictKind::EditedRemoteDeletedLocal => "edited on remote, deleted locally",
        ConflictKind::TypeMismatch => "file on one side, directory on the other",
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Local => "local",
        Side::Remote => "remote",
    }
}

/// Builds a `PreferencesGroup` wrapping an `ExpanderRow`. Returning the group
/// (not the expander) keeps the widget alive once appended to the column.
fn expander(
    title: &str,
    subtitle: &str,
    rows: Vec<adw::ActionRow>,
    expanded: bool,
) -> adw::PreferencesGroup {
    let expander = adw::ExpanderRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    for row in rows {
        expander.add_row(&row);
    }
    expander.set_expanded(expanded);
    let group = adw::PreferencesGroup::new();
    group.add(&expander);
    group
}

fn plain_row(text: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(text).build()
}

fn labelled_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build()
}

pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}
