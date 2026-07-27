use filesyncer_core::{Action, ConflictKind, Plan, RunSummary, Side, SkipReason};

pub fn plan_summary(plan: &Plan) -> String {
    let mut out = String::new();

    let mut section = |title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            out.push_str(title);
            out.push('\n');
            for line in lines {
                out.push_str("    ");
                out.push_str(&line);
                out.push('\n');
            }
            out.push('\n');
        }
    };

    for (side, label) in [
        (Side::Remote, "Copy to remote"),
        (Side::Local, "Copy to local"),
    ] {
        let lines: Vec<String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Copy {
                    rel_path,
                    from,
                    size,
                    ..
                } if from.other() == side => Some(format!("{rel_path}  ({})", human_bytes(*size))),
                _ => None,
            })
            .collect();
        let header = format!(
            "{label}  {} files, {}",
            plan.copy_count(side),
            human_bytes(plan.copy_bytes(side))
        );
        section(&header, lines);
    }

    let trash: Vec<String> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Trash { rel_path, side } => Some(format!("{rel_path}  (on {side:?})")),
            _ => None,
        })
        .collect();
    section(&format!("Move to trash  {} files", trash.len()), trash);

    let conflicts: Vec<String> = plan
        .conflicts
        .iter()
        .map(|c| format!("{}  — {}", c.rel_path, describe_conflict(c.kind)))
        .collect();
    section(&format!("Conflicts  {} files", conflicts.len()), conflicts);

    let skips: Vec<String> = plan
        .skips
        .iter()
        .map(|s| match &s.reason {
            SkipReason::Symlink => format!("{}  — symlink", s.rel_path),
            SkipReason::Unreadable(why) => format!("{}  — unreadable: {why}", s.rel_path),
        })
        .collect();
    section(&format!("Skipped  {} entries", skips.len()), skips);

    if plan.is_noop() && plan.conflicts.is_empty() {
        out.push_str("Nothing to do.\n");
    }
    out
}

fn describe_conflict(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothEdited => "edited on both sides",
        ConflictKind::BothNew => "created on both sides with different contents",
        ConflictKind::EditedLocalDeletedRemote => "edited locally, deleted on remote",
        ConflictKind::EditedRemoteDeletedLocal => "edited on remote, deleted locally",
        ConflictKind::TypeMismatch => "file on one side, directory on the other",
    }
}

pub fn run_summary(summary: &RunSummary) -> String {
    let mut out = String::new();
    if summary.cancelled {
        out.push_str("Cancelled.\n");
    }
    out.push_str(&format!(
        "{} copied, {} moved to trash.\n",
        summary.copied, summary.trashed
    ));
    if summary.conflicts > 0 {
        out.push_str(&format!(
            "{} conflict(s) left unresolved; they will be reported again next run.\n",
            summary.conflicts
        ));
    }
    if !summary.errors.is_empty() {
        out.push_str(&format!("{} error(s):\n", summary.errors.len()));
        for (path, message) in &summary.errors {
            out.push_str(&format!("    {path}: {message}\n"));
        }
    }
    out
}

pub fn confirm(prompt: &str) -> std::io::Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(
        line.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn human_bytes(bytes: u64) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use filesyncer_core::{Conflict, Skip, Stat};

    fn copy(rel: &str, from: Side, size: u64) -> Action {
        Action::Copy {
            rel_path: rel.into(),
            from,
            size,
            hash: "h".into(),
        }
    }

    #[test]
    fn an_empty_plan_says_so_plainly() {
        let text = plan_summary(&Plan::default());
        assert!(text.to_lowercase().contains("nothing to do"), "got: {text}");
    }

    #[test]
    fn copies_are_grouped_by_destination() {
        let plan = Plan {
            actions: vec![
                copy("a.txt", Side::Local, 1024),
                copy("b.txt", Side::Remote, 2048),
            ],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("Copy to remote"), "got: {text}");
        assert!(text.contains("Copy to local"), "got: {text}");
        assert!(text.contains("a.txt"), "got: {text}");
    }

    #[test]
    fn deletions_are_labelled_as_going_to_trash() {
        let plan = Plan {
            actions: vec![Action::Trash {
                rel_path: "gone.txt".into(),
                side: Side::Remote,
            }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.to_lowercase().contains("trash"), "got: {text}");
        assert!(text.contains("gone.txt"), "got: {text}");
    }

    #[test]
    fn conflicts_name_the_kind_in_words() {
        let plan = Plan {
            conflicts: vec![Conflict {
                rel_path: "notes.md".into(),
                kind: ConflictKind::EditedLocalDeletedRemote,
                local: Some(Stat { size: 1, mtime: 1 }),
                remote: None,
            }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("notes.md"), "got: {text}");
        assert!(
            text.contains("edited locally, deleted on remote"),
            "conflict kinds must be readable; got: {text}"
        );
    }

    #[test]
    fn state_only_actions_are_not_listed_as_work() {
        let plan = Plan {
            actions: vec![Action::DropState {
                rel_path: "old.txt".into(),
            }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.to_lowercase().contains("nothing to do"), "got: {text}");
    }

    #[test]
    fn skips_are_listed_with_their_reason() {
        let plan = Plan {
            skips: vec![Skip {
                rel_path: "link.txt".into(),
                reason: SkipReason::Symlink,
            }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("link.txt"), "got: {text}");
        assert!(text.to_lowercase().contains("symlink"), "got: {text}");
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn a_run_summary_reports_errors_individually() {
        let summary = RunSummary {
            copied: 2,
            trashed: 1,
            conflicts: 0,
            errors: vec![
                ("a.txt".into(), "permission denied".into()),
                ("b.txt".into(), "no space left".into()),
            ],
            cancelled: false,
        };
        let text = run_summary(&summary);
        assert!(text.contains("a.txt: permission denied"), "got: {text}");
        assert!(text.contains("b.txt: no space left"), "got: {text}");
    }

    #[test]
    fn a_cancelled_run_says_so() {
        let summary = RunSummary {
            cancelled: true,
            ..Default::default()
        };
        assert!(run_summary(&summary).to_lowercase().contains("cancelled"));
    }

    #[test]
    fn conflicts_in_a_summary_tell_the_user_what_to_do() {
        let summary = RunSummary {
            conflicts: 2,
            ..Default::default()
        };
        let text = run_summary(&summary);
        assert!(text.contains('2'), "got: {text}");
        assert!(text.to_lowercase().contains("conflict"), "got: {text}");
    }
}
