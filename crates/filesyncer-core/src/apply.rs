use crate::plan::{Action, Plan, Side, Stat};
use crate::progress::{Phase, Reporter};
use crate::scan::TRASH_DIR;
use crate::state::{StateRow, StateStore};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub const TEMP_SUFFIX: &str = ".filesyncer-tmp";

/// Read size for copies. Large enough that a network share is asked for few,
/// big reads, and that BLAKE3 can hash many chunks per call.
const COPY_BUF: usize = 256 * 1024;

/// Committing state every action is the safest shape but costs a write per
/// file; batching bounds what a crash can lose to this many rows, all of which
/// the next scan rebuilds by hashing (see `apply`).
const COMMIT_EVERY: usize = 500;

/// The least time between byte counts sent during a copy.
const BYTES_INTERVAL: std::time::Duration = std::time::Duration::from_millis(100);

/// The longest a batch of finished rows may wait to be committed.
const COMMIT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(2);

/// Copies at least this big may take long enough that the rows before them
/// are committed first, rather than left waiting out the transfer.
const LARGE_COPY: u64 = 64 * 1024 * 1024;

thread_local! {
    /// One copy buffer per thread, reused across files.
    static COPY_BUFFER: std::cell::RefCell<Vec<u8>> = std::cell::RefCell::new(vec![0; COPY_BUF]);
}

/// What `copy_atomic` read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Copied {
    /// BLAKE3 of the bytes copied.
    pub hash: String,
    /// The source as it was when opened, before any byte was read. If the
    /// source changes during the copy, its next stat will differ from this,
    /// so the next scan sees the change instead of trusting `hash`.
    pub source: Stat,
}

fn temp_path_for(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!("{name}{TEMP_SUFFIX}-{}", std::process::id()))
}

/// Copy `src` over `dst` such that `dst` is never observed partially written.
///
/// The hash of the bytes copied is computed in the same pass, so the source is
/// read once.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<Copied> {
    copy_atomic_reporting(src, dst, &mut |_| {})
}

/// `copy_atomic`, calling `copied` with the size of each chunk as it is
/// written.
fn copy_atomic_reporting(src: &Path, dst: &Path, copied: &mut dyn FnMut(u64)) -> Result<Copied> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = temp_path_for(dst);

    // Scoped so both handles are closed before the rename.
    let (hash, meta) = {
        let mut reader = std::fs::File::open(src).map_err(|e| Error::io(src, e))?;
        let meta = reader.metadata().map_err(|e| Error::io(src, e))?;
        let mut writer = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        let hash = match copy_hashing(&mut reader, &mut writer, copied) {
            Ok(hash) => hash,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e.into_error(src, &tmp));
            }
        };
        if let Err(e) = writer.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::io(&tmp, e));
        }
        (hash, meta)
    };

    std::fs::rename(&tmp, dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(dst, e)
    })?;

    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    filetime::set_file_mtime(dst, mtime).map_err(|e| Error::io(dst, e))?;
    Ok(Copied {
        hash,
        source: stat_of_meta(&meta),
    })
}

/// Which end of a copy failed, so the error names the right path.
enum CopyError {
    Read(std::io::Error),
    Write(std::io::Error),
}

impl CopyError {
    fn into_error(self, src: &Path, dst: &Path) -> Error {
        match self {
            CopyError::Read(e) => Error::io(src, e),
            CopyError::Write(e) => Error::io(dst, e),
        }
    }
}

fn copy_hashing(
    reader: &mut impl std::io::Read,
    writer: &mut impl std::io::Write,
    copied: &mut dyn FnMut(u64),
) -> std::result::Result<String, CopyError> {
    COPY_BUFFER.with_borrow_mut(|buf| {
        let mut hasher = blake3::Hasher::new();
        loop {
            let n = match reader.read(buf) {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(CopyError::Read(e)),
            };
            hasher.update(&buf[..n]);
            writer.write_all(&buf[..n]).map_err(CopyError::Write)?;
            copied(n as u64);
        }
        Ok(crate::hash::to_hex(hasher))
    })
}

