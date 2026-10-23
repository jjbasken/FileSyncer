//! How much of a sync is left and how long it should take.
//!
//! Time is estimated from bytes alone: copies dominate a sync, and a trash or
//! a state row costs next to nothing beside them. Before a sync, the rate is
//! the one the previous sync measured in each direction; during one, it is
//! the rate seen so far.

use crate::plan::{Action, Plan, Side};
use crate::progress::{Phase, Progress};
use std::time::{Duration, Instant};

/// How long a sync runs before its own rate replaces the remembered one. Any
/// shorter and the first file's open and the disk's spin-up skew it.
const WARMUP: Duration = Duration::from_secs(3);

/// What is left of a sync.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Remaining {
    /// Copies and trashes not yet finished, counting the one under way.
    pub items: usize,
    pub bytes: u64,
    /// `None` until a rate is known: no sync has been measured before and
    /// this one has not run long enough to measure itself.
    pub time: Option<Duration>,
}

/// How long copying `plan` should take at the given rates, in bytes per
/// second for copies into each side.
///
/// A direction never measured borrows the other's rate, which is a better
/// guess than none; with neither known there is no estimate.
pub fn estimate(plan: &Plan, to_local: Option<u64>, to_remote: Option<u64>) -> Option<Duration> {
    let mut secs = 0.0;
    for (side, rate) in [
        (Side::Local, to_local.or(to_remote)),
        (Side::Remote, to_remote.or(to_local)),
    ] {
        let bytes = plan.copy_bytes(side);
        if bytes == 0 {
            continue;
        }
        let rate = rate.filter(|&r| r > 0)?;
        secs += bytes as f64 / rate as f64;
    }
    Some(Duration::from_secs_f64(secs))
}

/// Follows a sync's progress events against its plan.
#[derive(Debug, Clone)]
pub struct Eta {
    /// For each action, how many copies and trashes come before it.
    work_before: Vec<usize>,
    total_items: usize,
    total_bytes: u64,
    items_done: usize,
    bytes_done: u64,
    started: Instant,
    /// Bytes per second to assume until this sync has measured its own.
    prior_rate: Option<f64>,
}

impl Eta {
    /// `estimate` is the time the whole plan was expected to take, if known.
    pub fn new(plan: &Plan, estimate: Option<Duration>, now: Instant) -> Eta {
        let mut work_before = Vec::with_capacity(plan.actions.len());
        let mut work = 0;
        for action in &plan.actions {
            work_before.push(work);
            if is_work(action) {
                work += 1;
            }
        }
        let total_bytes = plan.copy_bytes(Side::Local) + plan.copy_bytes(Side::Remote);
        let prior_rate = estimate
            .filter(|e| !e.is_zero())
            .map(|e| total_bytes as f64 / e.as_secs_f64());
        Eta {
            work_before,
            total_items: work,
            total_bytes,
            items_done: 0,
            bytes_done: 0,
            started: now,
            prior_rate,
        }
    }

    /// Take in one event of the apply phase; others are ignored.
    pub fn observe(&mut self, event: &Progress) {
        match *event {
            Progress::Item {
                phase: Phase::Apply,
                index,
                ..
            } => {
                if let Some(&before) = self.work_before.get(index) {
                    self.items_done = before;
                }
            }
            Progress::Bytes {
                phase: Phase::Apply,
                done,
                ..
            } => self.bytes_done = done.min(self.total_bytes),
            Progress::Finished {
                phase: Phase::Apply,
                count,
            } => {
                self.items_done = self
                    .work_before
                    .get(count)
                    .copied()
                    .unwrap_or(self.total_items);
                if count >= self.work_before.len() {
                    self.bytes_done = self.total_bytes;
                }
            }
            _ => {}
        }
    }

    /// Bytes per second: this sync's own once it has run past the warm-up,
    /// the remembered rate before that.
    pub fn rate(&self, now: Instant) -> Option<f64> {
        let elapsed = now.saturating_duration_since(self.started);
        if elapsed >= WARMUP && self.bytes_done > 0 {
            Some(self.bytes_done as f64 / elapsed.as_secs_f64())
        } else {
            self.prior_rate
        }
    }

