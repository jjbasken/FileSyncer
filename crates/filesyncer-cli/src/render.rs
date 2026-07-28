use filesyncer_core::{Action, ConflictKind, Phase, Plan, Progress, RunSummary, Side, SkipReason};
use std::io::Write;
use std::time::{Duration, Instant};

/// How often the interactive status line may be redrawn.
const REDRAW_INTERVAL: Duration = Duration::from_millis(100);

/// Renders scan progress. Written against an injected writer and an explicit
/// TTY flag so both output shapes are testable without a terminal.
pub struct ScanReporter<W: Write> {
    out: W,
    tty: bool,
    /// Length of the status line currently on screen, so it can be blanked.
    drawn: usize,
    last_draw: Option<Instant>,
}

impl<W: Write> ScanReporter<W> {
    pub fn new(out: W, tty: bool) -> Self {
        ScanReporter {
            out,
            tty,
            drawn: 0,
            last_draw: None,
        }
    }

    pub fn handle(&mut self, event: Progress) {
        // Errors here are a closed or broken stdout; progress is not worth
        // failing a sync over, so they are dropped.
        let _ = self.write(event);
    }

    fn write(&mut self, event: Progress) -> std::io::Result<()> {
        match event {
            Progress::Started { .. } => {
                self.last_draw = None;
                Ok(())
            }

            // Non-interactive output says nothing until a phase is done: a log
            // file wants one line per phase, not one per file.
            Progress::Item { .. } if !self.tty => Ok(()),

            Progress::Item {
                phase,
                index,
                rel_path,
            } => {
                let now = Instant::now();
                if self
                    .last_draw
                    .is_some_and(|t| now.duration_since(t) < REDRAW_INTERVAL)
                {
                    return Ok(());
                }
                self.last_draw = Some(now);
                self.status_line(&format!(
                    "{}… {} files  {rel_path}",
                    phase.label(),
                    commas(index + 1)
                ))
            }

            Progress::Finished { phase, count } => {
                self.last_draw = None;
                if self.tty {
                    self.clear_line()
                } else {
                    writeln!(self.out, "{}… {}", phase.label(), summarise(phase, count))
                }
            }
        }
    }

    fn status_line(&mut self, text: &str) -> std::io::Result<()> {
        // Pad to the previous width so a shorter line does not leave debris.
        let pad = self.drawn.saturating_sub(text.chars().count());
        write!(self.out, "\r{text}{:pad$}", "", pad = pad)?;
        self.drawn = text.chars().count();
        self.out.flush()
    }

    fn clear_line(&mut self) -> std::io::Result<()> {
        if self.drawn > 0 {
            write!(self.out, "\r{:width$}\r", "", width = self.drawn)?;
            self.drawn = 0;
            self.out.flush()?;
        }
        Ok(())
    }
}

fn summarise(phase: Phase, count: usize) -> String {
    match phase {
        // Comparing reports work done rather than files seen; the two differ,
        // and "40 files" after "1,284 files" would read as a vanished tree.
        Phase::Compare => format!("{} files hashed", commas(count)),
        _ => format!("{} files", commas(count)),
    }
}