/// Move `root/rel_path` into `root/.filesyncer-trash/<run_stamp>/<rel_path>`.
pub fn move_to_trash(root: &Path, rel_path: &str, run_stamp: &str) -> Result<()> {
    let source = root.join(rel_path);
    if !source.exists() {
        return Ok(());
    }
    let dest = root.join(TRASH_DIR).join(run_stamp).join(rel_path);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }

    match std::fs::rename(&source, &dest) {
        Ok(()) => Ok(()),
        // Rename fails across filesystems; fall back to copy-then-delete.
        Err(_) => {
            copy_atomic(&source, &dest)?;
            std::fs::remove_file(&source).map_err(|e| Error::io(&source, e))
        }
    }
}

/// Remove leftover temp files from an interrupted run. Returns how many.
pub fn clean_stale_temps(root: &Path) -> Result<usize> {
    let mut removed = 0;
    let mut stack = vec![root.to_path_buf()];

    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy().contains(TEMP_SUFFIX)
                && std::fs::remove_file(&path).is_ok()
            {
                removed += 1;
            }
        }
    }
    Ok(removed)
}

pub struct ApplyCtx<'a> {
    pub local_root: &'a Path,
    pub remote_root: &'a Path,
    pub state: &'a StateStore,
    pub run_stamp: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub copied: usize,
    pub trashed: usize,
    pub conflicts: usize,
    pub errors: Vec<(String, String)>,
    pub cancelled: bool,
    /// Copies that finished into the local side.
    pub to_local: Transfer,
    /// Copies that finished into the remote side.
    pub to_remote: Transfer,
}

/// Bytes copied in one direction and the time spent copying them, from which
/// the next plan's duration is estimated.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Transfer {
    pub bytes: u64,
    pub time: std::time::Duration,
}

impl RunSummary {
    pub fn transfer(&self, to: Side) -> Transfer {
        match to {
            Side::Local => self.to_local,
            Side::Remote => self.to_remote,
        }
    }

    fn transfer_mut(&mut self, to: Side) -> &mut Transfer {
        match to {
            Side::Local => &mut self.to_local,
            Side::Remote => &mut self.to_remote,
        }
    }
}

impl ApplyCtx<'_> {
    fn root(&self, side: Side) -> &Path {
        match side {
            Side::Local => self.local_root,
            Side::Remote => self.remote_root,
        }
    }
}

fn stat_of_file(path: &Path) -> Result<Stat> {
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    Ok(stat_of_meta(&meta))
}

fn stat_of_meta(meta: &std::fs::Metadata) -> Stat {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Stat {
        size: meta.len(),
        mtime,
    }
}

enum Outcome {
    Copied,
    Trashed,
    Bookkeeping,
}

