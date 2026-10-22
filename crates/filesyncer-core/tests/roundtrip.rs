use filesyncer_core::{Choice, Profile, Reporter, Session, Side, create_profile_roots};
use std::path::{Path, PathBuf};

struct World {
    dir: tempfile::TempDir,
    profile: Profile,
}

fn world() -> World {
    let dir = tempfile::tempdir().unwrap();
    let local = dir.path().join("local");
    let remote = dir.path().join("remote");
    std::fs::create_dir_all(&local).unwrap();
    std::fs::create_dir_all(&remote).unwrap();
    let profile = Profile {
        id: "roundtrip".into(),
        name: "Round Trip".into(),
        local,
        remote,
    };
    create_profile_roots(&profile).unwrap();
    World { dir, profile }
}

impl World {
    fn session(&self) -> Session {
        Session::open_with_state_path(&self.profile, &self.dir.path().join("state.db")).unwrap()
    }
    fn local(&self, rel: &str) -> PathBuf {
        self.profile.local.join(rel)
    }
    fn remote(&self, rel: &str) -> PathBuf {
        self.profile.remote.join(rel)
    }
    fn write(&self, path: PathBuf, contents: &str) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, contents).unwrap();
    }
    /// Scan, apply, and return the summary.
    fn sync(&self) -> filesyncer_core::RunSummary {
        let s = self.session();
        let plan = prepared(&s, false);
        s.run(&plan, &Reporter::silent()).unwrap()
    }
}

/// Prepare with no progress reporting.
fn prepared(session: &Session, force: bool) -> filesyncer_core::Plan {
    session
        .prepare(force, filesyncer_core::Compare::Quick, &Reporter::silent())
        .unwrap()
        .expect("a silent reporter never cancels")
}

fn assert_trees_match(a: &Path, b: &Path) {
    let list = |root: &Path| -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            for entry in std::fs::read_dir(&dir).unwrap().flatten() {
                let path = entry.path();
                let name = entry.file_name().to_string_lossy().to_string();
                if name.starts_with(".filesyncer") {
                    continue;
                }
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    out.push((rel, std::fs::read_to_string(&path).unwrap()));
                }
            }
        }
        out.sort();
        out
    };
    assert_eq!(list(a), list(b));
}

#[test]
fn a_populated_local_tree_propagates_to_an_empty_remote() {
    let w = world();
    w.write(w.local("a.txt"), "alpha");
    w.write(w.local("docs/b.md"), "beta");
    w.write(w.local("docs/deep/c.txt"), "gamma");

    let summary = w.sync();

    assert_eq!(summary.copied, 3);
    assert!(summary.errors.is_empty(), "got: {:?}", summary.errors);
    assert_trees_match(&w.profile.local, &w.profile.remote);
}

#[test]
fn a_second_run_immediately_after_the_first_plans_nothing() {
    let w = world();
    w.write(w.local("a.txt"), "alpha");
    w.write(w.remote("b.txt"), "beta");
    w.sync();

    let plan = prepared(&w.session(), false);
    assert!(
        plan.actions.is_empty(),
        "sync must be idempotent; got: {:?}",
        plan.actions
    );
    assert!(plan.conflicts.is_empty());
}

#[test]
fn mtime_drift_on_the_remote_causes_no_transfer() {
    let w = world();
    w.write(w.local("a.txt"), "alpha");
    w.sync();

    // Simulate a remount shifting mtimes without changing content.
    let shifted = filetime::FileTime::from_unix_time(1_500_000_000, 0);
    filetime::set_file_mtime(w.remote("a.txt"), shifted).unwrap();

    let s = w.session();
    let plan = prepared(&s, false);
    assert!(
        plan.is_noop(),
        "drift must not transfer; got: {:?}",
        plan.actions
    );

    s.run(&plan, &Reporter::silent()).unwrap();
    let plan = prepared(&s, false);
    assert!(plan.actions.is_empty(), "state should now be settled");
}

#[test]
fn an_edit_on_each_side_propagates_both_ways() {
    let w = world();
    w.write(w.local("a.txt"), "alpha");
    w.write(w.local("b.txt"), "beta");
    w.sync();

    w.write(w.local("a.txt"), "alpha edited locally");
    w.write(w.remote("b.txt"), "beta edited remotely");
    w.sync();

    assert_eq!(
        std::fs::read_to_string(w.remote("a.txt")).unwrap(),
        "alpha edited locally"
    );
    assert_eq!(
        std::fs::read_to_string(w.local("b.txt")).unwrap(),
        "beta edited remotely"
    );
    assert_trees_match(&w.profile.local, &w.profile.remote);
}