/// Group digits in threes, so a six-figure file count stays readable.
fn commas(n: usize) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

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

    fn item(phase: Phase, index: usize, rel_path: &str) -> Progress {
        Progress::Item {
            phase,
            index,
            rel_path: rel_path.into(),
        }
    }

    /// Drive a reporter over a buffer and return what it wrote.
    fn driven(tty: bool, events: Vec<Progress>) -> String {
        let mut reporter = ScanReporter::new(Vec::new(), tty);
        for e in events {
            reporter.handle(e);
        }
        String::from_utf8(reporter.out).unwrap()
    }

    #[test]
    fn non_interactive_output_says_nothing_until_a_phase_finishes() {
        let text = driven(
            false,
            vec![
                Progress::Started {
                    phase: Phase::ScanLocal,
                    total: None,
                },
                item(Phase::ScanLocal, 0, "a.txt"),
                item(Phase::ScanLocal, 1, "b.txt"),
            ],
        );
        assert!(text.is_empty(), "got: {text:?}");
    }

    #[test]
    fn non_interactive_output_is_one_plain_line_per_phase() {
        let text = driven(
            false,
            vec![
                Progress::Started {
                    phase: Phase::ScanLocal,
                    total: None,
                },
                item(Phase::ScanLocal, 0, "a.txt"),
                Progress::Finished {
                    phase: Phase::ScanLocal,
                    count: 1284,
                },
                Progress::Started {
                    phase: Phase::ScanRemote,
                    total: None,
                },
                Progress::Finished {
                    phase: Phase::ScanRemote,
                    count: 1290,
                },
                Progress::Started {
                    phase: Phase::Compare,
                    total: Some(40),
                },
                Progress::Finished {
                    phase: Phase::Compare,
                    count: 40,
                },
            ],
        );
        assert_eq!(
            text,
            "Scanning local… 1,284 files\n\
             Scanning remote… 1,290 files\n\
             Comparing… 40 files hashed\n"
        );
        assert!(!text.contains('\r'), "logs must not get control codes");
    }

    #[test]
    fn interactive_output_rewrites_one_status_line_naming_the_current_path() {
        let text = driven(
            true,
            vec![
                Progress::Started {
                    phase: Phase::ScanRemote,
                    total: None,
                },
                item(Phase::ScanRemote, 1283, "photos/2024/img_09.jpg"),
            ],
        );
        assert!(text.starts_with('\r'), "got: {text:?}");
        assert!(
            text.contains("Scanning remote… 1,284 files  photos/2024/img_09.jpg"),
            "got: {text:?}"
        );
        assert!(!text.contains('\n'), "the line is rewritten, not appended");
    }

    #[test]
    fn interactive_output_throttles_redraws() {
        // Every item arrives well inside the redraw interval, so only the first
        // reaches the terminal.
        let text = driven(
            true,
            vec![
                Progress::Started {
                    phase: Phase::ScanLocal,
                    total: None,
                },
                item(Phase::ScanLocal, 0, "first.txt"),
                item(Phase::ScanLocal, 1, "second.txt"),
                item(Phase::ScanLocal, 2, "third.txt"),
            ],
        );
        assert!(text.contains("first.txt"), "got: {text:?}");
        assert!(!text.contains("second.txt"), "got: {text:?}");
        assert!(!text.contains("third.txt"), "got: {text:?}");
    }

    #[test]
    fn interactive_output_clears_the_line_when_a_phase_finishes() {
        let text = driven(
            true,
            vec![
                Progress::Started {
                    phase: Phase::ScanLocal,
                    total: None,
                },
                item(Phase::ScanLocal, 0, "a.txt"),
                Progress::Finished {
                    phase: Phase::ScanLocal,
                    count: 1,
                },
            ],
        );
        assert!(
            text.ends_with('\r'),
            "the cursor must return to column zero so the summary starts clean; got: {text:?}"
        );
        let tail = text.rsplit_once("a.txt").unwrap().1;
        assert!(
            tail.trim_matches(|c| c == '\r' || c == ' ').is_empty(),
            "the status line must be blanked, not left on screen; got: {tail:?}"
        );
    }

    #[test]
    fn a_shorter_status_line_erases_the_longer_one_before_it() {
        let mut reporter = ScanReporter::new(Vec::new(), true);
        reporter
            .write(item(Phase::ScanLocal, 0, "a/very/long/path/indeed.txt"))
            .unwrap();
        // Bypass the throttle, which is not what this test is about.
        reporter.last_draw = None;
        reporter.write(item(Phase::ScanLocal, 1, "b.txt")).unwrap();

        let text = String::from_utf8(reporter.out).unwrap();
        let last = text.rsplit('\r').next().unwrap();
        assert!(last.contains("b.txt"), "got: {last:?}");
        assert!(
            last.len() > "Scanning local… 2 files  b.txt".len(),
            "the shorter line must be padded over the longer one; got: {last:?}"
        );
        assert!(!last.contains("indeed.txt"), "got: {last:?}");
    }

    #[test]
    fn a_phase_finishing_with_no_line_on_screen_writes_nothing() {
        let text = driven(
            true,
            vec![
                Progress::Started {
                    phase: Phase::Compare,
                    total: Some(0),
                },
                Progress::Finished {
                    phase: Phase::Compare,
                    count: 0,
                },
            ],
        );
        assert!(text.is_empty(), "got: {text:?}");
    }

    #[test]
    fn file_counts_are_grouped_in_threes() {
        assert_eq!(commas(0), "0");
        assert_eq!(commas(999), "999");
        assert_eq!(commas(1_000), "1,000");
        assert_eq!(commas(1_284), "1,284");
        assert_eq!(commas(1_234_567), "1,234,567");
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