pub fn apply(plan: &Plan, ctx: &ApplyCtx, reporter: &Reporter) -> Result<RunSummary> {
    let mut summary = RunSummary {
        conflicts: plan.conflicts.len(),
        ..Default::default()
    };

    clean_stale_temps(ctx.local_root)?;
    clean_stale_temps(ctx.remote_root)?;

    let total = plan.actions.len();
    reporter.started(Phase::Apply, Some(total));

    let mut done = 0usize;

    // Bytes are counted from the plan's sizes once a copy ends, whether it
    // succeeded or not, so the count always reaches the total.
    let total_bytes = plan.copy_bytes(Side::Local) + plan.copy_bytes(Side::Remote);
    let mut bytes_done = 0u64;
    let mut bytes_sent = std::time::Instant::now();
    reporter.bytes(Phase::Apply, 0, total_bytes);

    // State rows are written in batches, not one transaction per action. A
    // crash can therefore lose the rows of up to `COMMIT_EVERY` finished
    // actions, but never in a way that loses data: a copied file without a row
    // is new on both sides with equal content, so the next scan adopts it; a
    // trashed file without its row removed is gone from both sides and the row
    // is dropped. A clean finish or a cancel commits all; an error rolls the
    // open batch back as the transaction drops.
    let mut batch = ctx.state.begin()?;
    let mut pending = 0usize;
    let mut opened = std::time::Instant::now();

    for (index, action) in plan.actions.iter().enumerate() {
        let large_copy = matches!(action, Action::Copy { size, .. } if *size >= LARGE_COPY);
        if pending > 0
            && (pending >= COMMIT_EVERY || opened.elapsed() > COMMIT_INTERVAL || large_copy)
        {
            batch.commit()?;
            batch = ctx.state.begin()?;
            pending = 0;
            opened = std::time::Instant::now();
        }

        if reporter.cancelled() {
            summary.cancelled = true;
            break;
        }

        let rel_path = match action {
            Action::Copy { rel_path, .. }
            | Action::Trash { rel_path, .. }
            | Action::RecordState { rel_path, .. }
            | Action::DropState { rel_path } => rel_path.clone(),
        };

        reporter.item(Phase::Apply, index, &rel_path);
        done = index + 1;
        pending += 1;

        let outcome = match action {
            Action::Copy {
                rel_path,
                from,
                size,
            } => {
                let base = bytes_done;
                let mut in_file = 0u64;
                let started = std::time::Instant::now();
                let result = apply_copy(ctx, rel_path, *from, &mut |n| {
                    in_file += n;
                    if bytes_sent.elapsed() >= BYTES_INTERVAL {
                        bytes_sent = std::time::Instant::now();
                        reporter.bytes(Phase::Apply, base + in_file.min(*size), total_bytes);
                    }
                });
                bytes_done = base + size;
                reporter.bytes(Phase::Apply, bytes_done, total_bytes);
                if result.is_ok() {
                    let transfer = summary.transfer_mut(from.other());
                    transfer.bytes += size;
                    transfer.time += started.elapsed();
                }
                result.map(|()| Outcome::Copied)
            }
            Action::Trash { rel_path, side } => {
                move_to_trash(ctx.root(*side), rel_path, &ctx.run_stamp)
                    .and_then(|()| ctx.state.remove(rel_path))
                    .map(|()| Outcome::Trashed)
            }
            Action::RecordState {
                rel_path,
                hash,
                local,
                remote,
            } => ctx
                .state
                .upsert(&StateRow {
                    rel_path: rel_path.clone(),
                    hash: hash.clone(),
                    local_size: local.size,
                    local_mtime: local.mtime,
                    remote_size: remote.size,
                    remote_mtime: remote.mtime,
                })
                .map(|()| Outcome::Bookkeeping),
            Action::DropState { rel_path } => {
                ctx.state.remove(rel_path).map(|()| Outcome::Bookkeeping)
            }
        };

        match outcome {
            Ok(Outcome::Copied) => summary.copied += 1,
            Ok(Outcome::Trashed) => summary.trashed += 1,
            Ok(Outcome::Bookkeeping) => {}
            Err(e) => summary.errors.push((rel_path, e.to_string())),
        }
    }

    batch.commit()?;
    reporter.finished(Phase::Apply, done);
    Ok(summary)
}

fn apply_copy(
    ctx: &ApplyCtx,
    rel_path: &str,
    from: Side,
    copied: &mut dyn FnMut(u64),
) -> Result<()> {
    let src = ctx.root(from).join(rel_path);
    let dst = ctx.root(from.other()).join(rel_path);
    let Copied {
        hash,
        source: src_stat,
    } = copy_atomic_reporting(&src, &dst, copied)?;
    let dst_stat = stat_of_file(&dst)?;
    let (local, remote) = match from {
        Side::Local => (src_stat, dst_stat),
        Side::Remote => (dst_stat, src_stat),
    };

    ctx.state.upsert(&StateRow {
        rel_path: rel_path.to_string(),
        hash: Some(hash),
        local_size: local.size,
        local_mtime: local.mtime,
        remote_size: remote.size,
        remote_mtime: remote.mtime,
    })
}

