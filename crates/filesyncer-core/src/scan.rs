use crate::{Error, Result, marker::MARKER_FILE};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

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

pub fn scan_root(root: &Path, extra_ignore_files: &[PathBuf]) -> Result<Scan> {
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
        if extra.exists() {
            if let Some(err) = builder.add_ignore(extra) {
                return Err(Error::Config(format!("{}: {err}", extra.display())));
            }
        }
    }

    let mut scan = Scan::default();

    for result in builder.build() {
        let entry = match result {
            Ok(e) => e,
            Err(e) => {
                scan.errors.push((String::new(), e.to_string()));
                continue;
            }
        };

        let Ok(rel) = entry.path().strip_prefix(root) else {
            continue;
        };
        let rel_path = rel_to_string(rel);
        if rel_path.is_empty() || is_reserved(&rel_path) {
            continue;
        }

        let file_type = match entry.file_type() {
            Some(ft) => ft,
            None => continue,
        };

        if file_type.is_symlink() {
            scan.symlinks.push(rel_path);
            continue;
        }
        if file_type.is_dir() {
            scan.dirs.insert(rel_path);
            continue;
        }

        match entry.metadata() {
            Ok(meta) => {
                scan.files.insert(
                    rel_path.clone(),
                    Entry {
                        rel_path,
                        kind: EntryKind::File,
                        size: meta.len(),
                        mtime: mtime_secs(&meta),
                    },
                );
            }
            Err(e) => scan.errors.push((rel_path, e.to_string())),
        }
    }

    Ok(scan)
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

    fn touch(root: &Path, rel: &str, contents: &str) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, contents).unwrap();
    }

    #[test]
    fn finds_files_with_slash_separated_relative_paths() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        touch(dir.path(), "sub/b.txt", "b");
        let scan = scan_root(dir.path(), &[]).unwrap();
        let keys: Vec<_> = scan.files.keys().cloned().collect();
        assert_eq!(keys, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
    }

    #[test]
    fn records_size_and_kind() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "hello");
        let scan = scan_root(dir.path(), &[]).unwrap();
        let e = &scan.files["a.txt"];
        assert_eq!(e.size, 5);
        assert_eq!(e.kind, EntryKind::File);
        assert!(e.mtime > 0);
    }

    #[test]
    fn records_directories_separately_from_files() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "sub/b.txt", "b");
        let scan = scan_root(dir.path(), &[]).unwrap();
        assert!(scan.dirs.contains("sub"));
        assert!(!scan.files.contains_key("sub"));
    }

    #[test]
    fn root_itself_is_not_listed_as_a_directory() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "a.txt", "a");
        let scan = scan_root(dir.path(), &[]).unwrap();
        assert!(!scan.dirs.contains(""), "root leaked in as empty path");
    }

    #[test]
    fn symlinks_are_reported_not_synced() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), "real.txt", "r");
        std::os::unix::fs::symlink("real.txt", dir.path().join("link.txt")).unwrap();
        let scan = scan_root(dir.path(), &[]).unwrap();
        assert_eq!(scan.symlinks, vec!["link.txt".to_string()]);
        assert!(!scan.files.contains_key("link.txt"));
    }

    #[test]
    fn marker_and_trash_are_always_excluded() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), MARKER_FILE, "documents");
        touch(dir.path(), &format!("{TRASH_DIR}/2026/old.txt"), "x");
        touch(dir.path(), "keep.txt", "k");
        let scan = scan_root(dir.path(), &[]).unwrap();
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
        let scan = scan_root(dir.path(), &[]).unwrap();
        let keys: Vec<_> = scan.files.keys().cloned().collect();
        assert_eq!(keys, vec![IGNORE_FILE.to_string(), "keep.txt".to_string()]);
    }

    #[test]
    fn negation_patterns_work() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), IGNORE_FILE, "*.log\n!keep.log\n");
        touch(dir.path(), "drop.log", "d");
        touch(dir.path(), "keep.log", "k");
        let scan = scan_root(dir.path(), &[]).unwrap();
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
        let scan = scan_root(dir.path(), &[other_ignore]).unwrap();
        assert!(scan.files.contains_key("keep.txt"));
        assert!(!scan.files.contains_key("old.bak"));
    }

    #[test]
    fn hidden_files_are_synced_by_default() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".bashrc", "x");
        let scan = scan_root(dir.path(), &[]).unwrap();
        assert!(scan.files.contains_key(".bashrc"));
    }

    #[test]
    fn gitignore_files_are_not_honoured() {
        let dir = tempfile::tempdir().unwrap();
        touch(dir.path(), ".gitignore", "secret.txt\n");
        touch(dir.path(), "secret.txt", "s");
        let scan = scan_root(dir.path(), &[]).unwrap();
        assert!(
            scan.files.contains_key("secret.txt"),
            "only .syncignore should filter a sync"
        );
    }
}