#[test]
fn a_deletion_propagates_and_lands_in_the_trash() {
    let w = world();
    for i in 0..10 {
        w.write(w.local(&format!("f{i}.txt")), "x");
    }
    w.sync();

    std::fs::remove_file(w.local("f0.txt")).unwrap();
    let summary = w.sync();

    assert_eq!(summary.trashed, 1);
    assert!(!w.remote("f0.txt").exists());
    let trash = w.profile.remote.join(".filesyncer-trash");
    let found = std::fs::read_dir(&trash)
        .unwrap()
        .flatten()
        .any(|run| run.path().join("f0.txt").exists());
    assert!(found, "deleted file should be recoverable from {trash:?}");
}

#[test]
fn concurrent_edits_are_flagged_and_neither_side_is_touched() {
    let w = world();
    w.write(w.local("a.txt"), "original");
    w.sync();

    w.write(w.local("a.txt"), "local version");
    w.write(w.remote("a.txt"), "remote version");

    let s = w.session();
    let plan = prepared(&s, false);
    assert_eq!(plan.conflicts.len(), 1);
    s.run(&plan, &Reporter::silent()).unwrap();

    assert_eq!(
        std::fs::read_to_string(w.local("a.txt")).unwrap(),
        "local version"
    );
    assert_eq!(
        std::fs::read_to_string(w.remote("a.txt")).unwrap(),
        "remote version"
    );
}

#[test]
fn an_unresolved_conflict_reappears_on_every_run() {
    let w = world();
    w.write(w.local("a.txt"), "original");
    w.sync();
    w.write(w.local("a.txt"), "local version");
    w.write(w.remote("a.txt"), "remote version");
    w.sync();

    let plan = prepared(&w.session(), false);
    assert_eq!(
        plan.conflicts.len(),
        1,
        "the conflict must not be forgotten"
    );
}

#[test]
fn resolving_a_conflict_converges_the_trees() {
    let w = world();
    w.write(w.local("a.txt"), "original");
    w.sync();
    w.write(w.local("a.txt"), "local version");
    w.write(w.remote("a.txt"), "remote version");

    let s = w.session();
    let mut plan = prepared(&s, false);
    plan.resolve_conflict("a.txt", Side::Local).unwrap();
    s.run(&plan, &Reporter::silent()).unwrap();

    assert_eq!(
        std::fs::read_to_string(w.remote("a.txt")).unwrap(),
        "local version"
    );
    let plan = prepared(&s, false);
    assert!(
        plan.actions.is_empty(),
        "resolution should settle the state"
    );
    assert!(plan.conflicts.is_empty());
}

#[test]
fn an_unmounted_remote_aborts_rather_than_deleting_everything() {
    let w = world();
    for i in 0..10 {
        w.write(w.local(&format!("f{i}.txt")), "x");
    }
    w.sync();

    // An unmounted mount point is an empty directory: no files, no marker.
    std::fs::remove_dir_all(&w.profile.remote).unwrap();
    std::fs::create_dir(&w.profile.remote).unwrap();

    let err =
        Session::open_with_state_path(&w.profile, &w.dir.path().join("state.db")).unwrap_err();
    assert!(err.to_string().contains("missing"), "got: {err}");

    for i in 0..10 {
        assert!(
            w.local(&format!("f{i}.txt")).exists(),
            "local tree must be intact"
        );
    }
}

#[test]
fn deleting_the_state_database_re_baselines_without_data_loss() {
    let w = world();
    w.write(w.local("same.txt"), "identical");
    w.write(w.local("only-local.txt"), "l");
    w.sync();
    w.write(w.remote("differs.txt"), "remote side");
    w.write(w.local("differs.txt"), "local side");

    std::fs::remove_file(w.dir.path().join("state.db")).unwrap();

    let plan = prepared(&w.session(), false);
    assert!(plan.trash_count() == 0, "a re-baseline must never delete");
    assert_eq!(plan.conflicts.len(), 1, "only the genuinely differing file");
    assert_eq!(plan.conflicts[0].rel_path, "differs.txt");
}

