use crate::apply::{ApplyCtx, RunSummary, apply, run_stamp_now};
use crate::marker;
use crate::plan::Plan;
use crate::profile::Profile;
use crate::progress::{Phase, Reporter};
use crate::reconcile::{FsHashProvider, reconcile};
use crate::scan::{IGNORE_FILE, scan_root};
use crate::state::StateStore;
use crate::{Result, profile};
use std::path::Path;

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
    pub fn prepare(&self, force: bool, reporter: &Reporter) -> Result<Option<Plan>> {
        marker::verify_marker(&self.profile.local, &self.profile.id)?;
        marker::verify_marker(&self.profile.remote, &self.profile.id)?;

        let local_ignore = self.profile.local.join(IGNORE_FILE);
        let remote_ignore = self.profile.remote.join(IGNORE_FILE);

        let Some(local) = scan_root(
            &self.profile.local,
            std::slice::from_ref(&remote_ignore),
            Phase::ScanLocal,
            reporter,
        )?
        else {
            return Ok(None);
        };
        let Some(remote) = scan_root(
            &self.profile.remote,
            std::slice::from_ref(&local_ignore),
            Phase::ScanRemote,
            reporter,
        )?
        else {
            return Ok(None);
        };

        let state = self.state.load_all()?;
        let hashes = FsHashProvider {
            local_root: self.profile.local.clone(),
            remote_root: self.profile.remote.clone(),
        };
        let Some(plan) = reconcile(&local, &remote, &state, &hashes, reporter)? else {
            return Ok(None);
        };

        if !force {
            plan.check_deletion_threshold(state.len())?;
        }
        Ok(Some(plan))
    }

    pub fn run(&self, plan: &Plan, reporter: &Reporter) -> Result<RunSummary> {
        let ctx = ApplyCtx {
            local_root: &self.profile.local,
            remote_root: &self.profile.remote,
            state: &self.state,
            run_stamp: run_stamp_now(),
        };
        let summary = apply(plan, &ctx, reporter)?;

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

    pub fn tracked_count(&self) -> Result<usize> {
        self.state.tracked_count()
    }
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
            .prepare(force, &Reporter::silent())
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
        let err = session.prepare(false, &Reporter::silent()).unwrap_err();
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
            .prepare(false, &Reporter::new(&sink, &|| true))
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
            .prepare(false, &Reporter::new(&sink, &|| true))
            .unwrap();

        assert_eq!(
            (tree(&f.profile.local), tree(&f.profile.remote)),
            before,
            "a scan writes nothing, cancelled or not"
        );
        assert_eq!(session.tracked_count().unwrap(), 0);
        assert_eq!(session.last_sync().unwrap(), None);
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
