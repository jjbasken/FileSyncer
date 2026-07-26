use crate::{Error, Result};
use std::path::Path;

pub const MARKER_FILE: &str = ".filesyncer-root";

pub fn write_marker(root: &Path, profile_id: &str) -> Result<()> {
    let path = root.join(MARKER_FILE);
    std::fs::write(&path, format!("{profile_id}\n")).map_err(|e| Error::io(&path, e))
}

pub fn verify_marker(root: &Path, profile_id: &str) -> Result<()> {
    let path = root.join(MARKER_FILE);
    let found = match std::fs::read_to_string(&path) {
        Ok(text) => text.trim().to_string(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Err(Error::Marker {
                path: path.clone(),
                detail: format!(
                    "missing; if this root is correct, restore it by writing \
                     `{profile_id}` into {MARKER_FILE}. If the share is not \
                     mounted, mount it and retry."
                ),
            });
        }
        Err(e) => return Err(Error::io(&path, e)),
    };

    if found != profile_id {
        return Err(Error::Marker {
            path,
            detail: format!("expected profile id `{profile_id}`, found `{found}`"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn written_marker_verifies() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "documents").unwrap();
        verify_marker(dir.path(), "documents").unwrap();
    }

    #[test]
    fn missing_marker_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_marker(dir.path(), "documents").unwrap_err();
        assert!(matches!(err, Error::Marker { .. }), "got: {err:?}");
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn wrong_profile_id_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        write_marker(dir.path(), "photos").unwrap();
        let err = verify_marker(dir.path(), "documents").unwrap_err();
        assert!(err.to_string().contains("photos"), "got: {err}");
    }

    #[test]
    fn surrounding_whitespace_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(MARKER_FILE), "  documents\n\n").unwrap();
        verify_marker(dir.path(), "documents").unwrap();
    }

    #[test]
    fn error_message_explains_the_repair() {
        let dir = tempfile::tempdir().unwrap();
        let err = verify_marker(dir.path(), "documents")
            .unwrap_err()
            .to_string();
        assert!(err.contains(MARKER_FILE), "got: {err}");
        assert!(err.contains("documents"), "got: {err}");
    }
}