#[test]
fn a_syncignored_file_is_never_transferred() {
    let w = world();
    w.write(w.local(".syncignore"), "*.tmp\nbuild/\n");
    w.write(w.local("keep.txt"), "k");
    w.write(w.local("scratch.tmp"), "s");
    w.write(w.local("build/out.bin"), "o");
    w.sync();

    assert!(w.remote("keep.txt").exists());
    assert!(!w.remote("scratch.tmp").exists());
    assert!(!w.remote("build/out.bin").exists());
}

#[test]
fn symlinks_are_skipped_and_reported_rather_than_synced() {
    let w = world();
    w.write(w.local("real.txt"), "r");
    std::os::unix::fs::symlink("real.txt", w.local("link.txt")).unwrap();

    let plan = prepared(&w.session(), false);
    assert!(plan.skips.iter().any(|s| s.rel_path == "link.txt"));
    w.sync();
    assert!(!w.remote("link.txt").exists());
    assert!(w.remote("real.txt").exists());
}

#[test]
fn a_thousand_files_converge_and_then_settle() {
    let w = world();
    for i in 0..1000 {
        w.write(
            w.local(&format!("dir{}/f{i}.txt", i % 20)),
            &format!("contents {i}"),
        );
    }
    let summary = w.sync();
    assert_eq!(summary.copied, 1000);
    assert!(summary.errors.is_empty());
    assert_trees_match(&w.profile.local, &w.profile.remote);

    let plan = prepared(&w.session(), false);
    assert!(
        plan.actions.is_empty(),
        "large tree must also be idempotent"
    );
}

#[test]
fn verification_rechecks_a_pair_previously_adopted_by_quick_sync() {
    let w = world();
    w.write(w.local("a.txt"), "alpha");
    w.write(w.remote("a.txt"), "bravo");
    let stamp = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    filetime::set_file_mtime(w.local("a.txt"), stamp).unwrap();
    filetime::set_file_mtime(w.remote("a.txt"), stamp).unwrap();
    let session = w.session();
    let quick = prepared(&session, false);
    assert!(quick.conflicts.is_empty());
    assert_eq!(quick.actions.len(), 1);
    session.run(&quick, &Reporter::silent()).unwrap();
    drop(session);

    let plan = w
        .session()
        .prepare(false, filesyncer_core::Compare::Verify, &Reporter::silent())
        .unwrap()
        .unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    assert!(plan.actions.is_empty());
    assert_eq!(std::fs::read_to_string(w.local("a.txt")).unwrap(), "alpha");
    assert_eq!(std::fs::read_to_string(w.remote("a.txt")).unwrap(), "bravo");
}

#[test]
fn trashing_a_new_file_instead_of_copying_it_settles() {
    let w = world();
    w.write(w.local("junk.txt"), "unwanted");

    let s = w.session();
    let mut plan = prepared(&s, false);
    plan.set_choice("junk.txt", Choice::Trash(Side::Local))
        .unwrap();
    s.run(&plan, &Reporter::silent()).unwrap();

    assert!(!w.local("junk.txt").exists());
    assert!(!w.remote("junk.txt").exists());
    assert!(prepared(&s, false).is_noop());
}

#[test]
fn reversing_a_copy_restores_the_deleted_file() {
    let w = world();
    w.write(w.local("a.txt"), "keep me");
    // Enough other files that one deletion stays under the safety threshold.
    for i in 0..9 {
        w.write(w.local(&format!("other{i}.txt")), "x");
    }
    w.sync();
    std::fs::remove_file(w.remote("a.txt")).unwrap();

    // The plan propagates the deletion; choosing to copy undoes it.
    let s = w.session();
    let mut plan = prepared(&s, false);
    assert_eq!(plan.choice("a.txt"), Choice::Trash(Side::Local));
    plan.set_choice("a.txt", Choice::Copy(Side::Local)).unwrap();
    s.run(&plan, &Reporter::silent()).unwrap();

    assert_eq!(
        std::fs::read_to_string(w.remote("a.txt")).unwrap(),
        "keep me"
    );
    assert!(prepared(&s, false).is_noop());
}

#[test]
fn a_skipped_copy_is_planned_again() {
    let w = world();
    w.write(w.local("later.txt"), "not yet");

    let s = w.session();
    let mut plan = prepared(&s, false);
    plan.set_choice("later.txt", Choice::Skip).unwrap();
    s.run(&plan, &Reporter::silent()).unwrap();

    assert!(!w.remote("later.txt").exists());
    assert_eq!(
        prepared(&s, false).choice("later.txt"),
        Choice::Copy(Side::Local)
    );
}
