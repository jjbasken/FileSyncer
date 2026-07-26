use crate::{Error, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Profile {
    pub id: String,
    pub name: String,
    pub local: PathBuf,
    pub remote: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileSet {
    #[serde(rename = "profile", default)]
    pub profiles: Vec<Profile>,
}

impl Profile {
    /// Both roots must exist as directories, be distinct, and neither may
    /// contain the other. Nesting silently produces infinite recursion, so it
    /// is rejected loudly.
    pub fn validate(&self) -> Result<()> {
        let local = canonical_dir(&self.local)?;
        let remote = canonical_dir(&self.remote)?;

        if local == remote {
            return Err(Error::Config(format!(
                "local and remote are the same directory: {}",
                local.display()
            )));
        }
        if remote.starts_with(&local) {
            return Err(Error::Config(format!(
                "local {} contains remote {}",
                local.display(),
                remote.display()
            )));
        }
        if local.starts_with(&remote) {
            return Err(Error::Config(format!(
                "remote {} contains local {}",
                remote.display(),
                local.display()
            )));
        }
        Ok(())
    }
}

fn canonical_dir(path: &Path) -> Result<PathBuf> {
    let meta =
        std::fs::metadata(path).map_err(|e| Error::Config(format!("{}: {e}", path.display())))?;
    if !meta.is_dir() {
        return Err(Error::Config(format!(
            "{}: not a directory",
            path.display()
        )));
    }
    std::fs::canonicalize(path).map_err(|e| Error::io(path, e))
}

impl ProfileSet {
    pub fn load(path: &Path) -> Result<ProfileSet> {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                toml::from_str(&text).map_err(|e| Error::Config(format!("{}: {e}", path.display())))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(ProfileSet::default()),
            Err(e) => Err(Error::io(path, e)),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|e| Error::Config(format!("serialising profiles: {e}")))?;
        std::fs::write(path, text).map_err(|e| Error::io(path, e))
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        self.profiles.iter().find(|p| p.id == id)
    }
}

/// `$XDG_CONFIG_HOME/filesyncer/profiles.toml`
pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filesyncer")
        .join("profiles.toml")
}

/// `$XDG_DATA_HOME/filesyncer/state/<id>.db`
pub fn state_db_path(id: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filesyncer")
        .join("state")
        .join(format!("{id}.db"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> ProfileSet {
        ProfileSet {
            profiles: vec![Profile {
                id: "documents".into(),
                name: "Documents ↔ NAS".into(),
                local: "/home/jeremy/Documents".into(),
                remote: "/mnt/nas/documents".into(),
            }],
        }
    }

    #[test]
    fn round_trips_through_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        sample().save(&path).unwrap();
        assert_eq!(ProfileSet::load(&path).unwrap(), sample());
    }

    #[test]
    fn serialises_as_profile_array_tables() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        sample().save(&path).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[[profile]]"), "got: {text}");
    }

    #[test]
    fn missing_file_loads_as_empty_set() {
        let dir = tempfile::tempdir().unwrap();
        let loaded = ProfileSet::load(&dir.path().join("absent.toml")).unwrap();
        assert!(loaded.profiles.is_empty());
    }

    #[test]
    fn save_creates_missing_parent_directories() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("deep").join("nested").join("profiles.toml");
        sample().save(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn malformed_toml_is_a_config_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("profiles.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();
        let err = ProfileSet::load(&path).unwrap_err();
        assert!(matches!(err, Error::Config(_)), "got: {err:?}");
    }

    #[test]
    fn get_finds_by_id_and_misses_cleanly() {
        let set = sample();
        assert_eq!(set.get("documents").unwrap().name, "Documents ↔ NAS");
        assert!(set.get("photos").is_none());
    }

    #[test]
    fn state_db_path_is_namespaced_by_id() {
        let p = state_db_path("documents");
        assert!(p.ends_with("filesyncer/state/documents.db"), "got: {p:?}");
    }

    #[test]
    fn config_path_lands_under_filesyncer() {
        assert!(config_path().ends_with("filesyncer/profiles.toml"));
    }

    fn profile_for(local: &Path, remote: &Path) -> Profile {
        Profile {
            id: "t".into(),
            name: "T".into(),
            local: local.to_path_buf(),
            remote: remote.to_path_buf(),
        }
    }

    #[test]
    fn two_distinct_directories_validate() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        std::fs::create_dir(&a).unwrap();
        std::fs::create_dir(&b).unwrap();
        profile_for(&a, &b).validate().unwrap();
    }

    #[test]
    fn missing_root_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let err = profile_for(&a, &dir.path().join("gone"))
            .validate()
            .unwrap_err();
        assert!(err.to_string().contains("gone"), "got: {err}");
    }

    #[test]
    fn file_instead_of_directory_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a");
        std::fs::create_dir(&a).unwrap();
        let f = dir.path().join("file.txt");
        std::fs::write(&f, "x").unwrap();
        let err = profile_for(&a, &f).validate().unwrap_err();
        assert!(err.to_string().contains("not a directory"), "got: {err}");
    }

    #[test]
    fn identical_roots_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let err = profile_for(dir.path(), dir.path()).validate().unwrap_err();
        assert!(err.to_string().contains("same directory"), "got: {err}");
    }

    #[test]
    fn remote_nested_inside_local_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let err = profile_for(dir.path(), &inner).validate().unwrap_err();
        assert!(err.to_string().contains("contains"), "got: {err}");
    }

    #[test]
    fn local_nested_inside_remote_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir(&inner).unwrap();
        let err = profile_for(&inner, dir.path()).validate().unwrap_err();
        assert!(err.to_string().contains("contains"), "got: {err}");
    }
}
