use crate::apply::{ApplyCtx, Progress, RunSummary, apply, run_stamp_now};
use crate::marker;
use crate::plan::Plan;
use crate::profile::Profile;
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

    pub fn prepare(&self, force: bool) -> Result<Plan> {
        marker::verify_marker(&self.profile.local, &self.profile.id)?;
        marker::verify_marker(&self.profile.remote, &self.profile.id)?;

        let local_ignore = self.profile.local.join(IGNORE_FILE);
        let remote_ignore = self.profile.remote.join(IGNORE_FILE);

        let local = scan_root(&self.profile.local, std::slice::from_ref(&remote_ignore))?;
        let remote = scan_root(&self.profile.remote, std::slice::from_ref(&local_ignore))?;

        let state = self.state.load_all()?;
        let hashes = FsHashProvider {
            local_root: self.profile.local.clone(),
            remote_root: self.profile.remote.clone(),
        };
        let plan = reconcile(&local, &remote, &state, &hashes)?;

        if !force {
            plan.check_deletion_threshold(state.len())?;
        }
        Ok(plan)
    }

    pub fn run(
        &self,
        plan: &Plan,
        progress: &dyn Fn(Progress),
        cancel: &dyn Fn() -> bool,
    ) -> Result<RunSummary> {
        let ctx = ApplyCtx {
            local_root: &self.profile.local,
            remote_root: &self.profile.remote,
            state: &self.state,
            run_stamp: run_stamp_now(),
        };
        let summary = apply(plan, &ctx, progress, cancel)?;

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
        let plan = f.session().prepare(false).unwrap();
        assert_eq!(plan.copy_count(crate::Side::Remote), 1);
        assert_eq!(plan.copy_count(crate::Side::Local), 1);
    }

    #[test]
    fn markers_are_never_part_of_a_plan() {
        let f = fixture();
        f.write("local", "a.txt", "aaa");
        let plan = f.session().prepare(false).unwrap();
        assert_eq!(plan.actions.len(), 1, "got: {:?}", plan.actions);
    }

    #[test]
    fn syncignore_from_either_root_filters_both_sides() {
        let f = fixture();
        f.write("remote", IGNORE_FILE, "*.tmp\n");
        f.write("local", "keep.txt", "k");
        f.write("local", "scratch.tmp", "s");
        let plan = f.session().prepare(false).unwrap();
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
        let plan = session.prepare(false).unwrap();
        session.run(&plan, &|_| {}, &|| false).unwrap();

        // Simulate the share going away: delete everything on the remote.
        for i in 0..10 {
            std::fs::remove_file(f.profile.remote.join(format!("f{i}.txt"))).unwrap();
        }
        let err = session.prepare(false).unwrap_err();
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
        let plan = session.prepare(false).unwrap();
        session.run(&plan, &|_| {}, &|| false).unwrap();
        for i in 0..10 {
            std::fs::remove_file(f.profile.remote.join(format!("f{i}.txt"))).unwrap();
        }
        let plan = session.prepare(true).unwrap();
        assert_eq!(plan.trash_count(), 10);
    }

    #[test]
    fn run_stamps_the_last_sync_time() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        let session = f.session();
        assert_eq!(session.last_sync().unwrap(), None);
        let plan = session.prepare(false).unwrap();
        session.run(&plan, &|_| {}, &|| false).unwrap();
        assert!(session.last_sync().unwrap().is_some());
    }

    #[test]
    fn a_cancelled_run_does_not_stamp_the_last_sync_time() {
        let f = fixture();
        f.write("local", "a.txt", "x");
        let session = f.session();
        let plan = session.prepare(false).unwrap();
        session.run(&plan, &|_| {}, &|| true).unwrap();
        assert_eq!(
            session.last_sync().unwrap(),
            None,
            "an interrupted run has not synced"
        );
    }
}
