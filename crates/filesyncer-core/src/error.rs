use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("configuration error: {0}")]
    Config(String),

    #[error("root marker problem at {path}: {detail}")]
    Marker { path: PathBuf, detail: String },

    #[error(
        "refusing to delete {would_delete} of {tracked} tracked paths; \
         re-run with --force if this is intended"
    )]
    DeletionThreshold { would_delete: usize, tracked: usize },

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("no conflict recorded for {0}")]
    NoSuchConflict(String),

    #[error("that choice is not open for {0}")]
    ChoiceUnavailable(String),
}

impl Error {
    /// Attach a path to a `std::io::Error`, which carries none of its own.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io {
            path: path.into(),
            source,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_threshold_message_names_the_counts() {
        let e = Error::DeletionThreshold {
            would_delete: 400,
            tracked: 1000,
        };
        let msg = e.to_string();
        assert!(msg.contains("400"), "message was: {msg}");
        assert!(msg.contains("1000"), "message was: {msg}");
        assert!(msg.contains("--force"), "message was: {msg}");
    }

    #[test]
    fn io_error_includes_the_path() {
        let source = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "denied");
        let e = Error::io("/mnt/nas/file.txt", source);
        assert!(e.to_string().contains("/mnt/nas/file.txt"));
    }
}
