use crate::scan::TRASH_DIR;
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub const TEMP_SUFFIX: &str = ".filesyncer-tmp";

fn temp_path_for(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!("{name}{TEMP_SUFFIX}-{}", std::process::id()))
}

/// Copy `src` over `dst` such that `dst` is never observed partially written.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<()> {
    if let Some(parent) = dst.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = temp_path_for(dst);

    // Scoped so both handles are closed before the rename.
    {
        let mut reader = std::fs::File::open(src).map_err(|e| Error::io(src, e))?;
        let mut writer = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        if let Err(e) = std::io::copy(&mut reader, &mut writer) {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::io(&tmp, e));
        }
        if let Err(e) = writer.sync_all() {
            let _ = std::fs::remove_file(&tmp);
            return Err(Error::io(&tmp, e));
        }
    }

    std::fs::rename(&tmp, dst).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        Error::io(dst, e)
    })?;

    let meta = std::fs::metadata(src).map_err(|e| Error::io(src, e))?;
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    filetime::set_file_mtime(dst, mtime).map_err(|e| Error::io(dst, e))
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
            } else if entry.file_name().to_string_lossy().contains(TEMP_SUFFIX) {
                if std::fs::remove_file(&path).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