/// A timestamp suitable for naming a trash subdirectory.
pub fn run_stamp_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let time = secs.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}{m:02}{d:02}-{:02}{:02}{:02}",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Howard Hinnant's days-from-civil, inverted. Avoids a date-time dependency
/// for the one place FileSyncer needs a calendar.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Progress;

    #[test]
    fn copy_creates_the_destination_with_identical_contents() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("out").join("dst.txt");
        std::fs::write(&src, "payload").unwrap();
        copy_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "payload");
    }

    #[test]
    fn copy_returns_the_hash_of_what_it_copied() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.bin");
        // Spans several buffers, with a ragged tail.
        let payload: Vec<u8> = (0..COPY_BUF * 3 + 17).map(|i| (i * 31) as u8).collect();
        std::fs::write(&src, &payload).unwrap();
        let hash = copy_atomic(&src, &dir.path().join("dst.bin")).unwrap().hash;
        assert_eq!(hash, crate::hash::hash_file(&src).unwrap());
        assert_eq!(std::fs::read(dir.path().join("dst.bin")).unwrap(), payload);
    }

    #[test]
    fn copy_overwrites_an_existing_destination() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, "new").unwrap();
        std::fs::write(&dst, "old and much longer").unwrap();
        copy_atomic(&src, &dst).unwrap();
        assert_eq!(std::fs::read_to_string(&dst).unwrap(), "new");
    }

    #[test]
    fn copy_preserves_the_source_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        let dst = dir.path().join("dst.txt");
        std::fs::write(&src, "payload").unwrap();
        let want = filetime::FileTime::from_unix_time(1_600_000_000, 0);
        filetime::set_file_mtime(&src, want).unwrap();
        copy_atomic(&src, &dst).unwrap();
        let got =
            filetime::FileTime::from_last_modification_time(&std::fs::metadata(&dst).unwrap());
        assert_eq!(got.unix_seconds(), 1_600_000_000);
    }

    #[test]
    fn copy_leaves_no_temp_file_behind() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src.txt");
        std::fs::write(&src, "payload").unwrap();
        copy_atomic(&src, &dir.path().join("dst.txt")).unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(TEMP_SUFFIX))
            .collect();
        assert!(leftovers.is_empty(), "got: {leftovers:?}");
    }

    #[test]
    fn copy_from_a_missing_source_reports_the_source_path() {
        let dir = tempfile::tempdir().unwrap();
        let err =
            copy_atomic(&dir.path().join("absent.txt"), &dir.path().join("dst.txt")).unwrap_err();
        assert!(err.to_string().contains("absent.txt"), "got: {err}");
    }

    #[test]
    fn trash_preserves_the_relative_directory_structure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("sub").join("deep").join("a.txt");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&target, "bye").unwrap();

        move_to_trash(dir.path(), "sub/deep/a.txt", "20260726-143000").unwrap();

        assert!(!target.exists(), "original should be gone");
        let trashed = dir
            .path()
            .join(TRASH_DIR)
            .join("20260726-143000")
            .join("sub/deep/a.txt");
        assert_eq!(std::fs::read_to_string(trashed).unwrap(), "bye");
    }

    #[test]
    fn trashing_twice_in_one_run_keeps_both_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "one").unwrap();
        move_to_trash(dir.path(), "a.txt", "run1").unwrap();
        std::fs::write(dir.path().join("a.txt"), "two").unwrap();
        move_to_trash(dir.path(), "a.txt", "run2").unwrap();
        let base = dir.path().join(TRASH_DIR);
        assert_eq!(
            std::fs::read_to_string(base.join("run1/a.txt")).unwrap(),
            "one"
        );
        assert_eq!(
            std::fs::read_to_string(base.join("run2/a.txt")).unwrap(),
            "two"
        );
    }

    #[test]
    fn trashing_an_absent_file_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        move_to_trash(dir.path(), "never-existed.txt", "run1").unwrap();
    }

    #[test]
    fn stale_temps_are_cleaned_recursively() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join(format!("a.txt{TEMP_SUFFIX}-1234")), "x").unwrap();
        std::fs::write(
            dir.path().join("sub").join(format!("b.txt{TEMP_SUFFIX}-9")),
            "y",
        )
        .unwrap();
        std::fs::write(dir.path().join("keep.txt"), "z").unwrap();

        assert_eq!(clean_stale_temps(dir.path()).unwrap(), 2);
        assert!(dir.path().join("keep.txt").exists());
    }

    use crate::plan::Stat;

    struct Fixture {
        _dir: tempfile::TempDir,
        local: PathBuf,
        remote: PathBuf,
        state: StateStore,
    }

    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let local = dir.path().join("local");
        let remote = dir.path().join("remote");
        std::fs::create_dir_all(&local).unwrap();
        std::fs::create_dir_all(&remote).unwrap();
        let state = StateStore::open_in_memory().unwrap();
        Fixture {
            _dir: dir,
            local,
            remote,
            state,
        }
    }

    impl Fixture {
        fn ctx(&self) -> ApplyCtx<'_> {
            ApplyCtx {
                local_root: &self.local,
                remote_root: &self.remote,
                state: &self.state,
                run_stamp: "run1".to_string(),
            }
        }
        fn write_local(&self, rel: &str, contents: &str) {
            let p = self.local.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
        fn write_remote(&self, rel: &str, contents: &str) {
            let p = self.remote.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, contents).unwrap();
        }
    }

    fn run(plan: &Plan, f: &Fixture) -> RunSummary {
        apply(plan, &f.ctx(), &Reporter::silent()).unwrap()
    }

    #[test]
    fn copy_to_remote_transfers_the_file_and_records_state() {
        let f = fixture();
        f.write_local("a.txt", "payload");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 7,
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);

        assert_eq!(summary.copied, 1);
        assert_eq!(
            std::fs::read_to_string(f.remote.join("a.txt")).unwrap(),
            "payload"
        );
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(
            row.hash,
            Some(crate::hash::hash_file(&f.local.join("a.txt")).unwrap())
        );
        assert_eq!(row.local_size, 7);
        assert_eq!(row.remote_size, 7);
    }

    #[test]
    fn copy_to_local_works_in_the_other_direction() {
        let f = fixture();
        f.write_remote("a.txt", "from remote");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Remote,
                size: 11,
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert_eq!(
            std::fs::read_to_string(f.local.join("a.txt")).unwrap(),
            "from remote"
        );
    }

    #[test]
    fn nested_paths_get_their_directories_created() {
        let f = fixture();
        f.write_local("deep/nested/a.txt", "x");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "deep/nested/a.txt".into(),
                from: Side::Local,
                size: 1,
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert!(f.remote.join("deep/nested/a.txt").exists());
    }

    #[test]
    fn the_recorded_hash_is_of_the_bytes_copied() {
        let f = fixture();
        f.write_local("a.txt", "payload");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 7,
            }],
            ..Default::default()
        };
        run(&plan, &f);
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(
            row.hash,
            Some(crate::hash::hash_file(&f.local.join("a.txt")).unwrap())
        );
        assert!(row.hash.as_ref().is_some_and(|h| !h.is_empty()));
    }

    #[test]
    fn trash_moves_the_file_and_drops_the_state_row() {
        let f = fixture();
        f.write_local("a.txt", "bye");
        f.state
            .upsert(&StateRow {
                rel_path: "a.txt".into(),
                hash: Some("h".into()),
                local_size: 3,
                local_mtime: 1,
                remote_size: 3,
                remote_mtime: 1,
            })
            .unwrap();

        let plan = Plan {
            actions: vec![Action::Trash {
                rel_path: "a.txt".into(),
                side: Side::Local,
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);

        assert_eq!(summary.trashed, 1);
        assert!(!f.local.join("a.txt").exists());
        assert_eq!(
            std::fs::read_to_string(f.local.join(TRASH_DIR).join("run1/a.txt")).unwrap(),
            "bye"
        );
        assert!(f.state.load_all().unwrap().is_empty());
    }

    #[test]
    fn record_state_writes_a_row_without_touching_files() {
        let f = fixture();
        let plan = Plan {
            actions: vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: Some("h9".into()),
                local: Stat {
                    size: 20,
                    mtime: 900,
                },
                remote: Stat {
                    size: 20,
                    mtime: 901,
                },
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);
        assert_eq!(summary.copied, 0);
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(row.hash.as_deref(), Some("h9"));
        assert_eq!(row.remote_mtime, 901);
    }

    #[test]
    fn drop_state_removes_the_row() {
        let f = fixture();
        f.state
            .upsert(&StateRow {
                rel_path: "a.txt".into(),
                hash: Some("h".into()),
                local_size: 1,
                local_mtime: 1,
                remote_size: 1,
                remote_mtime: 1,
            })
            .unwrap();
        let plan = Plan {
            actions: vec![Action::DropState {
                rel_path: "a.txt".into(),
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert!(f.state.load_all().unwrap().is_empty());
    }

    #[test]
    fn a_failing_file_is_recorded_but_the_run_continues() {
        let f = fixture();
        f.write_local("good.txt", "ok");
        // "missing.txt" is in the plan but absent on disk.
        let plan = Plan {
            actions: vec![
                Action::Copy {
                    rel_path: "missing.txt".into(),
                    from: Side::Local,
                    size: 1,
                },
                Action::Copy {
                    rel_path: "good.txt".into(),
                    from: Side::Local,
                    size: 2,
                },
            ],
            ..Default::default()
        };
        let summary = run(&plan, &f);

        assert_eq!(summary.copied, 1, "the good file still transferred");
        assert_eq!(summary.errors.len(), 1);
        assert_eq!(summary.errors[0].0, "missing.txt");
        assert!(f.remote.join("good.txt").exists());
    }

    #[test]
    fn a_failed_file_leaves_no_state_row_so_the_next_run_retries() {
        let f = fixture();
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "missing.txt".into(),
                from: Side::Local,
                size: 1,
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert!(f.state.load_all().unwrap().is_empty());
    }

    #[test]
    fn cancelling_stops_early_and_keeps_completed_work() {
        let f = fixture();
        f.write_local("a.txt", "one");
        f.write_local("b.txt", "two");
        let plan = Plan {
            actions: vec![
                Action::Copy {
                    rel_path: "a.txt".into(),
                    from: Side::Local,
                    size: 3,
                },
                Action::Copy {
                    rel_path: "b.txt".into(),
                    from: Side::Local,
                    size: 3,
                },
            ],
            ..Default::default()
        };
        let seen = std::cell::Cell::new(0usize);
        let sink = |_| {};
        let summary = apply(
            &plan,
            &f.ctx(),
            &Reporter::new(&sink, &|| {
                seen.set(seen.get() + 1);
                seen.get() > 1 // allow the first item, cancel before the second
            }),
        )
        .unwrap();

        assert!(summary.cancelled);
        assert_eq!(summary.copied, 1);
        assert!(f.remote.join("a.txt").exists());
        assert!(!f.remote.join("b.txt").exists());
        assert_eq!(
            f.state.load_all().unwrap().len(),
            1,
            "state must reflect exactly the work that finished"
        );
    }

    #[test]
    fn progress_brackets_the_run_and_reports_every_item() {
        let f = fixture();
        f.write_local("a.txt", "one");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 3,
            }],
            ..Default::default()
        };
        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        apply(&plan, &f.ctx(), &Reporter::new(&sink, &|| false)).unwrap();

        let events = events.into_inner();
        assert_eq!(
            events.first(),
            Some(&Progress::Started {
                phase: Phase::Apply,
                total: Some(1)
            }),
            "the work is counted up front, so the total is known"
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished {
                phase: Phase::Apply,
                count: 1
            })
        );
        assert!(events.iter().any(|e| matches!(
            e, Progress::Item { rel_path, .. } if rel_path == "a.txt"
        )));
    }

    #[test]
    fn bytes_count_up_to_the_total_including_a_failed_copy() {
        let f = fixture();
        f.write_local("a.txt", "one");
        let plan = Plan {
            actions: vec![
                Action::Copy {
                    rel_path: "a.txt".into(),
                    from: Side::Local,
                    size: 3,
                },
                Action::Copy {
                    rel_path: "missing.txt".into(),
                    from: Side::Remote,
                    size: 5,
                },
            ],
            ..Default::default()
        };
        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        let summary = apply(&plan, &f.ctx(), &Reporter::new(&sink, &|| false)).unwrap();

        let counts: Vec<(u64, u64)> = events
            .into_inner()
            .into_iter()
            .filter_map(|e| match e {
                Progress::Bytes { done, total, .. } => Some((done, total)),
                _ => None,
            })
            .collect();
        assert_eq!(counts.first(), Some(&(0, 8)));
        assert_eq!(counts.last(), Some(&(8, 8)));
        assert!(counts.windows(2).all(|w| w[0].0 <= w[1].0));

        // Only the copy that finished is measured.
        assert_eq!(summary.to_remote.bytes, 3);
        assert_eq!(summary.to_local, Transfer::default());
    }

    #[test]
    fn unresolved_conflicts_are_counted_and_never_acted_on() {
        let f = fixture();
        f.write_local("a.txt", "local");
        f.write_remote("a.txt", "remote");
        let plan = Plan {
            conflicts: vec![crate::plan::Conflict {
                rel_path: "a.txt".into(),
                kind: crate::plan::ConflictKind::BothEdited,
                local: Some(Stat { size: 5, mtime: 1 }),
                remote: Some(Stat { size: 6, mtime: 2 }),
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);

        assert_eq!(summary.conflicts, 1);
        assert_eq!(
            std::fs::read_to_string(f.local.join("a.txt")).unwrap(),
            "local"
        );
        assert_eq!(
            std::fs::read_to_string(f.remote.join("a.txt")).unwrap(),
            "remote"
        );
        assert!(
            f.state.load_all().unwrap().is_empty(),
            "conflict rows stay untouched"
        );
    }

    #[test]
    fn run_stamp_is_sortable_and_filename_safe() {
        let s = run_stamp_now();
        assert_eq!(s.len(), 15, "expected YYYYMMDD-HHMMSS, got {s}");
        assert!(s.chars().all(|c| c.is_ascii_digit() || c == '-'), "got {s}");
    }
}
