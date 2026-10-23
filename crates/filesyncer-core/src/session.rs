use crate::apply::{ApplyCtx, RunSummary, apply, run_stamp_now};
use crate::eta;
use crate::marker;
use crate::plan::{Plan, Side};
use crate::profile::Profile;
use crate::progress::{CANCEL_POLL, Phase, Progress, Reporter};
use crate::reconcile::{Compare, FsHashProvider, reconcile};
use crate::scan::{IGNORE_FILE, Scan, scan_root};
use crate::state::StateStore;
use crate::{Result, profile};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

#[derive(Debug)]
pub struct Session {
    profile: Profile,
    state: StateStore,
}

impl Session {
    pub fn open(profile: &Profile) -> Result<Session> {
        Self::open_with_state_path(profile, &profile::state_db_path(&profile.id))
    }

    pub fn open_with_state_path(profile: &Profile, state_path: &Path) -> Result<Session> {
        profile.validate()?;
        // Marker checks come before anything reads the trees: an unmounted
        // share must be caught before it can look like a mass deletion.
        marker::verify_marker(&profile.local, &profile.id)?;
        marker::verify_marker(&profile.remote, &profile.id)?;
        Ok(Session {
            profile: profile.clone(),
            state: StateStore::open(state_path)?,
        })
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    /// Scan both roots and reconcile them into a plan.
    ///
    /// Returns `Ok(None)` when the scan was cancelled. Cancellation is an absent
    /// plan rather than an `Error` because nothing failed and nothing was
    /// written; an error would force every caller to tell a cancelled scan apart
    /// from a broken one.
    pub fn prepare(
        &self,
        force: bool,
        compare: Compare,
        reporter: &Reporter,
    ) -> Result<Option<Plan>> {
        marker::verify_marker(&self.profile.local, &self.profile.id)?;
        marker::verify_marker(&self.profile.remote, &self.profile.id)?;

        let Some((local, remote)) = self.scan_both(reporter)? else {
            return Ok(None);
        };

        let state = self.state.load_all()?;
        let hashes = FsHashProvider {
            local_root: self.profile.local.clone(),
            remote_root: self.profile.remote.clone(),
        };
        let Some(plan) = reconcile(&local, &remote, &state, &hashes, compare, reporter)? else {
            return Ok(None);
        };

        if !force {
            plan.check_deletion_threshold(state.len())?;
        }
        Ok(Some(plan))
    }

    /// Scan the two roots at the same time, so that a slow side (usually the
    /// remote share) is walked while the local disk is.
    ///
    /// Frontends expect one phase at a time, so only the local scan reports
    /// live. The remote scan's events wait in a bounded queue and are passed
    /// on once the local phase has finished. While the queue is full its file
    /// items are dropped, which a count-up display shows as a jump, but its
    /// start and finish are always kept.
    fn scan_both(&self, reporter: &Reporter) -> Result<Option<(Scan, Scan)>> {
        let local_ignore = self.profile.local.join(IGNORE_FILE);
        let remote_ignore = self.profile.remote.join(IGNORE_FILE);

        // Set when the user cancels or the local scan fails, to stop the
        // remote walk early.
        let stop = AtomicBool::new(false);
        // Set when the remote scan fails, to stop the local walk early rather
        // than make the user wait it out before seeing the error.
        let remote_failed = AtomicBool::new(false);

        // Borrowed apart from `self`, whose database connection is not `Sync`.
        let remote_root = &self.profile.remote;

        std::thread::scope(|scope| {
            // Created inside the scope so that an early return drops the
            // receiver before the scope joins, which releases a remote thread
            // blocked sending into a full queue.
            let (tx, rx) = mpsc::sync_channel::<Progress>(REMOTE_BACKLOG);

            let remote = scope.spawn(|| {
                let sink = move |p| match p {
                    Progress::Item { .. } => {
                        let _ = tx.try_send(p);
                    }
                    _ => {
                        let _ = tx.send(p);
                    }
                };
                let cancel = || stop.load(Ordering::Relaxed);
                let result = scan_root(
                    remote_root,
                    std::slice::from_ref(&local_ignore),
                    Phase::ScanRemote,
                    &Reporter::new(&sink, &cancel),
                );
                if result.is_err() {
                    remote_failed.store(true, Ordering::Relaxed);
                }
                result
            });

            let forward = |p| reporter.emit(p);
            let cancel = || {
                let cancelled = reporter.cancelled();
                if cancelled {
                    stop.store(true, Ordering::Relaxed);
                }
                cancelled || remote_failed.load(Ordering::Relaxed)
            };
            let local = scan_root(
                &self.profile.local,
                std::slice::from_ref(&remote_ignore),
                Phase::ScanLocal,
                &Reporter::new(&forward, &cancel),
            );
            let local = match local {
                Ok(Some(scan)) => scan,
                Ok(None) if remote_failed.load(Ordering::Relaxed) => {
                    return join(remote).map(|_| None);
                }
                other => {
                    stop.store(true, Ordering::Relaxed);
                    return other.map(|_| None);
                }
            };

            // Replay the remote scan's events, live from here on, until its
            // reporter is dropped at the end of the walk.
            loop {
                match rx.recv_timeout(CANCEL_POLL) {
                    Ok(event) => reporter.emit(event),
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
                if reporter.cancelled() {
                    stop.store(true, Ordering::Relaxed);
                    return Ok(None);
                }
            }

            Ok(join(remote)?.map(|remote| (local, remote)))
        })
    }

    pub fn run(&self, plan: &Plan, reporter: &Reporter) -> Result<RunSummary> {
        self.run_with_force(plan, false, reporter)
    }

    /// Apply the final reviewed plan. Recheck guards after user choices and
    /// before cleanup or any writes; a mount may have disappeared during review.
    pub fn run_with_force(
        &self,
        plan: &Plan,
        force: bool,
        reporter: &Reporter,
    ) -> Result<RunSummary> {
        marker::verify_marker(&self.profile.local, &self.profile.id)?;
        marker::verify_marker(&self.profile.remote, &self.profile.id)?;
        if !force {
            plan.check_deletion_threshold(self.state.tracked_count()?)?;
        }
        let ctx = ApplyCtx {
            local_root: &self.profile.local,
            remote_root: &self.profile.remote,
            state: &self.state,
            run_stamp: run_stamp_now(),
        };
        let summary = apply(plan, &ctx, reporter)?;

        // A cancelled run still measured the copies it finished.
        for to in [Side::Local, Side::Remote] {
            let transfer = summary.transfer(to);
            if transfer.time >= MIN_RATE_SAMPLE {
                let rate = transfer.bytes as f64 / transfer.time.as_secs_f64();
                self.state.set_rate(to, rate as u64)?;
            }
        }

        // Only a run that finished represents a synchronised moment in time.
        if !summary.cancelled {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            self.state.set_last_sync(now)?;
        }
        Ok(summary)
    }

    pub fn last_sync(&self) -> Result<Option<i64>> {
        self.state.last_sync()
    }

    /// How long applying `plan` should take, at the rates the last sync
    /// measured. `None` until a sync has copied enough to measure.
    pub fn estimate(&self, plan: &Plan) -> Result<Option<Duration>> {
        Ok(eta::estimate(
            plan,
            self.state.rate(Side::Local)?,
            self.state.rate(Side::Remote)?,
        ))
    }

    pub fn tracked_count(&self) -> Result<usize> {
        self.state.tracked_count()
    }
}

/// The least copying time in one direction that is trusted as a rate. Shorter
/// runs are mostly the cost of opening files, not of moving bytes.
const MIN_RATE_SAMPLE: Duration = Duration::from_secs(1);

/// How many remote scan events may wait while the local scan reports.
const REMOTE_BACKLOG: usize = 256;

/// Join a scan thread, passing on its panic as this thread's.
fn join<T>(handle: std::thread::ScopedJoinHandle<'_, T>) -> T {
    handle
        .join()
        .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
}

/// Validate a new profile and stamp both roots with markers.
pub fn create_profile_roots(profile: &Profile) -> Result<()> {
    profile.validate()?;
    marker::write_marker(&profile.local, &profile.id)?;
    marker::write_marker(&profile.remote, &profile.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        dir: tempfile::TempDir,
        profile: Profile,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let profile = Profile {
            id: "t".into(),
            name: "T".into(),
            local,
            remote,
        };
        create_profile_roots(&profile).unwrap();
        Fixture { dir, profile }
    }

    impl Fixture {
        fn session(&self) -> Session {
            Session::open_with_state_path(&self.profile, &self.dir.path().join("state.db")).unwrap()
        }
        fn write(&self, side: &str, rel: &str, contents: &str) {
            let p = self.profile_root(side).join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
        fn profile_root(&self, side: &str) -> std::path::PathBuf {
            match side {
                "local" => self.profile.local.clone(),
                _ => self.profile.remote.clone(),
            }
        }
    }

    /// Prepare with no progress reporting, for the tests that only care about
    /// the resulting plan.
    fn prepared(session: &Session, force: bool) -> Plan {
        session
            .prepare(force, Compare::Quick, &Reporter::silent())
            .unwrap()
            .expect("a silent reporter never cancels")
    }

    #[test]
    fn create_profile_roots_writes_both_markers() {
        let f = fixture();
        marker::verify_marker(&f.profile.local, "t").unwrap();
        marker::verify_marker(&f.profile.remote, "t").unwrap();
    }

    #[test]
    fn opening_without_a_marker_fails() {
        let f = fixture();
        std::fs::remove_file(f.profile.remote.join(marker::MARKER_FILE)).unwrap();
        let err =
            Session::open_with_state_path(&f.profile, &f.dir.path().join("s.db")).unwrap_err();
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn opening_a_nested_profile_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let profile = Profile {
            id: "t".into(),
            name: "T".into(),
            local: dir.path().to_path_buf(),
            remote: inner,
        };
        assert!(create_profile_roots(&profile).is_err());
    }

    #[test]
    fn prepare_finds_new_files_on_both_sides() {
        let f = fixture();
        f.write("local", "a.txt", "aaa");
        f.write("remote", "b.txt", "bbb");
        let plan = prepared(&f.session(), false);
        assert_eq!(plan.copy_count(crate::Side::Remote), 1);
        assert_eq!(plan.copy_count(crate::Side::Local), 1);
    }

    #[test]
    fn markers_are_never_part_of_a_plan() {
        let f = fixture();
        f.write("local", "a.txt", "aaa");
        let plan = prepared(&f.session(), false);
        assert_eq!(plan.actions.len(), 1, "got: {:?}", plan.actions);
    }

    #[test]
    fn syncignore_from_either_root_filters_both_sides() {
        let f = fixture();
        f.write("remote", IGNORE_FILE, "*.tmp\n");
        f.write("local", "keep.txt", "k");
        f.write("local", "scratch.tmp", "s");
        let plan = prepared(&f.session(), false);
        let paths: Vec<_> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                crate::Action::Copy { rel_path, .. } => Some(rel_path.clone()),
                _ => None,
            })
            .collect();
        assert!(paths.contains(&"keep.txt".to_string()), "got: {paths:?}");
        assert!(
            !paths.contains(&"scratch.tmp".to_string()),
            "got: {paths:?}"
        );
    }

    #[test]
    fn prepare_refuses_a_plan_over_the_deletion_threshold() {
        let f = fixture();
        for i in 0..10 {
            f.write("local", &format!("f{i}.txt"), "x");
            f.write("remote", &format!("f{i}.txt"), "x");
        }
        let session = f.session();
        let plan = prepared(&session, false);
        session.run(&plan, &Reporter::silent()).unwrap();

        // Simulate the share going away: delete everything on the remote.
        for i in 0..10 {
            std::fs::remove_file(f.profile.remote.join(format!("f{i}.txt"))).unwrap();
        }
        let err = session
            .prepare(false, Compare::Quick, &Reporter::silent())
            .unwrap_err();
        assert!(
            matches!(err, crate::Error::DeletionThreshold { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn force_overrides_the_deletion_threshold() {
        let f = fixture();
        for i in 0..10 {
            f.write("local", &format!("f{i}.txt"), "x");
            f.write("remote", &format!("f{i}.txt"), "x");
        }
        let session = f.session();
        let plan = prepared(&session, false);
        session.run(&plan, &Reporter::silent()).unwrap();
        for i in 0..10 {
            std::fs::remove_file(f.profile.remote.join(format!("f{i}.txt"))).unwrap();
        }
        let plan = prepared(&session, true);
        assert_eq!(plan.trash_count(), 10);
    }

    #[test]
    fn final_review_choices_cannot_bypass_the_deletion_guard() {
        let f = fixture();
        for i in 0..4 {
            f.write("local", &format!("f{i}.txt"), "keep");
        }
        let session = f.session();
        session
            .run(&prepared(&session, false), &Reporter::silent())
            .unwrap();
        // All pairs were adopted without a hash, so these are edit/delete
        // conflicts. Resolving them as deletions must still respect the guard.
        for i in 0..2 {
            std::fs::remove_file(f.profile.remote.join(format!("f{i}.txt"))).unwrap();
            f.write("local", &format!("f{i}.txt"), "edited locally");
        }
        let mut plan = prepared(&session, false);
        for i in 0..2 {
            plan.set_choice(&format!("f{i}.txt"), crate::Choice::Trash(Side::Local))
                .unwrap();
        }
        assert!(matches!(
            session.run(&plan, &Reporter::silent()),
            Err(crate::Error::DeletionThreshold { .. })
        ));
        assert!(f.profile.local.join("f0.txt").exists());
        session
            .run_with_force(&plan, true, &Reporter::silent())
            .unwrap();
        assert!(!f.profile.local.join("f0.txt").exists());
    }

    #[test]
    fn a_mount_lost_during_review_aborts_before_writes_even_with_force() {
        let f = fixture();
        f.write("local", "a.txt", "keep");
        let session = f.session();
        let plan = prepared(&session, false);
        std::fs::remove_file(f.profile.remote.join(marker::MARKER_FILE)).unwrap();
        assert!(
            session
                .run_with_force(&plan, true, &Reporter::silent())
                .is_err()
        );
        assert!(!f.profile.remote.join("a.txt").exists());
        assert_eq!(session.tracked_count().unwrap(), 0);
    }

    #[test]
    fn run_stamps_the_last_sync_time() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        let session = f.session();
        assert_eq!(session.last_sync().unwrap(), None);
        let plan = prepared(&session, false);
        session.run(&plan, &Reporter::silent()).unwrap();
        assert!(session.last_sync().unwrap().is_some());
    }

    #[test]
    fn a_cancelled_run_does_not_stamp_the_last_sync_time() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        let session = f.session();
        let plan = prepared(&session, false);
        session
            .run(&plan, &Reporter::new(&|_| {}, &|| true))
            .unwrap();
        assert_eq!(
            session.last_sync().unwrap(),
            None,
            "an interrupted run has not synced"
        );
    }

    #[test]
    fn a_cancelled_scan_yields_no_plan() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        let sink = |_| {};
        let plan = f
            .session()
            .prepare(false, Compare::Quick, &Reporter::new(&sink, &|| true))
            .unwrap();
        assert!(plan.is_none());
    }

    #[test]
    fn a_cancelled_scan_leaves_both_roots_and_the_state_untouched() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        f.write("remote", "b.txt", "y");
        let session = f.session();

        let before = (tree(&f.profile.local), tree(&f.profile.remote));
        let sink = |_| {};
        session
            .prepare(false, Compare::Quick, &Reporter::new(&sink, &|| true))
            .unwrap();

        assert_eq!(
            (tree(&f.profile.local), tree(&f.profile.remote)),
            before,
            "a scan writes nothing, cancelled or not"
        );
        assert_eq!(session.tracked_count().unwrap(), 0);
        assert_eq!(session.last_sync().unwrap(), None);
    }

    #[test]
    fn concurrent_scans_still_report_one_phase_at_a_time() {
        let f = fixture();
        for i in 0..50 {
            f.write("local", &format!("l{i}.txt"), "l");
            f.write("remote", &format!("r{i}.txt"), "r");
        }
        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        f.session()
            .prepare(false, Compare::Quick, &Reporter::new(&sink, &|| false))
            .unwrap()
            .unwrap();

        let phases: Vec<Phase> = events
            .into_inner()
            .into_iter()
            .map(|e| match e {
                Progress::Started { phase, .. }
                | Progress::Item { phase, .. }
                | Progress::Bytes { phase, .. }
                | Progress::Finished { phase, .. } => phase,
            })
            .collect();
        let mut runs = phases.clone();
        runs.dedup();
        assert_eq!(
            runs,
            vec![Phase::ScanLocal, Phase::ScanRemote, Phase::Compare],
            "phases must not interleave"
        );
        assert_eq!(
            phases.iter().filter(|p| **p == Phase::ScanRemote).count(),
            52,
            "every remote file reported, plus Started and Finished"
        );
    }

    #[test]
    fn a_remote_tree_larger_than_the_backlog_still_finishes() {
        let f = fixture();
        f.write("local", "only.txt", "l");
        let n = REMOTE_BACKLOG * 3;
        for i in 0..n {
            f.write("remote", &format!("r{i}.txt"), "r");
        }
        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        let plan = f
            .session()
            .prepare(false, Compare::Quick, &Reporter::new(&sink, &|| false))
            .unwrap()
            .unwrap();
        assert_eq!(plan.copy_count(crate::Side::Local), n);

        let events = events.into_inner();
        assert!(events.contains(&Progress::Started {
            phase: Phase::ScanRemote,
            total: None
        }));
        assert!(
            events.contains(&Progress::Finished {
                phase: Phase::ScanRemote,
                count: n
            }),
            "the remote finish is never dropped"
        );
    }

    #[test]
    fn a_failing_remote_scan_fails_the_prepare() {
        let f = fixture();
        // The local ignore file also filters the remote scan, which cannot
        // read it when it is a directory and reports a configuration error.
        f.write("local", &format!("{IGNORE_FILE}/inner"), "x");
        f.write("local", "a.txt", "x");
        let err = f
            .session()
            .prepare(false, Compare::Quick, &Reporter::silent())
            .unwrap_err();
        assert!(matches!(err, crate::Error::Config(_)), "got: {err:?}");
    }

    /// Every file under `root`, as sorted (relative path, contents) pairs.
    fn tree(root: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path.strip_prefix(root).unwrap().display().to_string();
                    out.push((rel, std::fs::read_to_string(&path).unwrap_or_default()));
                }
            }
        }
        out.sort();
        out
    }
}
