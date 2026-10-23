//! The progress vocabulary shared by every long-running phase.

/// How long a phase that waits on worker threads may go between checks for
/// cancellation, so one slow file cannot make Cancel feel unresponsive.
pub(crate) const CANCEL_POLL: std::time::Duration = std::time::Duration::from_millis(100);

/// Which part of a sync is reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    ScanLocal,
    ScanRemote,
    Compare,
    Apply,
}

impl Phase {
    /// A human-readable name for the phase, in progressive form.
    pub fn label(self) -> &'static str {
        match self {
            Phase::ScanLocal => "Scanning local",
            Phase::ScanRemote => "Scanning remote",
            Phase::Compare => "Comparing",
            Phase::Apply => "Applying",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    /// `total: None` means the size of the phase is not known ahead of time,
    /// which is a frontend's signal to show an indeterminate indicator.
    Started {
        phase: Phase,
        total: Option<usize>,
    },
    Item {
        phase: Phase,
        index: usize,
        rel_path: String,
    },
    /// Bytes of file content transferred so far in the phase, out of `total`.
    /// Sent between items too, so a long copy shows movement.
    Bytes {
        phase: Phase,
        done: u64,
        total: u64,
    },
    Finished {
        phase: Phase,
        count: usize,
    },
}

/// Bundles a progress sink with a cancellation check, so a phase takes one
/// argument rather than two closures.
pub struct Reporter<'a> {
    progress: &'a dyn Fn(Progress),
    cancel: &'a dyn Fn() -> bool,
}

impl<'a> Reporter<'a> {
    pub fn new(progress: &'a dyn Fn(Progress), cancel: &'a dyn Fn() -> bool) -> Self {
        Reporter { progress, cancel }
    }

    /// A reporter that discards progress and never cancels.
    pub fn silent() -> Reporter<'static> {
        Reporter {
            progress: &|_| {},
            cancel: &|| false,
        }
    }

    pub fn cancelled(&self) -> bool {
        (self.cancel)()
    }

    /// Pass on an event produced elsewhere, such as by a scan running on
    /// another thread with a reporter of its own.
    pub(crate) fn emit(&self, event: Progress) {
        (self.progress)(event);
    }

    pub fn started(&self, phase: Phase, total: Option<usize>) {
        (self.progress)(Progress::Started { phase, total });
    }

    pub fn item(&self, phase: Phase, index: usize, rel_path: &str) {
        (self.progress)(Progress::Item {
            phase,
            index,
            rel_path: rel_path.to_string(),
        });
    }

    pub fn bytes(&self, phase: Phase, done: u64, total: u64) {
        (self.progress)(Progress::Bytes { phase, done, total });
    }

    pub fn finished(&self, phase: Phase, count: usize) {
        (self.progress)(Progress::Finished { phase, count });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_silent_reporter_never_cancels() {
        assert!(!Reporter::silent().cancelled());
    }

    #[test]
    fn events_reach_the_sink_with_their_phase() {
        let seen = std::cell::RefCell::new(Vec::new());
        let sink = |p: Progress| seen.borrow_mut().push(p);
        let reporter = Reporter::new(&sink, &|| false);

        reporter.started(Phase::ScanLocal, None);
        reporter.item(Phase::ScanLocal, 0, "a.txt");
        reporter.finished(Phase::ScanLocal, 1);

        assert_eq!(
            seen.into_inner(),
            vec![
                Progress::Started {
                    phase: Phase::ScanLocal,
                    total: None
                },
                Progress::Item {
                    phase: Phase::ScanLocal,
                    index: 0,
                    rel_path: "a.txt".into()
                },
                Progress::Finished {
                    phase: Phase::ScanLocal,
                    count: 1
                },
            ]
        );
    }

    #[test]
    fn cancellation_is_read_through_on_every_check() {
        let flag = std::cell::Cell::new(false);
        let sink = |_| {};
        let cancel = || flag.get();
        let reporter = Reporter::new(&sink, &cancel);
        assert!(!reporter.cancelled());
        flag.set(true);
        assert!(reporter.cancelled());
    }
}
