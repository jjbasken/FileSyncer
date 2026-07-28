//! The progress vocabulary shared by every long-running phase.

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