    pub fn remaining(&self, now: Instant) -> Remaining {
        let bytes = self.total_bytes - self.bytes_done;
        let time = if bytes == 0 {
            Some(Duration::ZERO)
        } else {
            self.rate(now)
                .filter(|&r| r > 0.0)
                .map(|r| Duration::from_secs_f64(bytes as f64 / r))
        };
        Remaining {
            items: self.total_items - self.items_done,
            bytes,
            time,
        }
    }

    pub fn total_items(&self) -> usize {
        self.total_items
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }
}

/// Whether an action is one the user would count: a file copied or trashed,
/// not a state row.
pub fn is_work(action: &Action) -> bool {
    matches!(action, Action::Copy { .. } | Action::Trash { .. })
}

/// A rough, readable form of a duration: "about 4 min".
pub fn approx_duration(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 10 {
        "a few seconds".to_string()
    } else if secs < 60 {
        format!("about {} s", secs.div_ceil(10) * 10)
    } else if secs < 60 * 60 {
        format!("about {} min", secs.div_ceil(60))
    } else {
        let mins = secs.div_ceil(60);
        match mins % 60 {
            0 => format!("about {} h", mins / 60),
            m => format!("about {} h {m} min", mins / 60),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MB: u64 = 1_000_000;

    fn copy(path: &str, to: Side, size: u64) -> Action {
        Action::Copy {
            rel_path: path.into(),
            from: to.other(),
            size,
        }
    }

    fn record(path: &str) -> Action {
        Action::DropState {
            rel_path: path.into(),
        }
    }

    fn trash(path: &str) -> Action {
        Action::Trash {
            rel_path: path.into(),
            side: Side::Local,
        }
    }

    fn plan_of(actions: Vec<Action>) -> Plan {
        Plan {
            actions,
            ..Default::default()
        }
    }

    fn item(index: usize) -> Progress {
        Progress::Item {
            phase: Phase::Apply,
            index,
            rel_path: String::new(),
        }
    }

    fn bytes(done: u64, total: u64) -> Progress {
        Progress::Bytes {
            phase: Phase::Apply,
            done,
            total,
        }
    }

    #[test]
    fn an_estimate_adds_each_direction_at_its_own_rate() {
        let plan = plan_of(vec![
            copy("a", Side::Local, 100 * MB),
            copy("b", Side::Remote, 20 * MB),
        ]);
        let e = estimate(&plan, Some(10 * MB), Some(2 * MB)).unwrap();
        assert_eq!(e, Duration::from_secs(20));
    }

    #[test]
    fn an_unmeasured_direction_borrows_the_other_rate() {
        let plan = plan_of(vec![
            copy("a", Side::Local, 100 * MB),
            copy("b", Side::Remote, 20 * MB),
        ]);
        let e = estimate(&plan, None, Some(10 * MB)).unwrap();
        assert_eq!(e, Duration::from_secs(12));
    }

    #[test]
    fn with_no_rate_known_there_is_no_estimate() {
        let plan = plan_of(vec![copy("a", Side::Local, MB)]);
        assert_eq!(estimate(&plan, None, None), None);
    }

    #[test]
    fn a_plan_that_copies_nothing_takes_no_time() {
        let plan = plan_of(vec![trash("a"), record("b")]);
        assert_eq!(estimate(&plan, None, None), Some(Duration::ZERO));
    }

    #[test]
    fn items_count_copies_and_trashes_but_not_state_rows() {
        let plan = plan_of(vec![
            record("r1"),
            copy("a", Side::Local, MB),
            record("r2"),
            trash("t"),
        ]);
        let mut eta = Eta::new(&plan, None, Instant::now());
        assert_eq!(eta.total_items(), 2);
        eta.observe(&item(2));
        assert_eq!(eta.remaining(Instant::now()).items, 1);
    }

    #[test]
    fn the_item_under_way_is_still_remaining() {
        let plan = plan_of(vec![copy("a", Side::Local, MB), copy("b", Side::Local, MB)]);
        let mut eta = Eta::new(&plan, None, Instant::now());
        eta.observe(&item(0));
        assert_eq!(eta.remaining(Instant::now()).items, 2);
        eta.observe(&item(1));
        assert_eq!(eta.remaining(Instant::now()).items, 1);
    }

    #[test]
    fn before_the_warmup_the_remembered_rate_is_used() {
        let plan = plan_of(vec![copy("a", Side::Local, 100 * MB)]);
        let start = Instant::now();
        let mut eta = Eta::new(&plan, Some(Duration::from_secs(50)), start);
        eta.observe(&bytes(50 * MB, 100 * MB));
        let left = eta.remaining(start + Duration::from_secs(1));
        assert_eq!(left.bytes, 50 * MB);
        assert_eq!(left.time, Some(Duration::from_secs(25)));
    }

    #[test]
    fn after_the_warmup_the_measured_rate_is_used() {
        let plan = plan_of(vec![copy("a", Side::Local, 100 * MB)]);
        let start = Instant::now();
        let mut eta = Eta::new(&plan, Some(Duration::from_secs(50)), start);
        eta.observe(&bytes(20 * MB, 100 * MB));
        // 20 MB in 10 s is 2 MB/s, so 80 MB takes 40 s.
        let left = eta.remaining(start + Duration::from_secs(10));
        assert_eq!(left.time, Some(Duration::from_secs(40)));
    }

    #[test]
    fn with_nothing_remembered_time_is_unknown_until_measured() {
        let plan = plan_of(vec![copy("a", Side::Local, 100 * MB)]);
        let start = Instant::now();
        let mut eta = Eta::new(&plan, None, start);
        assert_eq!(eta.remaining(start).time, None);
        eta.observe(&bytes(10 * MB, 100 * MB));
        assert!(eta.remaining(start + WARMUP).time.is_some());
    }

    #[test]
    fn finishing_leaves_nothing() {
        let plan = plan_of(vec![copy("a", Side::Local, MB), trash("t")]);
        let mut eta = Eta::new(&plan, None, Instant::now());
        eta.observe(&Progress::Finished {
            phase: Phase::Apply,
            count: 2,
        });
        let left = eta.remaining(Instant::now());
        assert_eq!((left.items, left.bytes), (0, 0));
        assert_eq!(left.time, Some(Duration::ZERO));
    }

    #[test]
    fn cancellation_preserves_the_remaining_work() {
        let plan = plan_of(vec![
            record("r"),
            copy("a", Side::Local, MB),
            copy("b", Side::Local, 2 * MB),
            trash("t"),
        ]);
        let mut eta = Eta::new(&plan, None, Instant::now());
        eta.observe(&bytes(MB, 3 * MB));
        eta.observe(&Progress::Finished {
            phase: Phase::Apply,
            count: 2,
        });
        let left = eta.remaining(Instant::now());
        assert_eq!((left.items, left.bytes), (2, 2 * MB));
    }

    #[test]
    fn events_of_other_phases_are_ignored() {
        let plan = plan_of(vec![copy("a", Side::Local, MB)]);
        let mut eta = Eta::new(&plan, None, Instant::now());
        eta.observe(&Progress::Bytes {
            phase: Phase::Compare,
            done: MB,
            total: MB,
        });
        assert_eq!(eta.remaining(Instant::now()).bytes, MB);
    }

    #[test]
    fn durations_read_roughly() {
        let d = Duration::from_secs;
        assert_eq!(approx_duration(d(3)), "a few seconds");
        assert_eq!(approx_duration(d(42)), "about 50 s");
        assert_eq!(approx_duration(d(61)), "about 2 min");
        assert_eq!(approx_duration(d(3600)), "about 1 h");
        assert_eq!(approx_duration(d(5000)), "about 1 h 24 min");
    }
}
