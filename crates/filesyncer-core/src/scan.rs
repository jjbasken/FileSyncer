use crate::progress::{CANCEL_POLL, Phase, Reporter};
use crate::{Error, Result, marker::MARKER_FILE};
use ignore::WalkState;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

pub const TRASH_DIR: &str = ".filesyncer-trash";
pub const IGNORE_FILE: &str = ".syncignore";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub rel_path: String,
    pub kind: EntryKind,
    pub size: u64,
    pub mtime: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Scan {
    /// Regular files only, keyed by relative path.
    pub files: BTreeMap<String, Entry>,
    /// Relative paths of directories, used to detect file/directory collisions.
    pub dirs: BTreeSet<String>,
    /// Relative paths of symlinks, which are never synced.
    pub symlinks: Vec<String>,
    /// (relative path, message) for entries that could not be read.
    pub errors: Vec<(String, String)>,
}

/// Walk `root`, reporting under `phase`. The phase is a parameter because only
/// the caller knows whether a given root is the local or the remote side.
///
/// Returns `Ok(None)` when the walk was cancelled. A scan writes nothing, so a
/// cancelled one is an absent result rather than an error.
pub fn scan_root(
    root: &Path,
    extra_ignore_files: &[PathBuf],
    phase: Phase,
    reporter: &Reporter,
) -> Result<Option<Scan>> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .hidden(false)
        .parents(false)
        .ignore(false)
        .git_ignore(false)
        .git_global(false)
        .git_exclude(false)
        .follow_links(false)
        .require_git(false);
    builder.add_custom_ignore_filename(IGNORE_FILE);
    for extra in extra_ignore_files {
        if extra.exists()
            && let Some(err) = builder.add_ignore(extra)
        {
            return Err(Error::Config(format!("{}: {err}", extra.display())));
        }
    }

    let mut scan = Scan::default();

    // A walk cannot know how many files it will find, so the total is absent
    // and frontends show a count-up rather than a percentage.
    reporter.started(phase, None);
    if reporter.cancelled() {
        return Ok(None);
    }

    // The walk runs on a pool of threads, so that directory listings and
    // stats — each a round trip on a network share — overlap. This thread
    // owns the `Reporter`, which is not thread-safe, and assembles the scan.
    let stop = AtomicBool::new(false);
    let (tx, rx) = mpsc::channel::<Found>();
    let walker = builder.build_parallel();

    let cancelled = std::thread::scope(|scope| {
        scope.spawn(|| {
            walker.run(|| {
                let tx = tx.clone();
                let stop = &stop;
                Box::new(move |result| visit(root, result, stop, &tx))
            });
            // The walk is over once `run` returns; dropping the last sender
            // tells the loop below.
            drop(tx);
        });

        loop {
            match rx.recv_timeout(CANCEL_POLL) {
                Ok(Found::File(rel_path, entry)) => {
                    reporter.item(phase, scan.files.len(), &rel_path);
                    scan.files.insert(rel_path, entry);
                }
                Ok(Found::Dir(rel_path)) => {
                    scan.dirs.insert(rel_path);
                }
                Ok(Found::Symlink(rel_path)) => scan.symlinks.push(rel_path),
                Ok(Found::Error(rel_path, message)) => scan.errors.push((rel_path, message)),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => return false,
            }
            if reporter.cancelled() {
                stop.store(true, Ordering::Relaxed);
                return true;
            }
        }
    });
    if cancelled {
        return Ok(None);
    }

    // Threads finish in no particular order; keep the lists stable.
    scan.symlinks.sort();
    scan.errors.sort();

    reporter.finished(phase, scan.files.len());
    Ok(Some(scan))
}

/// What a walker thread found at one path.
enum Found {
    File(String, Entry),
    Dir(String),
    Symlink(String),
    Error(String, String),
}

/// Classify one walk result and send it to the assembling thread.
fn visit(
    root: &Path,
    result: std::result::Result<ignore::DirEntry, ignore::Error>,
    stop: &AtomicBool,
    tx: &mpsc::Sender<Found>,
) -> WalkState {
    if stop.load(Ordering::Relaxed) {
        return WalkState::Quit;
    }

    let found = match result {
        Err(e) => Found::Error(String::new(), e.to_string()),
        Ok(entry) => {
            let Ok(rel) = entry.path().strip_prefix(root) else {
                return WalkState::Continue;
            };
            let rel_path = rel_to_string(rel);
            if rel_path.is_empty() {
                return WalkState::Continue;
            }
            // `Skip` keeps the walk out of the trash directory altogether.
            if is_reserved(&rel_path) {
                return WalkState::Skip;
            }
            let Some(file_type) = entry.file_type() else {
                return WalkState::Continue;
            };

            if file_type.is_symlink() {
                Found::Symlink(rel_path)
            } else if file_type.is_dir() {
                Found::Dir(rel_path)
            } else {
                match entry.metadata() {
                    Ok(meta) => Found::File(
                        rel_path.clone(),
                        Entry {
                            rel_path,
                            kind: EntryKind::File,
                            size: meta.len(),
                            mtime: mtime_secs(&meta),
                        },
                    ),
                    Err(e) => Found::Error(rel_path, e.to_string()),
                }
            }
        }
    };

    if tx.send(found).is_err() {
        return WalkState::Quit;
    }
    WalkState::Continue
}

fn rel_to_string(rel: &Path) -> String {
    rel.components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// Paths FileSyncer owns, excluded regardless of ignore rules.
fn is_reserved(rel_path: &str) -> bool {
    rel_path == MARKER_FILE
        || rel_path == TRASH_DIR
        || rel_path.starts_with(&format!("{TRASH_DIR}/"))
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::progress::Progress;

    fn touch(root: &Path, rel: &str, contents: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    /// Scan with no progress reporting, for the tests that only care about the
    /// resulting tree.
    fn scan(root: &Path, extra_ignore_files: &[PathBuf]) -> Scan {
        scan_root(
            root,
            extra_ignore_files,
            Phase::ScanLocal,
            &Reporter::silent(),
        )
        .unwrap()
        .expect("a silent reporter never cancels")
    }

    #[test]
    fn finds_files_with_slash_separated_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        touch(dir.path(), "sub/b.txt", "b");
        let scan = scan(dir.path(), &[]);
        let keys: Vec<_> = scan.files.keys().cloned().collect();
        assert_eq!(keys, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
    }

    #[test]
    fn records_size_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "hello");
        let scan = scan(dir.path(), &[]);
        let e = &scan.files["a.txt"];
        assert_eq!(e.size, 5);
        assert_eq!(e.kind, EntryKind::File);
        assert!(e.mtime > 0);
    }

    #[test]
    fn records_directories_separately_from_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "sub/b.txt", "b");
        let scan = scan(dir.path(), &[]);
        assert!(scan.dirs.contains("sub"));
        assert!(!scan.files.contains_key("sub"));
    }

    #[test]
    fn root_itself_is_not_listed_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        let scan = scan(dir.path(), &[]);
        assert!(!scan.dirs.contains(""), "root leaked in as empty path");
    }

    #[test]
    fn symlinks_are_reported_not_synced() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "real.txt", "r");
        std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();
        let scan = scan(dir.path(), &[]);
        assert_eq!(scan.symlinks, vec!["link.txt".to_string()]);
        assert!(!scan.files.contains_key("link.txt"));
    }

    #[test]
    fn marker_and_trash_are_always_excluded() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), MARKER_FILE, "documents");
        touch(dir.path(), &format!("{TRASH_DIR}/2026/old.txt"), "x");
        touch(dir.path(), "keep.txt", "k");
        let scan = scan(dir.path(), &[]);
        let keys: Vec<_> = scan.files.keys().cloned().collect();
        assert_eq!(keys, vec!["keep.txt".to_string()]);
        assert!(!scan.dirs.iter().any(|d| d.starts_with(TRASH_DIR)));
    }

    #[test]
    fn syncignore_in_the_root_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), IGNORE_FILE, "*.tmp\nbuild/\n");
        touch(dir.path(), "keep.txt", "k");
        touch(dir.path(), "scratch.tmp", "s");
        touch(dir.path(), "build/out.bin", "o");
        let scan = scan(dir.path(), &[]);
        let keys: Vec<_> = scan.files.keys().cloned().collect();
        assert_eq!(keys, vec![IGNORE_FILE.to_string(), "keep.txt".to_string()]);
    }

    #[test]
    fn negation_patterns_work() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), IGNORE_FILE, "*.log\n!keep.log\n");
        touch(dir.path(), "drop.log", "d");
        touch(dir.path(), "keep.log", "k");
        let scan = scan(dir.path(), &[]);
        assert!(scan.files.contains_key("keep.log"));
        assert!(!scan.files.contains_key("drop.log"));
    }

    #[test]
    fn extra_ignore_files_are_unioned_in() {
        let dir = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();
        let other_ignore = other.path().join(IGNORE_FILE);
        std::fs::write(&other_ignore, "*.bak\n").unwrap();
        touch(dir.path(), "keep.txt", "k");
        touch(dir.path(), "old.bak", "b");
        let scan = scan(dir.path(), &[other_ignore]);
        assert!(scan.files.contains_key("keep.txt"));
        assert!(!scan.files.contains_key("old.bak"));
    }

    #[test]
    fn hidden_files_are_synced_by_default() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".bashrc", "x");
        let scan = scan(dir.path(), &[]);
        assert!(scan.files.contains_key(".bashrc"));
    }

    #[test]
    fn progress_brackets_the_walk_and_reports_every_file() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        touch(dir.path(), "sub/b.txt", "b");

        let events = std::cell::RefCell::new(Vec::new());
        let sink = |p| events.borrow_mut().push(p);
        let scan = scan_root(
            dir.path(),
            &[],
            Phase::ScanRemote,
            &Reporter::new(&sink, &|| false),
        )
        .unwrap()
        .unwrap();
        let events = events.into_inner();

        assert_eq!(
            events.first(),
            Some(&Progress::Started {
                phase: Phase::ScanRemote,
                total: None
            }),
            "a walk cannot know its size, so the total must be absent"
        );
        assert_eq!(
            events.last(),
            Some(&Progress::Finished {
                phase: Phase::ScanRemote,
                count: scan.files.len()
            })
        );

        let items: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Progress::Item { rel_path, .. } => Some(rel_path.clone()),
                _ => None,
            })
            .collect();
        assert_eq!(items.len(), scan.files.len());
        assert!(items.contains(&"a.txt".to_string()), "got: {items:?}");
        assert!(items.contains(&"sub/b.txt".to_string()), "got: {items:?}");
    }

    #[test]
    fn item_indices_count_up_from_zero() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            touch(dir.path(), &format!("f{i}.txt"), "x");
        }
        let indices = std::cell::RefCell::new(Vec::new());
        let sink = |p| {
            if let Progress::Item { index, .. } = p {
                indices.borrow_mut().push(index);
            }
        };
        scan_root(
            dir.path(),
            &[],
            Phase::ScanLocal,
            &Reporter::new(&sink, &|| false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(indices.into_inner(), vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn cancelling_mid_walk_yields_no_scan() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            touch(dir.path(), &format!("f{i}.txt"), "x");
        }
        let seen = std::cell::Cell::new(0usize);
        let sink = |_| {};
        let result = scan_root(
            dir.path(),
            &[],
            Phase::ScanLocal,
            &Reporter::new(&sink, &|| {
                seen.set(seen.get() + 1);
                seen.get() > 3
            }),
        )
        .unwrap();
        assert!(result.is_none(), "a cancelled walk has no result to report");
    }

    #[test]
    fn a_cancelled_walk_leaves_the_tree_untouched() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        let before: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        let sink = |_| {};
        scan_root(
            dir.path(),
            &[],
            Phase::ScanLocal,
            &Reporter::new(&sink, &|| true),
        )
        .unwrap();
        let after: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(before, after);
    }

    #[test]
    fn gitignore_files_are_not_honoured() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".gitignore", "secret.txt\n");
        touch(dir.path(), "secret.txt", "s");
        let scan = scan(dir.path(), &[]);
        assert!(
            scan.files.contains_key("secret.txt"),
            "only .syncignore should filter a sync"
        );
    }
}
