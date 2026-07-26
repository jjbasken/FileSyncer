# FileSyncer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build a bidirectional file synchroniser between a local directory and a mounted network share, with conflicts flagged rather than resolved, deletions routed through a trash directory, and a scan/confirm/apply cycle exposed through both a CLI and a GTK4 GUI.

**Architecture:** A Cargo workspace of three crates. `filesyncer-core` holds the entire engine: scanning, a SQLite state store, a pure reconciliation function, and a plan applier. `filesyncer-cli` and `filesyncer-gui` are thin frontends that both call `core`'s `session` API. The reconciliation logic — which decides what gets deleted — takes hashing as an injected dependency so every branch is testable without touching a filesystem.

**Tech Stack:** Rust 2024 edition, `ignore` (gitignore matching), `rusqlite` (bundled SQLite), `blake3`, `serde`/`toml`, `clap`, `filetime`, GTK4 + libadwaita via `gtk4-rs`, `async-channel`.

## Global Constraints

- Rust edition **2024**; toolchain floor **1.96**.
- Workspace layout: crates live under `crates/<name>/`.
- `filesyncer-core` MUST NOT depend on `gtk4`, `libadwaita`, `clap`, or write to stdout/stderr. Enforced by review, not tooling.
- `rusqlite` MUST use the `bundled` feature. The build host has no system SQLite development package.
- Relative paths are stored and compared as `String` with `/` separators, never `PathBuf`.
- mtimes are `i64` seconds since the Unix epoch.
- Content hashes are lowercase hex BLAKE3 `String`s.
- Marker filename is exactly `.filesyncer-root`; trash directory is exactly `.filesyncer-trash`; ignore filename is exactly `.syncignore`.
- Deletion threshold is **25%** of tracked paths.
- Every task ends with a commit. Run `cargo fmt` before each commit.
- Add dependencies with `cargo add` so versions resolve to current releases; do not hand-write version numbers into `Cargo.toml`.

---

## File Structure

```
Cargo.toml                          workspace manifest
crates/filesyncer-core/
  src/lib.rs                        module wiring + re-exports
  src/error.rs                      Error enum, Result alias
  src/hash.rs                       BLAKE3 file hashing
  src/profile.rs                    Profile, ProfileSet, XDG paths, validation
  src/marker.rs                     root marker write/verify
  src/scan.rs                       Entry, Scan, scan_root
  src/state.rs                      StateRow, StateStore (SQLite)
  src/plan.rs                       Side, Action, Conflict, Skip, Plan
  src/reconcile.rs                  HashProvider trait, reconcile()
  src/apply.rs                      atomic copy, trash, apply()
  src/session.rs                    prepare()/run() orchestration
  tests/roundtrip.rs                end-to-end convergence + idempotence
crates/filesyncer-cli/
  src/main.rs                       entry point, exit codes
  src/args.rs                       clap definitions
  src/render.rs                     plan printing, progress, prompts
crates/filesyncer-gui/
  src/main.rs                       adw::Application bootstrap
  src/window.rs                     split view, profile sidebar
  src/profile_page.rs               profile detail + Scan button
  src/plan_view.rs                  plan groups + conflict rows
  src/run_view.rs                   progress + summary
  src/profile_dialog.rs             add-profile dialog
```

---

### Task 1: Workspace scaffold and error type

**Files:**
- Create: `Cargo.toml`
- Create: `crates/filesyncer-core/Cargo.toml`
- Create: `crates/filesyncer-core/src/lib.rs`
- Create: `crates/filesyncer-core/src/error.rs`
- Create: `.gitignore`

**Interfaces:**
- Consumes: nothing.
- Produces: `filesyncer_core::{Error, Result}`. `Error` variants used by every later task: `Io { path: PathBuf, source: std::io::Error }`, `Config(String)`, `Marker { path: PathBuf, detail: String }`, `DeletionThreshold { would_delete: usize, tracked: usize }`, `Db(rusqlite::Error)`, `NoSuchConflict(String)`.

- [ ] **Step 1: Create the workspace manifest**

`Cargo.toml`:

```toml
[workspace]
resolver = "3"
members = ["crates/filesyncer-core", "crates/filesyncer-cli", "crates/filesyncer-gui"]

[workspace.package]
version = "0.1.0"
edition = "2024"
rust-version = "1.96"
license = "GPL-3.0-or-later"
```

Note: `filesyncer-cli` and `filesyncer-gui` are listed now but created in Tasks 16 and 18. Until then `cargo` will error on the missing members, so create placeholder crates immediately in Step 2.

- [ ] **Step 2: Create all three crate skeletons**

```bash
mkdir -p crates
cargo new --lib crates/filesyncer-core
cargo new --bin crates/filesyncer-cli
cargo new --bin crates/filesyncer-gui
```

Edit each generated `Cargo.toml` so the `[package]` section inherits from the workspace:

```toml
[package]
name = "filesyncer-core"
version.workspace = true
edition.workspace = true
rust-version.workspace = true
license.workspace = true
```

Apply the same three-line inheritance change to `crates/filesyncer-cli/Cargo.toml` and `crates/filesyncer-gui/Cargo.toml`, changing only the `name`.

- [ ] **Step 3: Add core dependencies**

```bash
cargo add --package filesyncer-core thiserror serde --features serde/derive
cargo add --package filesyncer-core toml blake3 ignore filetime dirs
cargo add --package filesyncer-core rusqlite --features bundled
cargo add --package filesyncer-core --dev tempfile
```

- [ ] **Step 4: Write the error type**

`crates/filesyncer-core/src/error.rs`:

```rust
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
}

impl Error {
    /// Attach a path to a `std::io::Error`, which carries none of its own.
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Error::Io { path: path.into(), source }
    }
}
```

- [ ] **Step 5: Wire up lib.rs**

`crates/filesyncer-core/src/lib.rs`:

```rust
pub mod error;

pub use error::{Error, Result};
```

- [ ] **Step 6: Write a test that the error type formats usefully**

Append to `crates/filesyncer-core/src/error.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deletion_threshold_message_names_the_counts() {
        let e = Error::DeletionThreshold { would_delete: 400, tracked: 1000 };
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
```

- [ ] **Step 7: Run the tests**

Run: `cargo test --package filesyncer-core`
Expected: 2 passed.

- [ ] **Step 8: Write .gitignore and commit**

`.gitignore`:

```
/target
```

```bash
cargo fmt
git add -A
git commit -m "feat: workspace scaffold and core error type"
```

---

### Task 2: Content hashing

**Files:**
- Create: `crates/filesyncer-core/src/hash.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Error::io` from Task 1.
- Produces: `hash::hash_file(path: &Path) -> Result<String>` returning lowercase hex BLAKE3. Used by Task 11 (`FsHashProvider`) and Task 14.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/hash.rs`:

```rust
use crate::{Error, Result};
use std::path::Path;

/// BLAKE3 of a file's contents, as lowercase hex.
pub fn hash_file(path: &Path) -> Result<String> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_temp(contents: &[u8]) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(contents).unwrap();
        f.flush().unwrap();
        f
    }

    #[test]
    fn identical_contents_hash_identically() {
        let a = write_temp(b"hello world");
        let b = write_temp(b"hello world");
        assert_eq!(hash_file(a.path()).unwrap(), hash_file(b.path()).unwrap());
    }

    #[test]
    fn different_contents_hash_differently() {
        let a = write_temp(b"hello world");
        let b = write_temp(b"hello worlds");
        assert_ne!(hash_file(a.path()).unwrap(), hash_file(b.path()).unwrap());
    }

    #[test]
    fn empty_file_hashes_without_error() {
        let a = write_temp(b"");
        assert_eq!(hash_file(a.path()).unwrap().len(), 64);
    }

    #[test]
    fn missing_file_reports_its_path() {
        let err = hash_file(Path::new("/nonexistent/xyzzy")).unwrap_err();
        assert!(err.to_string().contains("xyzzy"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core hash`
Expected: FAIL — panics with `not implemented`.

- [ ] **Step 3: Implement hash_file**

Replace the `unimplemented!()` body:

```rust
pub fn hash_file(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let mut hasher = blake3::Hasher::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| Error::io(path, e))?;
    Ok(hasher.finalize().to_hex().to_string())
}
```

Remove the now-unused `unimplemented!` import concerns; keep `use crate::{Error, Result};`.

- [ ] **Step 4: Register the module**

In `crates/filesyncer-core/src/lib.rs`, add `pub mod hash;` above `pub mod error;`... actually keep alphabetical:

```rust
pub mod error;
pub mod hash;

pub use error::{Error, Result};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core hash`
Expected: 4 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: BLAKE3 content hashing"
```

---

### Task 3: Profile types and XDG paths

**Files:**
- Create: `crates/filesyncer-core/src/profile.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Error::{Config, io}`.
- Produces:
  - `struct Profile { id: String, name: String, local: PathBuf, remote: PathBuf }`
  - `struct ProfileSet { profiles: Vec<Profile> }` serialising as `[[profile]]` tables
  - `ProfileSet::load(&Path) -> Result<ProfileSet>`, `ProfileSet::save(&self, &Path) -> Result<()>`, `ProfileSet::get(&self, id: &str) -> Option<&Profile>`
  - `profile::config_path() -> PathBuf`, `profile::state_db_path(id: &str) -> PathBuf`

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/profile.rs`:

```rust
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

impl ProfileSet {
    pub fn load(path: &Path) -> Result<ProfileSet> {
        unimplemented!()
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        unimplemented!()
    }

    pub fn get(&self, id: &str) -> Option<&Profile> {
        unimplemented!()
    }
}

/// `$XDG_CONFIG_HOME/filesyncer/profiles.toml`
pub fn config_path() -> PathBuf {
    unimplemented!()
}

/// `$XDG_DATA_HOME/filesyncer/state/<id>.db`
pub fn state_db_path(id: &str) -> PathBuf {
    unimplemented!()
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
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core profile`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement the bodies**

```rust
impl ProfileSet {
    pub fn load(path: &Path) -> Result<ProfileSet> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| Error::Config(format!("{}: {e}", path.display()))),
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

pub fn config_path() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filesyncer")
        .join("profiles.toml")
}

pub fn state_db_path(id: &str) -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("filesyncer")
        .join("state")
        .join(format!("{id}.db"))
}
```

- [ ] **Step 4: Register the module**

Add `pub mod profile;` to `lib.rs` and re-export the types:

```rust
pub mod error;
pub mod hash;
pub mod profile;

pub use error::{Error, Result};
pub use profile::{Profile, ProfileSet};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core profile`
Expected: 8 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: profile definitions and XDG path resolution"
```

---

### Task 4: Profile validation

**Files:**
- Modify: `crates/filesyncer-core/src/profile.rs`

**Interfaces:**
- Consumes: `Profile` from Task 3.
- Produces: `Profile::validate(&self) -> Result<()>`. Called by Task 14 (`session::prepare`) and Task 21 (GUI dialog).

Validation rules from the spec: both roots exist, both are directories, they are not the same path, and neither contains the other.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `profile.rs`:

```rust
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
        let err = profile_for(&a, &dir.path().join("gone")).validate().unwrap_err();
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core profile`
Expected: FAIL — `no method named validate`.

- [ ] **Step 3: Implement validate**

Add to `impl Profile` (create the block above `impl ProfileSet`):

```rust
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
    let meta = std::fs::metadata(path).map_err(|e| {
        Error::Config(format!("{}: {e}", path.display()))
    })?;
    if !meta.is_dir() {
        return Err(Error::Config(format!("{}: not a directory", path.display())));
    }
    std::fs::canonicalize(path).map_err(|e| Error::io(path, e))
}
```

`canonicalize` resolves symlinks, so the nesting check catches a remote that reaches its parent through a link.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core profile`
Expected: 14 passed.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: profile root validation including nesting rejection"
```

---

### Task 5: Root markers

**Files:**
- Create: `crates/filesyncer-core/src/marker.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Error::{Marker, io}`.
- Produces:
  - `pub const MARKER_FILE: &str = ".filesyncer-root";`
  - `marker::write_marker(root: &Path, profile_id: &str) -> Result<()>`
  - `marker::verify_marker(root: &Path, profile_id: &str) -> Result<()>`

This is the primary defence against an unmounted share being read as a mass deletion. `verify_marker` never repairs; repairing automatically would defeat the check.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/marker.rs`:

```rust
use crate::{Error, Result};
use std::path::Path;

pub const MARKER_FILE: &str = ".filesyncer-root";

pub fn write_marker(root: &Path, profile_id: &str) -> Result<()> {
    unimplemented!()
}

pub fn verify_marker(root: &Path, profile_id: &str) -> Result<()> {
    unimplemented!()
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
        let err = verify_marker(dir.path(), "documents").unwrap_err().to_string();
        assert!(err.contains(MARKER_FILE), "got: {err}");
        assert!(err.contains("documents"), "got: {err}");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core marker`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement the bodies**

```rust
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
```

- [ ] **Step 4: Register the module**

`lib.rs`:

```rust
pub mod error;
pub mod hash;
pub mod marker;
pub mod profile;

pub use error::{Error, Result};
pub use marker::MARKER_FILE;
pub use profile::{Profile, ProfileSet};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core marker`
Expected: 5 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: root markers guarding against unmounted shares"
```

---

### Task 6: Directory scanning

**Files:**
- Create: `crates/filesyncer-core/src/scan.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `MARKER_FILE` from Task 5.
- Produces:
  - `pub const TRASH_DIR: &str = ".filesyncer-trash";`
  - `pub const IGNORE_FILE: &str = ".syncignore";`
  - `enum EntryKind { File, Dir, Symlink }`
  - `struct Entry { rel_path: String, kind: EntryKind, size: u64, mtime: i64 }`
  - `struct Scan { files: BTreeMap<String, Entry>, dirs: BTreeSet<String>, symlinks: Vec<String>, errors: Vec<(String, String)> }`
  - `scan::scan_root(root: &Path, extra_ignore_files: &[PathBuf]) -> Result<Scan>`

`extra_ignore_files` is how the spec's "rules from both roots are unioned" is achieved: the caller passes the *other* root's `.syncignore` path. Task 14 does this.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/scan.rs`:

```rust
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
    unimplemented!()
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
```

`gitignore_files_are_not_honoured` matters: a synced source tree would otherwise silently omit exactly the files a `.gitignore` hides, which is not what a backup should do.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core scan`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement scan_root**

```rust
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
    rel_path == MARKER_FILE || rel_path == TRASH_DIR || rel_path.starts_with(&format!("{TRASH_DIR}/"))
}

fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
```

`ignore(false)` disables the crate's default `.ignore` handling so only `.syncignore` filters; `git_ignore(false)` and friends make the `gitignore_files_are_not_honoured` test pass.

- [ ] **Step 4: Register the module**

Add `pub mod scan;` to `lib.rs` and re-export:

```rust
pub use scan::{Entry, EntryKind, Scan};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core scan`
Expected: 11 passed.

If `marker_and_trash_are_always_excluded` fails because the walker descends into the trash, note that `is_reserved` skips the entries but `ignore` still walks them; that is correct but wasteful. Add `builder.filter_entry(|e| { ... })` only if a later performance task calls for it — do not change behaviour now.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: directory scanning with .syncignore support"
```

---

### Task 7: SQLite state store

**Files:**
- Create: `crates/filesyncer-core/src/state.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Error::{Db, io}`.
- Produces:
  - `struct StateRow { rel_path: String, hash: String, local_size: u64, local_mtime: i64, remote_size: u64, remote_mtime: i64 }`
  - `struct StateStore`
  - `StateStore::open(&Path) -> Result<StateStore>`, `open_in_memory() -> Result<StateStore>`
  - `load_all(&self) -> Result<BTreeMap<String, StateRow>>`
  - `upsert(&self, &StateRow) -> Result<()>`, `remove(&self, rel_path: &str) -> Result<()>`
  - `tracked_count(&self) -> Result<usize>`
  - `last_sync(&self) -> Result<Option<i64>>`, `set_last_sync(&self, i64) -> Result<()>`

Per-row commits are what make an interrupted run resumable, so `upsert` must not be wrapped in a caller-held transaction.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/state.rs`:

```rust
use crate::{Error, Result};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StateRow {
    pub rel_path: String,
    pub hash: String,
    pub local_size: u64,
    pub local_mtime: i64,
    pub remote_size: u64,
    pub remote_mtime: i64,
}

pub struct StateStore {
    conn: rusqlite::Connection,
}

impl StateStore {
    pub fn open(path: &Path) -> Result<StateStore> {
        unimplemented!()
    }

    pub fn open_in_memory() -> Result<StateStore> {
        unimplemented!()
    }

    pub fn load_all(&self) -> Result<BTreeMap<String, StateRow>> {
        unimplemented!()
    }

    pub fn upsert(&self, row: &StateRow) -> Result<()> {
        unimplemented!()
    }

    pub fn remove(&self, rel_path: &str) -> Result<()> {
        unimplemented!()
    }

    pub fn tracked_count(&self) -> Result<usize> {
        unimplemented!()
    }

    pub fn last_sync(&self) -> Result<Option<i64>> {
        unimplemented!()
    }

    pub fn set_last_sync(&self, ts: i64) -> Result<()> {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(rel: &str) -> StateRow {
        StateRow {
            rel_path: rel.into(),
            hash: "abc123".into(),
            local_size: 10,
            local_mtime: 1000,
            remote_size: 10,
            remote_mtime: 1001,
        }
    }

    #[test]
    fn new_store_is_empty() {
        let s = StateStore::open_in_memory().unwrap();
        assert!(s.load_all().unwrap().is_empty());
        assert_eq!(s.tracked_count().unwrap(), 0);
        assert_eq!(s.last_sync().unwrap(), None);
    }

    #[test]
    fn upsert_then_load_round_trips() {
        let s = StateStore::open_in_memory().unwrap();
        s.upsert(&row("a.txt")).unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all["a.txt"], row("a.txt"));
    }

    #[test]
    fn upsert_replaces_an_existing_row() {
        let s = StateStore::open_in_memory().unwrap();
        s.upsert(&row("a.txt")).unwrap();
        let mut updated = row("a.txt");
        updated.hash = "def456".into();
        s.upsert(&updated).unwrap();
        assert_eq!(s.tracked_count().unwrap(), 1);
        assert_eq!(s.load_all().unwrap()["a.txt"].hash, "def456");
    }

    #[test]
    fn remove_deletes_only_the_named_row() {
        let s = StateStore::open_in_memory().unwrap();
        s.upsert(&row("a.txt")).unwrap();
        s.upsert(&row("b.txt")).unwrap();
        s.remove("a.txt").unwrap();
        let all = s.load_all().unwrap();
        assert_eq!(all.keys().cloned().collect::<Vec<_>>(), vec!["b.txt".to_string()]);
    }

    #[test]
    fn removing_an_absent_row_is_not_an_error() {
        let s = StateStore::open_in_memory().unwrap();
        s.remove("ghost.txt").unwrap();
    }

    #[test]
    fn last_sync_round_trips() {
        let s = StateStore::open_in_memory().unwrap();
        s.set_last_sync(1_700_000_000).unwrap();
        assert_eq!(s.last_sync().unwrap(), Some(1_700_000_000));
        s.set_last_sync(1_800_000_000).unwrap();
        assert_eq!(s.last_sync().unwrap(), Some(1_800_000_000));
    }

    #[test]
    fn large_sizes_survive_the_round_trip() {
        let s = StateStore::open_in_memory().unwrap();
        let mut big = row("huge.bin");
        big.local_size = 8_000_000_000;
        big.remote_size = 8_000_000_000;
        s.upsert(&big).unwrap();
        assert_eq!(s.load_all().unwrap()["huge.bin"].local_size, 8_000_000_000);
    }

    #[test]
    fn data_persists_across_reopen_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state").join("documents.db");
        {
            let s = StateStore::open(&path).unwrap();
            s.upsert(&row("a.txt")).unwrap();
            s.set_last_sync(42).unwrap();
        }
        let s = StateStore::open(&path).unwrap();
        assert_eq!(s.tracked_count().unwrap(), 1);
        assert_eq!(s.last_sync().unwrap(), Some(42));
    }

    #[test]
    fn paths_with_unicode_and_spaces_round_trip() {
        let s = StateStore::open_in_memory().unwrap();
        s.upsert(&row("dossiers/été 2026/note ✓.md")).unwrap();
        assert!(s.load_all().unwrap().contains_key("dossiers/été 2026/note ✓.md"));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core state`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement the store**

```rust
const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS entry (
    rel_path     TEXT PRIMARY KEY,
    hash         TEXT NOT NULL,
    local_size   INTEGER NOT NULL,
    local_mtime  INTEGER NOT NULL,
    remote_size  INTEGER NOT NULL,
    remote_mtime INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS meta (
    key   TEXT PRIMARY KEY,
    value INTEGER NOT NULL
);
";

impl StateStore {
    pub fn open(path: &Path) -> Result<StateStore> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
        }
        let conn = rusqlite::Connection::open(path)?;
        Self::init(conn)
    }

    pub fn open_in_memory() -> Result<StateStore> {
        Self::init(rusqlite::Connection::open_in_memory()?)
    }

    fn init(conn: rusqlite::Connection) -> Result<StateStore> {
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(StateStore { conn })
    }

    pub fn load_all(&self) -> Result<BTreeMap<String, StateRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT rel_path, hash, local_size, local_mtime, remote_size, remote_mtime
             FROM entry",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(StateRow {
                rel_path: r.get(0)?,
                hash: r.get(1)?,
                local_size: r.get::<_, i64>(2)? as u64,
                local_mtime: r.get(3)?,
                remote_size: r.get::<_, i64>(4)? as u64,
                remote_mtime: r.get(5)?,
            })
        })?;

        let mut out = BTreeMap::new();
        for row in rows {
            let row = row?;
            out.insert(row.rel_path.clone(), row);
        }
        Ok(out)
    }

    pub fn upsert(&self, row: &StateRow) -> Result<()> {
        self.conn.execute(
            "INSERT INTO entry
                (rel_path, hash, local_size, local_mtime, remote_size, remote_mtime)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(rel_path) DO UPDATE SET
                hash = excluded.hash,
                local_size = excluded.local_size,
                local_mtime = excluded.local_mtime,
                remote_size = excluded.remote_size,
                remote_mtime = excluded.remote_mtime",
            rusqlite::params![
                row.rel_path,
                row.hash,
                row.local_size as i64,
                row.local_mtime,
                row.remote_size as i64,
                row.remote_mtime,
            ],
        )?;
        Ok(())
    }

    pub fn remove(&self, rel_path: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM entry WHERE rel_path = ?1", [rel_path])?;
        Ok(())
    }

    pub fn tracked_count(&self) -> Result<usize> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM entry", [], |r| r.get(0))?;
        Ok(n as usize)
    }

    pub fn last_sync(&self) -> Result<Option<i64>> {
        let mut stmt = self
            .conn
            .prepare("SELECT value FROM meta WHERE key = 'last_sync'")?;
        let mut rows = stmt.query([])?;
        match rows.next()? {
            Some(r) => Ok(Some(r.get(0)?)),
            None => Ok(None),
        }
    }

    pub fn set_last_sync(&self, ts: i64) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES ('last_sync', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [ts],
        )?;
        Ok(())
    }
}
```

Sizes are stored as `INTEGER` (i64) and cast; SQLite has no unsigned type. Files above 8 EiB would overflow, which is not a real case.

- [ ] **Step 4: Register the module**

Add `pub mod state;` to `lib.rs` and re-export `pub use state::{StateRow, StateStore};`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core state`
Expected: 9 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: SQLite state store with per-row commits"
```

---

### Task 8: Plan types

**Files:**
- Create: `crates/filesyncer-core/src/plan.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: nothing.
- Produces (used by Tasks 9–21, so the names here are load-bearing):

```rust
pub enum Side { Local, Remote }
pub struct Stat { pub size: u64, pub mtime: i64 }
pub enum Action {
    Copy { rel_path: String, from: Side, size: u64, hash: String },
    Trash { rel_path: String, side: Side },
    RecordState { rel_path: String, hash: String, local: Stat, remote: Stat },
    DropState { rel_path: String },
}
pub enum ConflictKind { BothEdited, BothNew, EditedLocalDeletedRemote, EditedRemoteDeletedLocal, TypeMismatch }
pub struct Conflict { pub rel_path: String, pub kind: ConflictKind, pub local: Option<Stat>, pub remote: Option<Stat> }
pub enum SkipReason { Symlink, Unreadable(String) }
pub struct Skip { pub rel_path: String, pub reason: SkipReason }
pub struct Plan { pub actions: Vec<Action>, pub conflicts: Vec<Conflict>, pub skips: Vec<Skip> }
```

Note the design decision encoded here: `Copy` carries the source hash, and `RecordState` carries the hash and both stats. Reconciliation already computed them, so `apply` never hashes anything. Without this, the common mtime-drift case would re-hash the entire tree twice per run.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/plan.rs` — write the type definitions above verbatim, deriving `#[derive(Debug, Clone, PartialEq, Eq)]` on each (and `Copy` on `Side` and `ConflictKind`), add `#[derive(Default)]` to `Plan`, then:

```rust
impl Side {
    pub fn other(self) -> Side {
        unimplemented!()
    }
}

impl Plan {
    /// True when applying this plan would not touch either filesystem.
    pub fn is_noop(&self) -> bool {
        unimplemented!()
    }

    pub fn copy_count(&self, to: Side) -> usize {
        unimplemented!()
    }

    pub fn copy_bytes(&self, to: Side) -> u64 {
        unimplemented!()
    }

    pub fn trash_count(&self) -> usize {
        unimplemented!()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(rel: &str, from: Side, size: u64) -> Action {
        Action::Copy { rel_path: rel.into(), from, size, hash: "h".into() }
    }

    #[test]
    fn side_other_flips() {
        assert_eq!(Side::Local.other(), Side::Remote);
        assert_eq!(Side::Remote.other(), Side::Local);
    }

    #[test]
    fn empty_plan_is_a_noop() {
        assert!(Plan::default().is_noop());
    }

    #[test]
    fn state_only_actions_are_a_noop() {
        let plan = Plan {
            actions: vec![
                Action::DropState { rel_path: "a".into() },
                Action::RecordState {
                    rel_path: "b".into(),
                    hash: "h".into(),
                    local: Stat { size: 1, mtime: 2 },
                    remote: Stat { size: 1, mtime: 3 },
                },
            ],
            ..Default::default()
        };
        assert!(plan.is_noop(), "state bookkeeping does not touch either tree");
    }

    #[test]
    fn a_copy_makes_the_plan_non_noop() {
        let plan = Plan { actions: vec![copy("a", Side::Local, 5)], ..Default::default() };
        assert!(!plan.is_noop());
    }

    #[test]
    fn conflicts_alone_are_a_noop() {
        let plan = Plan {
            conflicts: vec![Conflict {
                rel_path: "a".into(),
                kind: ConflictKind::BothEdited,
                local: Some(Stat { size: 1, mtime: 1 }),
                remote: Some(Stat { size: 2, mtime: 2 }),
            }],
            ..Default::default()
        };
        assert!(plan.is_noop(), "unresolved conflicts are never acted on");
    }

    #[test]
    fn copy_counts_are_reported_by_destination_not_source() {
        let plan = Plan {
            actions: vec![
                copy("a", Side::Local, 10),
                copy("b", Side::Local, 20),
                copy("c", Side::Remote, 100),
            ],
            ..Default::default()
        };
        assert_eq!(plan.copy_count(Side::Remote), 2);
        assert_eq!(plan.copy_bytes(Side::Remote), 30);
        assert_eq!(plan.copy_count(Side::Local), 1);
        assert_eq!(plan.copy_bytes(Side::Local), 100);
    }

    #[test]
    fn trash_count_totals_both_sides() {
        let plan = Plan {
            actions: vec![
                Action::Trash { rel_path: "a".into(), side: Side::Local },
                Action::Trash { rel_path: "b".into(), side: Side::Remote },
            ],
            ..Default::default()
        };
        assert_eq!(plan.trash_count(), 2);
    }
}
```

The destination-not-source convention in `copy_count` is what the GUI's "Copy to remote" heading and the CLI's output both read from. Getting it backwards would mislabel every run.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core plan`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement the methods**

```rust
impl Side {
    pub fn other(self) -> Side {
        match self {
            Side::Local => Side::Remote,
            Side::Remote => Side::Local,
        }
    }
}

impl Plan {
    pub fn is_noop(&self) -> bool {
        !self.actions.iter().any(|a| {
            matches!(a, Action::Copy { .. } | Action::Trash { .. })
        })
    }

    pub fn copy_count(&self, to: Side) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::Copy { from, .. } if from.other() == to))
            .count()
    }

    pub fn copy_bytes(&self, to: Side) -> u64 {
        self.actions
            .iter()
            .filter_map(|a| match a {
                Action::Copy { from, size, .. } if from.other() == to => Some(*size),
                _ => None,
            })
            .sum()
    }

    pub fn trash_count(&self) -> usize {
        self.actions
            .iter()
            .filter(|a| matches!(a, Action::Trash { .. }))
            .count()
    }
}
```

- [ ] **Step 4: Register the module**

`lib.rs`:

```rust
pub mod plan;

pub use plan::{Action, Conflict, ConflictKind, Plan, Side, Skip, SkipReason, Stat};
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core plan`
Expected: 7 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: plan, action, and conflict types"
```

---

### Task 9: Reconciliation — files present on both sides

**Files:**
- Create: `crates/filesyncer-core/src/reconcile.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Scan` (Task 6), `StateRow` (Task 7), all of `plan` (Task 8).
- Produces:
  - `trait HashProvider { fn hash(&self, side: Side, rel_path: &str) -> Result<String>; }`
  - `reconcile::reconcile(local: &Scan, remote: &Scan, state: &BTreeMap<String, StateRow>, hashes: &dyn HashProvider) -> Result<Plan>`
  - `struct MapHashProvider(pub BTreeMap<(Side, String), String>)` — test double, `#[cfg(test)]`-free so Task 10's tests and the integration tests can both use it. Gate it behind `#[doc(hidden)]` rather than `#[cfg(test)]`.

This task implements only the rows of the decision table where the path exists on both sides. Task 10 adds deletions and new files. Splitting here keeps each task's test suite reviewable.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/reconcile.rs`:

```rust
use crate::plan::{Action, Conflict, ConflictKind, Plan, Side, Skip, SkipReason, Stat};
use crate::scan::Scan;
use crate::state::StateRow;
use crate::{Error, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Supplies content hashes on demand. Injected so the decision logic itself
/// performs no I/O and every branch is testable without a filesystem.
pub trait HashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String>;
}

/// A `HashProvider` backed by a lookup table, for tests.
#[doc(hidden)]
pub struct MapHashProvider(pub BTreeMap<(Side, String), String>);

impl HashProvider for MapHashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String> {
        self.0
            .get(&(side, rel_path.to_string()))
            .cloned()
            .ok_or_else(|| Error::Config(format!("test hash missing for {side:?} {rel_path}")))
    }
}

pub fn reconcile(
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    hashes: &dyn HashProvider,
) -> Result<Plan> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scan::{Entry, EntryKind};

    /// Build a scan containing one file per (path, size, mtime) triple.
    fn scan_of(items: &[(&str, u64, i64)]) -> Scan {
        let mut s = Scan::default();
        for (path, size, mtime) in items {
            s.files.insert(
                path.to_string(),
                Entry {
                    rel_path: path.to_string(),
                    kind: EntryKind::File,
                    size: *size,
                    mtime: *mtime,
                },
            );
        }
        s
    }

    fn state_of(items: &[(&str, &str, u64, i64, u64, i64)]) -> BTreeMap<String, StateRow> {
        items
            .iter()
            .map(|(p, h, ls, lm, rs, rm)| {
                (
                    p.to_string(),
                    StateRow {
                        rel_path: p.to_string(),
                        hash: h.to_string(),
                        local_size: *ls,
                        local_mtime: *lm,
                        remote_size: *rs,
                        remote_mtime: *rm,
                    },
                )
            })
            .collect()
    }

    fn hashes(items: &[(Side, &str, &str)]) -> MapHashProvider {
        MapHashProvider(
            items
                .iter()
                .map(|(s, p, h)| ((*s, p.to_string()), h.to_string()))
                .collect(),
        )
    }

    fn no_hashes() -> MapHashProvider {
        MapHashProvider(BTreeMap::new())
    }

    #[test]
    fn unchanged_on_both_sides_produces_nothing() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = reconcile(&local, &remote, &state, &no_hashes()).unwrap();
        assert!(plan.actions.is_empty(), "got: {:?}", plan.actions);
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn local_edit_copies_to_remote() {
        let local = scan_of(&[("a.txt", 12, 150)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h2")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 12,
                hash: "h2".into(),
            }]
        );
    }

    #[test]
    fn remote_edit_copies_to_local() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 14, 250)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "h3")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Remote,
                size: 14,
                hash: "h3".into(),
            }]
        );
    }

    #[test]
    fn mtime_drift_without_content_change_records_state_and_copies_nothing() {
        // The remount case: mtime shifted on both sides, contents identical.
        let local = scan_of(&[("a.txt", 10, 900)]);
        let remote = scan_of(&[("a.txt", 10, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h1"), (Side::Remote, "a.txt", "h1")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "h1".into(),
                local: Stat { size: 10, mtime: 900 },
                remote: Stat { size: 10, mtime: 901 },
            }]
        );
        assert!(plan.is_noop(), "drift must not schedule any transfer");
    }

    #[test]
    fn both_edited_to_the_same_content_converges_without_transfer() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 20, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h9"), (Side::Remote, "a.txt", "h9")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "h9".into(),
                local: Stat { size: 20, mtime: 900 },
                remote: Stat { size: 20, mtime: 901 },
            }]
        );
    }

    #[test]
    fn both_edited_differently_is_a_conflict() {
        let local = scan_of(&[("a.txt", 20, 900)]);
        let remote = scan_of(&[("a.txt", 30, 901)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();
        assert!(plan.actions.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![Conflict {
                rel_path: "a.txt".into(),
                kind: ConflictKind::BothEdited,
                local: Some(Stat { size: 20, mtime: 900 }),
                remote: Some(Stat { size: 30, mtime: 901 }),
            }]
        );
    }

    #[test]
    fn new_on_both_sides_with_identical_content_is_adopted() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hX"), (Side::Remote, "a.txt", "hX")]);
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::RecordState {
                rel_path: "a.txt".into(),
                hash: "hX".into(),
                local: Stat { size: 10, mtime: 100 },
                remote: Stat { size: 10, mtime: 200 },
            }]
        );
    }

    #[test]
    fn new_on_both_sides_with_different_content_is_a_conflict() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let remote = scan_of(&[("a.txt", 11, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hL"), (Side::Remote, "a.txt", "hR")]);
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &h).unwrap();
        assert_eq!(plan.conflicts[0].kind, ConflictKind::BothNew);
    }

    #[test]
    fn file_on_one_side_directory_on_the_other_is_a_type_mismatch() {
        let local = scan_of(&[("thing", 10, 100)]);
        let mut remote = Scan::default();
        remote.dirs.insert("thing".to_string());
        remote.files.insert(
            "thing/inner.txt".to_string(),
            Entry {
                rel_path: "thing/inner.txt".into(),
                kind: EntryKind::File,
                size: 1,
                mtime: 5,
            },
        );
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &no_hashes()).unwrap();
        assert!(
            plan.conflicts.iter().any(|c| c.rel_path == "thing"
                && c.kind == ConflictKind::TypeMismatch),
            "got: {:?}",
            plan.conflicts
        );
    }

    #[test]
    fn symlinks_and_errors_become_skips() {
        let mut local = scan_of(&[]);
        local.symlinks.push("link.txt".into());
        local.errors.push(("bad.txt".into(), "permission denied".into()));
        let plan = reconcile(&local, &Scan::default(), &BTreeMap::new(), &no_hashes()).unwrap();
        assert!(plan.skips.iter().any(|s| s.rel_path == "link.txt"
            && s.reason == SkipReason::Symlink));
        assert!(plan.skips.iter().any(|s| s.rel_path == "bad.txt"
            && matches!(s.reason, SkipReason::Unreadable(_))));
    }

    #[test]
    fn a_skipped_symlink_is_reported_once_per_side() {
        let mut local = Scan::default();
        local.symlinks.push("link.txt".into());
        let mut remote = Scan::default();
        remote.symlinks.push("link.txt".into());
        let plan = reconcile(&local, &remote, &BTreeMap::new(), &no_hashes()).unwrap();
        assert_eq!(plan.skips.len(), 2, "got: {:?}", plan.skips);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core reconcile`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement reconcile for the both-present cases**

```rust
pub fn reconcile(
    local: &Scan,
    remote: &Scan,
    state: &BTreeMap<String, StateRow>,
    hashes: &dyn HashProvider,
) -> Result<Plan> {
    let mut plan = Plan::default();

    collect_skips(local, &mut plan);
    collect_skips(remote, &mut plan);

    let mut keys: BTreeSet<&str> = BTreeSet::new();
    keys.extend(local.files.keys().map(String::as_str));
    keys.extend(remote.files.keys().map(String::as_str));
    keys.extend(state.keys().map(String::as_str));

    for key in keys {
        // A path that is a file on one side and a directory on the other can
        // never be reconciled by copying; refuse to touch it.
        if local.dirs.contains(key) || remote.dirs.contains(key) {
            plan.conflicts.push(Conflict {
                rel_path: key.to_string(),
                kind: ConflictKind::TypeMismatch,
                local: local.files.get(key).map(stat_of),
                remote: remote.files.get(key).map(stat_of),
            });
            continue;
        }

        let l = local.files.get(key);
        let r = remote.files.get(key);
        let s = state.get(key);

        match (l, r, s) {
            (Some(l), Some(r), Some(s)) => {
                let l_suspect = l.size != s.local_size || l.mtime != s.local_mtime;
                let r_suspect = r.size != s.remote_size || r.mtime != s.remote_mtime;
                if !l_suspect && !r_suspect {
                    continue;
                }

                let lh = if l_suspect {
                    hashes.hash(Side::Local, key)?
                } else {
                    s.hash.clone()
                };
                let rh = if r_suspect {
                    hashes.hash(Side::Remote, key)?
                } else {
                    s.hash.clone()
                };

                match (lh != s.hash, rh != s.hash) {
                    (false, false) => plan.actions.push(Action::RecordState {
                        rel_path: key.to_string(),
                        hash: s.hash.clone(),
                        local: stat_of(l),
                        remote: stat_of(r),
                    }),
                    (true, false) => plan.actions.push(Action::Copy {
                        rel_path: key.to_string(),
                        from: Side::Local,
                        size: l.size,
                        hash: lh,
                    }),
                    (false, true) => plan.actions.push(Action::Copy {
                        rel_path: key.to_string(),
                        from: Side::Remote,
                        size: r.size,
                        hash: rh,
                    }),
                    (true, true) => {
                        if lh == rh {
                            plan.actions.push(Action::RecordState {
                                rel_path: key.to_string(),
                                hash: lh,
                                local: stat_of(l),
                                remote: stat_of(r),
                            });
                        } else {
                            plan.conflicts.push(Conflict {
                                rel_path: key.to_string(),
                                kind: ConflictKind::BothEdited,
                                local: Some(stat_of(l)),
                                remote: Some(stat_of(r)),
                            });
                        }
                    }
                }
            }

            (Some(l), Some(r), None) => {
                let lh = hashes.hash(Side::Local, key)?;
                let rh = hashes.hash(Side::Remote, key)?;
                if lh == rh {
                    plan.actions.push(Action::RecordState {
                        rel_path: key.to_string(),
                        hash: lh,
                        local: stat_of(l),
                        remote: stat_of(r),
                    });
                } else {
                    plan.conflicts.push(Conflict {
                        rel_path: key.to_string(),
                        kind: ConflictKind::BothNew,
                        local: Some(stat_of(l)),
                        remote: Some(stat_of(r)),
                    });
                }
            }

            // Task 10 fills in the remaining arms.
            _ => {}
        }
    }

    Ok(plan)
}

fn stat_of(e: &crate::scan::Entry) -> Stat {
    Stat { size: e.size, mtime: e.mtime }
}

fn collect_skips(scan: &Scan, plan: &mut Plan) {
    for path in &scan.symlinks {
        plan.skips.push(Skip {
            rel_path: path.clone(),
            reason: SkipReason::Symlink,
        });
    }
    for (path, message) in &scan.errors {
        plan.skips.push(Skip {
            rel_path: path.clone(),
            reason: SkipReason::Unreadable(message.clone()),
        });
    }
}
```

Note the `(false, false)` arm: a file can be *suspect* on both sides yet unchanged in content. That is the remount case, and it must record state rather than transfer.

- [ ] **Step 4: Register the module**

Add `pub mod reconcile;` to `lib.rs` and `pub use reconcile::{HashProvider, reconcile};`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core reconcile`
Expected: 11 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: reconcile files present on both sides"
```

---

### Task 10: Reconciliation — deletions and new files

**Files:**
- Modify: `crates/filesyncer-core/src/reconcile.rs`

**Interfaces:**
- Consumes: everything from Task 9.
- Produces: no new public names. Replaces the `_ => {}` arm with the remaining rows of the decision table.

This is the task that decides what gets deleted. Review it accordingly.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `reconcile.rs`:

```rust
    #[test]
    fn remote_deleted_and_local_unchanged_trashes_local() {
        let local = scan_of(&[("a.txt", 10, 100)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = reconcile(&local, &Scan::default(), &state, &no_hashes()).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash { rel_path: "a.txt".into(), side: Side::Local }]
        );
    }

    #[test]
    fn local_deleted_and_remote_unchanged_trashes_remote() {
        let remote = scan_of(&[("a.txt", 10, 200)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = reconcile(&Scan::default(), &remote, &state, &no_hashes()).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash { rel_path: "a.txt".into(), side: Side::Remote }]
        );
    }

    #[test]
    fn remote_deleted_but_local_edited_is_a_conflict() {
        let local = scan_of(&[("a.txt", 99, 900)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "hNEW")]);
        let plan = reconcile(&local, &Scan::default(), &state, &h).unwrap();
        assert!(plan.actions.is_empty(), "got: {:?}", plan.actions);
        assert_eq!(
            plan.conflicts,
            vec![Conflict {
                rel_path: "a.txt".into(),
                kind: ConflictKind::EditedLocalDeletedRemote,
                local: Some(Stat { size: 99, mtime: 900 }),
                remote: None,
            }]
        );
    }

    #[test]
    fn local_deleted_but_remote_edited_is_a_conflict() {
        let remote = scan_of(&[("a.txt", 99, 900)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Remote, "a.txt", "hNEW")]);
        let plan = reconcile(&Scan::default(), &remote, &state, &h).unwrap();
        assert_eq!(plan.conflicts[0].kind, ConflictKind::EditedRemoteDeletedLocal);
        assert_eq!(plan.conflicts[0].local, None);
    }

    #[test]
    fn a_suspect_but_unchanged_file_whose_peer_vanished_is_still_trashed() {
        // mtime drifted locally, but content matches state: the remote deletion
        // is the only real change, so propagate it rather than calling it a conflict.
        let local = scan_of(&[("a.txt", 10, 777)]);
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let h = hashes(&[(Side::Local, "a.txt", "h1")]);
        let plan = reconcile(&local, &Scan::default(), &state, &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash { rel_path: "a.txt".into(), side: Side::Local }]
        );
    }

    #[test]
    fn a_new_local_file_copies_to_remote() {
        let local = scan_of(&[("new.txt", 42, 100)]);
        let h = hashes(&[(Side::Local, "new.txt", "hN")]);
        let plan = reconcile(&local, &Scan::default(), &BTreeMap::new(), &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "new.txt".into(),
                from: Side::Local,
                size: 42,
                hash: "hN".into(),
            }]
        );
    }

    #[test]
    fn a_new_remote_file_copies_to_local() {
        let remote = scan_of(&[("new.txt", 42, 100)]);
        let h = hashes(&[(Side::Remote, "new.txt", "hN")]);
        let plan = reconcile(&Scan::default(), &remote, &BTreeMap::new(), &h).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "new.txt".into(),
                from: Side::Remote,
                size: 42,
                hash: "hN".into(),
            }]
        );
    }

    #[test]
    fn gone_from_both_sides_drops_the_state_row() {
        let state = state_of(&[("a.txt", "h1", 10, 100, 10, 200)]);
        let plan = reconcile(&Scan::default(), &Scan::default(), &state, &no_hashes()).unwrap();
        assert_eq!(plan.actions, vec![Action::DropState { rel_path: "a.txt".into() }]);
        assert!(plan.is_noop(), "dropping a row touches no files");
    }

    #[test]
    fn an_emptied_remote_schedules_deletion_of_every_tracked_path() {
        // reconcile itself does not refuse this; Task 11's threshold check does.
        let local = scan_of(&[("a.txt", 1, 1), ("b.txt", 1, 1), ("c.txt", 1, 1)]);
        let state = state_of(&[
            ("a.txt", "h", 1, 1, 1, 1),
            ("b.txt", "h", 1, 1, 1, 1),
            ("c.txt", "h", 1, 1, 1, 1),
        ]);
        let plan = reconcile(&local, &Scan::default(), &state, &no_hashes()).unwrap();
        assert_eq!(plan.trash_count(), 3);
    }

    #[test]
    fn a_mixed_tree_produces_every_action_kind_at_once() {
        let local = scan_of(&[
            ("same.txt", 10, 100),   // unchanged
            ("edited.txt", 20, 900), // edited locally
            ("newlocal.txt", 5, 50), // new local
            ("delremote.txt", 10, 100), // remote deleted this
        ]);
        let remote = scan_of(&[
            ("same.txt", 10, 200),
            ("edited.txt", 10, 200),
            ("newremote.txt", 7, 70),
            ("dellocal.txt", 10, 200),
        ]);
        let state = state_of(&[
            ("same.txt", "h1", 10, 100, 10, 200),
            ("edited.txt", "h2", 10, 100, 10, 200),
            ("delremote.txt", "h3", 10, 100, 10, 200),
            ("dellocal.txt", "h4", 10, 100, 10, 200),
        ]);
        let h = hashes(&[
            (Side::Local, "edited.txt", "hE"),
            (Side::Local, "newlocal.txt", "hNL"),
            (Side::Remote, "newremote.txt", "hNR"),
        ]);
        let plan = reconcile(&local, &remote, &state, &h).unwrap();

        assert_eq!(plan.copy_count(Side::Remote), 2, "edited.txt + newlocal.txt");
        assert_eq!(plan.copy_count(Side::Local), 1, "newremote.txt");
        assert_eq!(plan.trash_count(), 2, "delremote.txt + dellocal.txt");
        assert!(plan.conflicts.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core reconcile`
Expected: FAIL — several tests assert non-empty `plan.actions` but get `[]`, because the `_ => {}` arm swallows these cases.

- [ ] **Step 3: Replace the catch-all arm**

Delete `// Task 10 fills in the remaining arms.` and `_ => {}`, and put these arms in its place:

```rust
            (Some(l), None, Some(s)) => {
                let suspect = l.size != s.local_size || l.mtime != s.local_mtime;
                let changed = suspect && hashes.hash(Side::Local, key)? != s.hash;
                if changed {
                    plan.conflicts.push(Conflict {
                        rel_path: key.to_string(),
                        kind: ConflictKind::EditedLocalDeletedRemote,
                        local: Some(stat_of(l)),
                        remote: None,
                    });
                } else {
                    plan.actions.push(Action::Trash {
                        rel_path: key.to_string(),
                        side: Side::Local,
                    });
                }
            }

            (None, Some(r), Some(s)) => {
                let suspect = r.size != s.remote_size || r.mtime != s.remote_mtime;
                let changed = suspect && hashes.hash(Side::Remote, key)? != s.hash;
                if changed {
                    plan.conflicts.push(Conflict {
                        rel_path: key.to_string(),
                        kind: ConflictKind::EditedRemoteDeletedLocal,
                        local: None,
                        remote: Some(stat_of(r)),
                    });
                } else {
                    plan.actions.push(Action::Trash {
                        rel_path: key.to_string(),
                        side: Side::Remote,
                    });
                }
            }

            (Some(l), None, None) => plan.actions.push(Action::Copy {
                rel_path: key.to_string(),
                from: Side::Local,
                size: l.size,
                hash: hashes.hash(Side::Local, key)?,
            }),

            (None, Some(r), None) => plan.actions.push(Action::Copy {
                rel_path: key.to_string(),
                from: Side::Remote,
                size: r.size,
                hash: hashes.hash(Side::Remote, key)?,
            }),

            (None, None, Some(_)) => plan.actions.push(Action::DropState {
                rel_path: key.to_string(),
            }),

            (None, None, None) => unreachable!("key came from one of the three sources"),
```

The `suspect && ...` short-circuit matters: it means an unchanged file whose peer was deleted never gets hashed, and `no_hashes()` in the tests proves it.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core reconcile`
Expected: 21 passed.

- [ ] **Step 5: Verify the whole decision table has coverage**

Run: `cargo test --package filesyncer-core`
Expected: all green. Cross-check the fourteen rows of the spec's decision table against the test names in `reconcile.rs`; every row must have at least one test.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: reconcile deletions, new files, and edit-vs-delete conflicts"
```

---

### Task 11: Deletion threshold and conflict resolution

**Files:**
- Modify: `crates/filesyncer-core/src/plan.rs`
- Modify: `crates/filesyncer-core/src/reconcile.rs`

**Interfaces:**
- Consumes: `Plan`, `Conflict`, `Side`, `Error::{DeletionThreshold, NoSuchConflict}`.
- Produces:
  - `pub const DELETION_THRESHOLD_PCT: usize = 25;`
  - `Plan::check_deletion_threshold(&self, tracked: usize) -> Result<()>`
  - `Plan::resolve_conflict(&mut self, rel_path: &str, keep: Side) -> Result<()>`
  - `reconcile::FsHashProvider { local_root: PathBuf, remote_root: PathBuf }` implementing `HashProvider` over the real filesystem.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module in `plan.rs`:

```rust
    fn conflict(rel: &str, kind: ConflictKind, local: Option<Stat>, remote: Option<Stat>) -> Conflict {
        Conflict { rel_path: rel.into(), kind, local, remote }
    }

    #[test]
    fn a_plan_with_no_deletions_always_passes_the_threshold() {
        let plan = Plan { actions: vec![copy("a", Side::Local, 1)], ..Default::default() };
        plan.check_deletion_threshold(4).unwrap();
    }

    #[test]
    fn deleting_a_quarter_or_less_passes() {
        let plan = Plan {
            actions: vec![Action::Trash { rel_path: "a".into(), side: Side::Local }],
            ..Default::default()
        };
        plan.check_deletion_threshold(4).unwrap();
    }

    #[test]
    fn deleting_more_than_a_quarter_is_refused() {
        let plan = Plan {
            actions: vec![
                Action::Trash { rel_path: "a".into(), side: Side::Local },
                Action::Trash { rel_path: "b".into(), side: Side::Local },
            ],
            ..Default::default()
        };
        let err = plan.check_deletion_threshold(4).unwrap_err();
        assert!(matches!(err, crate::Error::DeletionThreshold { .. }), "got: {err:?}");
    }

    #[test]
    fn an_unmounted_share_wiping_everything_is_refused() {
        let actions = (0..100)
            .map(|i| Action::Trash { rel_path: format!("f{i}"), side: Side::Local })
            .collect();
        let plan = Plan { actions, ..Default::default() };
        assert!(plan.check_deletion_threshold(100).is_err());
    }

    #[test]
    fn an_empty_state_table_cannot_trip_the_threshold() {
        // First-ever run: nothing is tracked, so nothing can be a deletion.
        Plan::default().check_deletion_threshold(0).unwrap();
    }

    #[test]
    fn keeping_local_turns_a_both_edited_conflict_into_a_copy() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::BothEdited,
                Some(Stat { size: 20, mtime: 900 }),
                Some(Stat { size: 30, mtime: 901 }),
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            plan.actions,
            vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 20,
                hash: String::new(),
            }]
        );
    }

    #[test]
    fn keeping_remote_copies_the_other_way() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::BothEdited,
                Some(Stat { size: 20, mtime: 900 }),
                Some(Stat { size: 30, mtime: 901 }),
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Remote).unwrap();
        assert_eq!(plan.copy_count(Side::Local), 1);
    }

    #[test]
    fn keeping_the_deleted_side_trashes_the_survivor() {
        // Local edited it, remote deleted it, user sides with the deletion.
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::EditedLocalDeletedRemote,
                Some(Stat { size: 20, mtime: 900 }),
                None,
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Remote).unwrap();
        assert_eq!(
            plan.actions,
            vec![Action::Trash { rel_path: "a.txt".into(), side: Side::Local }]
        );
    }

    #[test]
    fn keeping_the_surviving_side_of_a_delete_conflict_copies_it_back() {
        let mut plan = Plan {
            conflicts: vec![conflict(
                "a.txt",
                ConflictKind::EditedLocalDeletedRemote,
                Some(Stat { size: 20, mtime: 900 }),
                None,
            )],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert_eq!(plan.copy_count(Side::Remote), 1);
    }

    #[test]
    fn resolving_an_unknown_path_is_an_error() {
        let mut plan = Plan::default();
        let err = plan.resolve_conflict("ghost.txt", Side::Local).unwrap_err();
        assert!(matches!(err, crate::Error::NoSuchConflict(_)), "got: {err:?}");
    }

    #[test]
    fn resolving_one_conflict_leaves_the_others_alone() {
        let mut plan = Plan {
            conflicts: vec![
                conflict("a.txt", ConflictKind::BothEdited,
                    Some(Stat { size: 1, mtime: 1 }), Some(Stat { size: 2, mtime: 2 })),
                conflict("b.txt", ConflictKind::BothEdited,
                    Some(Stat { size: 1, mtime: 1 }), Some(Stat { size: 2, mtime: 2 })),
            ],
            ..Default::default()
        };
        plan.resolve_conflict("a.txt", Side::Local).unwrap();
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].rel_path, "b.txt");
    }
```

The empty `hash` in the expected `Copy` is deliberate: a resolved conflict has no precomputed hash, so `apply` must hash the source at copy time. Task 13 handles that.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core plan`
Expected: FAIL — `no method named check_deletion_threshold`.

- [ ] **Step 3: Implement the threshold and resolution**

Add to `plan.rs`:

```rust
/// A plan deleting more than this share of tracked paths is refused.
pub const DELETION_THRESHOLD_PCT: usize = 25;

impl Plan {
    /// Refuse plans that would delete an implausible share of tracked paths.
    /// The usual cause is an unmounted share scanning as an empty directory.
    pub fn check_deletion_threshold(&self, tracked: usize) -> crate::Result<()> {
        let would_delete = self.trash_count();
        if would_delete == 0 || tracked == 0 {
            return Ok(());
        }
        if would_delete * 100 > tracked * DELETION_THRESHOLD_PCT {
            return Err(crate::Error::DeletionThreshold { would_delete, tracked });
        }
        Ok(())
    }

    /// Turn a flagged conflict into a concrete action by choosing a winner.
    /// Keeping a side that holds no file means keeping a deletion.
    pub fn resolve_conflict(&mut self, rel_path: &str, keep: Side) -> crate::Result<()> {
        let index = self
            .conflicts
            .iter()
            .position(|c| c.rel_path == rel_path)
            .ok_or_else(|| crate::Error::NoSuchConflict(rel_path.to_string()))?;
        let conflict = self.conflicts.remove(index);

        let winner = match keep {
            Side::Local => conflict.local.as_ref(),
            Side::Remote => conflict.remote.as_ref(),
        };

        match winner {
            Some(stat) => self.actions.push(Action::Copy {
                rel_path: conflict.rel_path,
                from: keep,
                size: stat.size,
                // Not precomputed: `apply` hashes the source as it copies.
                hash: String::new(),
            }),
            None => self.actions.push(Action::Trash {
                rel_path: conflict.rel_path,
                side: keep.other(),
            }),
        }
        Ok(())
    }
}
```

`would_delete * 100 > tracked * 25` avoids floating point and integer-division rounding, so "exactly a quarter" passes and one more fails.

- [ ] **Step 4: Add the real HashProvider**

Append to `reconcile.rs`:

```rust
/// Hashes files on the real filesystem.
pub struct FsHashProvider {
    pub local_root: std::path::PathBuf,
    pub remote_root: std::path::PathBuf,
}

impl HashProvider for FsHashProvider {
    fn hash(&self, side: Side, rel_path: &str) -> Result<String> {
        let root = match side {
            Side::Local => &self.local_root,
            Side::Remote => &self.remote_root,
        };
        crate::hash::hash_file(&root.join(rel_path))
    }
}
```

- [ ] **Step 5: Add a test for FsHashProvider**

Append to the `tests` module in `reconcile.rs`:

```rust
    #[test]
    fn fs_hash_provider_reads_from_the_right_root() {
        let local = tempfile::tempdir().unwrap();
        let remote = tempfile::tempdir().unwrap();
        std::fs::write(local.path().join("a.txt"), "local contents").unwrap();
        std::fs::write(remote.path().join("a.txt"), "remote contents").unwrap();
        let p = FsHashProvider {
            local_root: local.path().to_path_buf(),
            remote_root: remote.path().to_path_buf(),
        };
        assert_ne!(
            p.hash(Side::Local, "a.txt").unwrap(),
            p.hash(Side::Remote, "a.txt").unwrap()
        );
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core`
Expected: all green — 11 new `plan` tests plus 1 new `reconcile` test.

- [ ] **Step 7: Update re-exports and commit**

In `lib.rs` add `DELETION_THRESHOLD_PCT` to the `plan` re-export list and `FsHashProvider` to the `reconcile` one.

```bash
cargo fmt
git add -A
git commit -m "feat: deletion threshold guard and conflict resolution"
```

---

### Task 12: Atomic copy and trash primitives

**Files:**
- Create: `crates/filesyncer-core/src/apply.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: `Error::io`, `scan::TRASH_DIR`.
- Produces (crate-internal but `pub` for testing):
  - `apply::copy_atomic(src: &Path, dst: &Path) -> Result<()>`
  - `apply::move_to_trash(root: &Path, rel_path: &str, run_stamp: &str) -> Result<()>`
  - `apply::clean_stale_temps(root: &Path) -> Result<usize>`
  - `pub const TEMP_SUFFIX: &str = ".filesyncer-tmp";`

Task 13 builds `apply()` on top of these. Splitting means the crash-safety primitives get their own review.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/apply.rs`:

```rust
use crate::plan::{Action, Plan, Side};
use crate::scan::TRASH_DIR;
use crate::state::{StateRow, StateStore};
use crate::{Error, Result};
use std::path::{Path, PathBuf};

pub const TEMP_SUFFIX: &str = ".filesyncer-tmp";

/// Copy `src` over `dst` such that `dst` is never observed partially written.
pub fn copy_atomic(src: &Path, dst: &Path) -> Result<()> {
    unimplemented!()
}

/// Move `root/rel_path` into `root/.filesyncer-trash/<run_stamp>/<rel_path>`.
pub fn move_to_trash(root: &Path, rel_path: &str, run_stamp: &str) -> Result<()> {
    unimplemented!()
}

/// Remove leftover temp files from an interrupted run. Returns how many.
pub fn clean_stale_temps(root: &Path) -> Result<usize> {
    unimplemented!()
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
        let got = filetime::FileTime::from_last_modification_time(
            &std::fs::metadata(&dst).unwrap(),
        );
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
        let err = copy_atomic(&dir.path().join("absent.txt"), &dir.path().join("dst.txt"))
            .unwrap_err();
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
        assert_eq!(std::fs::read_to_string(base.join("run1/a.txt")).unwrap(), "one");
        assert_eq!(std::fs::read_to_string(base.join("run2/a.txt")).unwrap(), "two");
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
        std::fs::write(dir.path().join("sub").join(format!("b.txt{TEMP_SUFFIX}-9")), "y").unwrap();
        std::fs::write(dir.path().join("keep.txt"), "z").unwrap();

        assert_eq!(clean_stale_temps(dir.path()).unwrap(), 2);
        assert!(dir.path().join("keep.txt").exists());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core apply`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement the primitives**

```rust
fn temp_path_for(dst: &Path) -> PathBuf {
    let name = dst.file_name().unwrap_or_default().to_string_lossy();
    dst.with_file_name(format!("{name}{TEMP_SUFFIX}-{}", std::process::id()))
}

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
            let Ok(file_type) = entry.file_type() else { continue };
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
```

- [ ] **Step 4: Register the module**

Add `pub mod apply;` to `lib.rs`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core apply`
Expected: 9 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: atomic copy, trash, and stale temp cleanup"
```

---

### Task 13: Applying a plan

**Files:**
- Modify: `crates/filesyncer-core/src/apply.rs`

**Interfaces:**
- Consumes: Task 12's primitives, `Plan`/`Action`/`Side`, `StateStore`/`StateRow`, `hash::hash_file`.
- Produces:
  - `struct ApplyCtx<'a> { local_root: &'a Path, remote_root: &'a Path, state: &'a StateStore, run_stamp: String }`
  - `enum Progress { Started { total: usize }, Item { index: usize, total: usize, rel_path: String }, Finished }`
  - `struct RunSummary { copied: usize, trashed: usize, conflicts: usize, errors: Vec<(String, String)>, cancelled: bool }`
  - `apply::apply(plan: &Plan, ctx: &ApplyCtx, progress: &dyn Fn(Progress), cancel: &dyn Fn() -> bool) -> Result<RunSummary>`

Two behaviours the spec pins down and the tests must prove: per-file errors do not abort the run and leave the state row untouched so the next run retries; and state is committed per file so cancellation leaves a consistent, resumable database.

- [ ] **Step 1: Write the failing tests**

Append to `apply.rs` (above the existing `#[cfg(test)]` module, add the types; inside it, the tests):

```rust
pub struct ApplyCtx<'a> {
    pub local_root: &'a Path,
    pub remote_root: &'a Path,
    pub state: &'a StateStore,
    pub run_stamp: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Progress {
    Started { total: usize },
    Item { index: usize, total: usize, rel_path: String },
    Finished,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RunSummary {
    pub copied: usize,
    pub trashed: usize,
    pub conflicts: usize,
    pub errors: Vec<(String, String)>,
    pub cancelled: bool,
}

pub fn apply(
    plan: &Plan,
    ctx: &ApplyCtx,
    progress: &dyn Fn(Progress),
    cancel: &dyn Fn() -> bool,
) -> Result<RunSummary> {
    unimplemented!()
}

/// A timestamp suitable for naming a trash subdirectory.
pub fn run_stamp_now() -> String {
    unimplemented!()
}
```

Tests inside the `tests` module:

```rust
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
        Fixture { _dir: dir, local, remote, state }
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
        apply(plan, &f.ctx(), &|_| {}, &|| false).unwrap()
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
                hash: "precomputed".into(),
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);

        assert_eq!(summary.copied, 1);
        assert_eq!(std::fs::read_to_string(f.remote.join("a.txt")).unwrap(), "payload");
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(row.hash, "precomputed");
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
                hash: "h".into(),
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert_eq!(std::fs::read_to_string(f.local.join("a.txt")).unwrap(), "from remote");
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
                hash: "h".into(),
            }],
            ..Default::default()
        };
        run(&plan, &f);
        assert!(f.remote.join("deep/nested/a.txt").exists());
    }

    #[test]
    fn a_resolved_conflict_with_no_precomputed_hash_gets_hashed_at_copy_time() {
        let f = fixture();
        f.write_local("a.txt", "payload");
        let plan = Plan {
            actions: vec![Action::Copy {
                rel_path: "a.txt".into(),
                from: Side::Local,
                size: 7,
                hash: String::new(),
            }],
            ..Default::default()
        };
        run(&plan, &f);
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(row.hash, crate::hash::hash_file(&f.local.join("a.txt")).unwrap());
        assert!(!row.hash.is_empty());
    }

    #[test]
    fn trash_moves_the_file_and_drops_the_state_row() {
        let f = fixture();
        f.write_local("a.txt", "bye");
        f.state
            .upsert(&StateRow {
                rel_path: "a.txt".into(),
                hash: "h".into(),
                local_size: 3,
                local_mtime: 1,
                remote_size: 3,
                remote_mtime: 1,
            })
            .unwrap();

        let plan = Plan {
            actions: vec![Action::Trash { rel_path: "a.txt".into(), side: Side::Local }],
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
                hash: "h9".into(),
                local: Stat { size: 20, mtime: 900 },
                remote: Stat { size: 20, mtime: 901 },
            }],
            ..Default::default()
        };
        let summary = run(&plan, &f);
        assert_eq!(summary.copied, 0);
        let row = &f.state.load_all().unwrap()["a.txt"];
        assert_eq!(row.hash, "h9");
        assert_eq!(row.remote_mtime, 901);
    }

    #[test]
    fn drop_state_removes_the_row() {
        let f = fixture();
        f.state
            .upsert(&StateRow {
                rel_path: "a.txt".into(),
                hash: "h".into(),
                local_size: 1, local_mtime: 1, remote_size: 1, remote_mtime: 1,
            })
            .unwrap();
        let plan = Plan {
            actions: vec![Action::DropState { rel_path: "a.txt".into() }],
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
                    from: Side::Local, size: 1, hash: "h".into(),
                },
                Action::Copy {
                    rel_path: "good.txt".into(),
                    from: Side::Local, size: 2, hash: "h".into(),
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
                from: Side::Local, size: 1, hash: "h".into(),
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
                Action::Copy { rel_path: "a.txt".into(), from: Side::Local, size: 3, hash: "h".into() },
                Action::Copy { rel_path: "b.txt".into(), from: Side::Local, size: 3, hash: "h".into() },
            ],
            ..Default::default()
        };
        let seen = std::cell::Cell::new(0usize);
        let summary = apply(&plan, &f.ctx(), &|_| {}, &|| {
            seen.set(seen.get() + 1);
            seen.get() > 1 // allow the first item, cancel before the second
        })
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
                rel_path: "a.txt".into(), from: Side::Local, size: 3, hash: "h".into(),
            }],
            ..Default::default()
        };
        let events = std::cell::RefCell::new(Vec::new());
        apply(&plan, &f.ctx(), &|p| events.borrow_mut().push(p), &|| false).unwrap();

        let events = events.into_inner();
        assert_eq!(events.first(), Some(&Progress::Started { total: 1 }));
        assert_eq!(events.last(), Some(&Progress::Finished));
        assert!(events.iter().any(|e| matches!(
            e, Progress::Item { rel_path, .. } if rel_path == "a.txt"
        )));
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
        assert_eq!(std::fs::read_to_string(f.local.join("a.txt")).unwrap(), "local");
        assert_eq!(std::fs::read_to_string(f.remote.join("a.txt")).unwrap(), "remote");
        assert!(f.state.load_all().unwrap().is_empty(), "conflict rows stay untouched");
    }

    #[test]
    fn run_stamp_is_sortable_and_filename_safe() {
        let s = run_stamp_now();
        assert_eq!(s.len(), 15, "expected YYYYMMDD-HHMMSS, got {s}");
        assert!(s.chars().all(|c| c.is_ascii_digit() || c == '-'), "got {s}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core apply`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement apply**

```rust
impl ApplyCtx<'_> {
    fn root(&self, side: Side) -> &Path {
        match side {
            Side::Local => self.local_root,
            Side::Remote => self.remote_root,
        }
    }
}

fn stat_of_file(path: &Path) -> Result<crate::plan::Stat> {
    let meta = std::fs::metadata(path).map_err(|e| Error::io(path, e))?;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    Ok(crate::plan::Stat { size: meta.len(), mtime })
}

pub fn apply(
    plan: &Plan,
    ctx: &ApplyCtx,
    progress: &dyn Fn(Progress),
    cancel: &dyn Fn() -> bool,
) -> Result<RunSummary> {
    let mut summary = RunSummary {
        conflicts: plan.conflicts.len(),
        ..Default::default()
    };

    clean_stale_temps(ctx.local_root)?;
    clean_stale_temps(ctx.remote_root)?;

    let total = plan.actions.len();
    progress(Progress::Started { total });

    for (index, action) in plan.actions.iter().enumerate() {
        if cancel() {
            summary.cancelled = true;
            break;
        }

        let rel_path = match action {
            Action::Copy { rel_path, .. }
            | Action::Trash { rel_path, .. }
            | Action::RecordState { rel_path, .. }
            | Action::DropState { rel_path } => rel_path.clone(),
        };

        progress(Progress::Item { index, total, rel_path: rel_path.clone() });

        // Each action commits its own state row, so an interruption leaves a
        // database describing exactly the work that completed.
        let outcome = match action {
            Action::Copy { rel_path, from, hash, .. } => {
                apply_copy(ctx, rel_path, *from, hash).map(|()| Outcome::Copied)
            }
            Action::Trash { rel_path, side } => {
                move_to_trash(ctx.root(*side), rel_path, &ctx.run_stamp)
                    .and_then(|()| ctx.state.remove(rel_path))
                    .map(|()| Outcome::Trashed)
            }
            Action::RecordState { rel_path, hash, local, remote } => ctx
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

    progress(Progress::Finished);
    Ok(summary)
}

enum Outcome {
    Copied,
    Trashed,
    Bookkeeping,
}

fn apply_copy(ctx: &ApplyCtx, rel_path: &str, from: Side, hash: &str) -> Result<()> {
    let src = ctx.root(from).join(rel_path);
    let dst = ctx.root(from.other()).join(rel_path);
    copy_atomic(&src, &dst)?;

    // A conflict resolved in the GUI carries no precomputed hash.
    let hash = if hash.is_empty() {
        crate::hash::hash_file(&src)?
    } else {
        hash.to_string()
    };

    let src_stat = stat_of_file(&src)?;
    let dst_stat = stat_of_file(&dst)?;
    let (local, remote) = match from {
        Side::Local => (src_stat, dst_stat),
        Side::Remote => (dst_stat, src_stat),
    };

    ctx.state.upsert(&StateRow {
        rel_path: rel_path.to_string(),
        hash,
        local_size: local.size,
        local_mtime: local.mtime,
        remote_size: remote.size,
        remote_mtime: remote.mtime,
    })
}

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
```

Note that `Action::Copy` writes the *destination's* stat after the copy rather than assuming it equals the source's. Network filesystems sometimes round mtimes on write, and storing the value we assumed rather than the value on disk would make every subsequent run see the file as suspect.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core apply`
Expected: 22 passed (9 from Task 12, 13 new).

- [ ] **Step 5: Update re-exports and commit**

`lib.rs`: `pub use apply::{ApplyCtx, Progress, RunSummary, apply};`

```bash
cargo fmt
git add -A
git commit -m "feat: apply plans with per-file error isolation and cancellation"
```

---

### Task 14: Session orchestration

**Files:**
- Create: `crates/filesyncer-core/src/session.rs`
- Modify: `crates/filesyncer-core/src/lib.rs`

**Interfaces:**
- Consumes: everything built so far.
- Produces — this is the entire API the CLI and GUI use:
  - `struct Session { profile: Profile, state: StateStore }`
  - `Session::open(profile: &Profile) -> Result<Session>` — validates the profile, verifies both markers, opens the state DB
  - `Session::open_with_state_path(profile: &Profile, state_path: &Path) -> Result<Session>` — same, for tests
  - `Session::prepare(&self, force: bool) -> Result<Plan>` — scan both roots, reconcile, threshold check
  - `Session::run(&self, plan: &Plan, progress: &dyn Fn(Progress), cancel: &dyn Fn() -> bool) -> Result<RunSummary>`
  - `Session::last_sync(&self) -> Result<Option<i64>>`, `Session::tracked_count(&self) -> Result<usize>`
  - `session::create_profile_roots(profile: &Profile) -> Result<()>` — validate and write both markers; used by CLI `add` and the GUI dialog

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/src/session.rs`:

```rust
use crate::apply::{ApplyCtx, Progress, RunSummary, apply, run_stamp_now};
use crate::marker;
use crate::plan::Plan;
use crate::profile::Profile;
use crate::reconcile::{FsHashProvider, reconcile};
use crate::scan::{IGNORE_FILE, scan_root};
use crate::state::StateStore;
use crate::{Result, profile};
use std::path::Path;

pub struct Session {
    profile: Profile,
    state: StateStore,
}

impl Session {
    pub fn open(profile: &Profile) -> Result<Session> {
        unimplemented!()
    }

    pub fn open_with_state_path(profile: &Profile, state_path: &Path) -> Result<Session> {
        unimplemented!()
    }

    pub fn profile(&self) -> &Profile {
        &self.profile
    }

    pub fn prepare(&self, force: bool) -> Result<Plan> {
        unimplemented!()
    }

    pub fn run(
        &self,
        plan: &Plan,
        progress: &dyn Fn(Progress),
        cancel: &dyn Fn() -> bool,
    ) -> Result<RunSummary> {
        unimplemented!()
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
    unimplemented!()
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
        let err = Session::open_with_state_path(&f.profile, &f.dir.path().join("s.db"))
            .unwrap_err();
        assert!(err.to_string().contains("missing"), "got: {err}");
    }

    #[test]
    fn opening_a_nested_profile_fails_validation() {
        let dir = tempfile::tempdir().unwrap();
        let inner = dir.path().join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let profile = Profile {
            id: "t".into(), name: "T".into(),
            local: dir.path().to_path_buf(), remote: inner,
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
        assert!(!paths.contains(&"scratch.tmp".to_string()), "got: {paths:?}");
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
        assert!(matches!(err, crate::Error::DeletionThreshold { .. }), "got: {err:?}");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-core session`
Expected: FAIL — `not implemented`.

- [ ] **Step 3: Implement Session**

```rust
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
}

pub fn create_profile_roots(profile: &Profile) -> Result<()> {
    profile.validate()?;
    marker::write_marker(&profile.local, &profile.id)?;
    marker::write_marker(&profile.remote, &profile.id)
}
```

`prepare` re-verifies the markers even though `open` already did: a share can drop between opening a session and pressing Scan, which is exactly the GUI's usage pattern.

- [ ] **Step 4: Register the module**

Add `pub mod session;` to `lib.rs` and `pub use session::{Session, create_profile_roots};`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-core session`
Expected: 10 passed.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: session orchestration shared by both frontends"
```

---

### Task 15: End-to-end convergence and idempotence

**Files:**
- Create: `crates/filesyncer-core/tests/roundtrip.rs`

**Interfaces:**
- Consumes: the public `filesyncer_core` API only. If something needed here is not public, that is a finding — fix the export rather than reaching into internals.
- Produces: no library code.

The idempotence property is the highest-value test in the suite: a second run immediately after a first must produce an empty plan. It catches change-detection bugs, mtime-drift bugs, and state-writing bugs in one assertion.

- [ ] **Step 1: Write the failing tests**

`crates/filesyncer-core/tests/roundtrip.rs`:

```rust
use filesyncer_core::{Profile, Session, Side, create_profile_roots};
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
        let plan = s.prepare(false).unwrap();
        s.run(&plan, &|_| {}, &|| false).unwrap()
    }
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
                    let rel = path.strip_prefix(root).unwrap().to_string_lossy().to_string();
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

    let plan = w.session().prepare(false).unwrap();
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
    let plan = s.prepare(false).unwrap();
    assert!(plan.is_noop(), "drift must not transfer; got: {:?}", plan.actions);

    s.run(&plan, &|_| {}, &|| false).unwrap();
    let plan = s.prepare(false).unwrap();
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

    assert_eq!(std::fs::read_to_string(w.remote("a.txt")).unwrap(), "alpha edited locally");
    assert_eq!(std::fs::read_to_string(w.local("b.txt")).unwrap(), "beta edited remotely");
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
    let plan = s.prepare(false).unwrap();
    assert_eq!(plan.conflicts.len(), 1);
    s.run(&plan, &|_| {}, &|| false).unwrap();

    assert_eq!(std::fs::read_to_string(w.local("a.txt")).unwrap(), "local version");
    assert_eq!(std::fs::read_to_string(w.remote("a.txt")).unwrap(), "remote version");
}

#[test]
fn an_unresolved_conflict_reappears_on_every_run() {
    let w = world();
    w.write(w.local("a.txt"), "original");
    w.sync();
    w.write(w.local("a.txt"), "local version");
    w.write(w.remote("a.txt"), "remote version");
    w.sync();

    let plan = w.session().prepare(false).unwrap();
    assert_eq!(plan.conflicts.len(), 1, "the conflict must not be forgotten");
}

#[test]
fn resolving_a_conflict_converges_the_trees() {
    let w = world();
    w.write(w.local("a.txt"), "original");
    w.sync();
    w.write(w.local("a.txt"), "local version");
    w.write(w.remote("a.txt"), "remote version");

    let s = w.session();
    let mut plan = s.prepare(false).unwrap();
    plan.resolve_conflict("a.txt", Side::Local).unwrap();
    s.run(&plan, &|_| {}, &|| false).unwrap();

    assert_eq!(std::fs::read_to_string(w.remote("a.txt")).unwrap(), "local version");
    let plan = s.prepare(false).unwrap();
    assert!(plan.actions.is_empty(), "resolution should settle the state");
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

    let err = Session::open_with_state_path(&w.profile, &w.dir.path().join("state.db"))
        .unwrap_err();
    assert!(err.to_string().contains("missing"), "got: {err}");

    for i in 0..10 {
        assert!(w.local(&format!("f{i}.txt")).exists(), "local tree must be intact");
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

    let plan = w.session().prepare(false).unwrap();
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

    let plan = w.session().prepare(false).unwrap();
    assert!(plan.skips.iter().any(|s| s.rel_path == "link.txt"));
    w.sync();
    assert!(!w.remote("link.txt").exists());
    assert!(w.remote("real.txt").exists());
}

#[test]
fn a_thousand_files_converge_and_then_settle() {
    let w = world();
    for i in 0..1000 {
        w.write(w.local(&format!("dir{}/f{i}.txt", i % 20)), &format!("contents {i}"));
    }
    let summary = w.sync();
    assert_eq!(summary.copied, 1000);
    assert!(summary.errors.is_empty());
    assert_trees_match(&w.profile.local, &w.profile.remote);

    let plan = w.session().prepare(false).unwrap();
    assert!(plan.actions.is_empty(), "large tree must also be idempotent");
}
```

- [ ] **Step 2: Add the dev-dependency the integration test needs**

```bash
cargo add --package filesyncer-core --dev filetime
```

`filetime` is already a normal dependency; adding it as a dev-dependency too makes it available to `tests/`. If `cargo` reports it as redundant, skip this step — normal dependencies are visible to integration tests.

- [ ] **Step 3: Run the tests to verify they fail or pass**

Run: `cargo test --package filesyncer-core --test roundtrip`
Expected: these exercise already-implemented code, so most should pass immediately. Any failure here is a real bug in Tasks 6–14, not a missing feature. Debug using superpowers:systematic-debugging rather than weakening the assertion.

- [ ] **Step 4: Run the whole suite**

Run: `cargo test --workspace`
Expected: all green.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add -A
git commit -m "test: end-to-end convergence, idempotence, and safeguard coverage"
```

---

### Task 16: CLI — argument parsing and plan rendering

**Files:**
- Create: `crates/filesyncer-cli/src/args.rs`
- Create: `crates/filesyncer-cli/src/render.rs`
- Modify: `crates/filesyncer-cli/src/main.rs`
- Modify: `crates/filesyncer-cli/Cargo.toml`

**Interfaces:**
- Consumes: `filesyncer_core::{Plan, Action, Conflict, ConflictKind, Side, Skip, SkipReason, RunSummary}`.
- Produces:
  - `args::{Cli, Command}` — clap definitions
  - `render::plan_summary(plan: &Plan) -> String`
  - `render::run_summary(summary: &RunSummary) -> String`
  - `render::confirm(prompt: &str) -> std::io::Result<bool>`

- [ ] **Step 1: Add dependencies**

```bash
cargo add --package filesyncer-cli --path crates/filesyncer-core
cargo add --package filesyncer-cli clap --features derive
```

If `cargo add --path` is rejected from the workspace root, add it by hand to `crates/filesyncer-cli/Cargo.toml`:

```toml
[dependencies]
filesyncer-core = { path = "../filesyncer-core" }
```

- [ ] **Step 2: Write the argument definitions**

`crates/filesyncer-cli/src/args.rs`:

```rust
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(name = "filesyncer", version, about = "Bidirectional directory sync")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Scan, show the plan, and apply it after confirmation.
    Sync {
        /// Profile id.
        profile: String,
        /// Apply without prompting.
        #[arg(long)]
        yes: bool,
        /// Show the plan and stop.
        #[arg(long)]
        dry_run: bool,
        /// Proceed even if the deletion threshold is exceeded.
        #[arg(long)]
        force: bool,
    },
    /// List configured profiles.
    List,
    /// Create a profile and write its root markers.
    Add {
        /// Profile id, used for the state database and root markers.
        id: String,
        /// Display name.
        #[arg(long)]
        name: Option<String>,
        local: PathBuf,
        remote: PathBuf,
    },
    /// Show a profile's paths, last sync time, and tracked path count.
    Status { profile: String },
}
```

- [ ] **Step 3: Write the failing render tests**

`crates/filesyncer-cli/src/render.rs`:

```rust
use filesyncer_core::{
    Action, Conflict, ConflictKind, Plan, RunSummary, Side, Skip, SkipReason,
};

pub fn plan_summary(plan: &Plan) -> String {
    unimplemented!()
}

pub fn run_summary(summary: &RunSummary) -> String {
    unimplemented!()
}

pub fn confirm(prompt: &str) -> std::io::Result<bool> {
    use std::io::Write;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    Ok(matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes"))
}

fn human_bytes(bytes: u64) -> String {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use filesyncer_core::Stat;

    fn copy(rel: &str, from: Side, size: u64) -> Action {
        Action::Copy { rel_path: rel.into(), from, size, hash: "h".into() }
    }

    #[test]
    fn an_empty_plan_says_so_plainly() {
        let text = plan_summary(&Plan::default());
        assert!(text.to_lowercase().contains("nothing to do"), "got: {text}");
    }

    #[test]
    fn copies_are_grouped_by_destination() {
        let plan = Plan {
            actions: vec![copy("a.txt", Side::Local, 1024), copy("b.txt", Side::Remote, 2048)],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("Copy to remote"), "got: {text}");
        assert!(text.contains("Copy to local"), "got: {text}");
        assert!(text.contains("a.txt"), "got: {text}");
    }

    #[test]
    fn deletions_are_labelled_as_going_to_trash() {
        let plan = Plan {
            actions: vec![Action::Trash { rel_path: "gone.txt".into(), side: Side::Remote }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.to_lowercase().contains("trash"), "got: {text}");
        assert!(text.contains("gone.txt"), "got: {text}");
    }

    #[test]
    fn conflicts_name_the_kind_in_words() {
        let plan = Plan {
            conflicts: vec![Conflict {
                rel_path: "notes.md".into(),
                kind: ConflictKind::EditedLocalDeletedRemote,
                local: Some(Stat { size: 1, mtime: 1 }),
                remote: None,
            }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("notes.md"), "got: {text}");
        assert!(
            text.contains("edited locally, deleted on remote"),
            "conflict kinds must be readable; got: {text}"
        );
    }

    #[test]
    fn state_only_actions_are_not_listed_as_work() {
        let plan = Plan {
            actions: vec![Action::DropState { rel_path: "old.txt".into() }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.to_lowercase().contains("nothing to do"), "got: {text}");
    }

    #[test]
    fn skips_are_listed_with_their_reason() {
        let plan = Plan {
            skips: vec![Skip { rel_path: "link.txt".into(), reason: SkipReason::Symlink }],
            ..Default::default()
        };
        let text = plan_summary(&plan);
        assert!(text.contains("link.txt"), "got: {text}");
        assert!(text.to_lowercase().contains("symlink"), "got: {text}");
    }

    #[test]
    fn byte_counts_are_human_readable() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(human_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(human_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn a_run_summary_reports_errors_individually() {
        let summary = RunSummary {
            copied: 2,
            trashed: 1,
            conflicts: 0,
            errors: vec![
                ("a.txt".into(), "permission denied".into()),
                ("b.txt".into(), "no space left".into()),
            ],
            cancelled: false,
        };
        let text = run_summary(&summary);
        assert!(text.contains("a.txt: permission denied"), "got: {text}");
        assert!(text.contains("b.txt: no space left"), "got: {text}");
    }

    #[test]
    fn a_cancelled_run_says_so() {
        let summary = RunSummary { cancelled: true, ..Default::default() };
        assert!(run_summary(&summary).to_lowercase().contains("cancelled"));
    }

    #[test]
    fn conflicts_in_a_summary_tell_the_user_what_to_do() {
        let summary = RunSummary { conflicts: 2, ..Default::default() };
        let text = run_summary(&summary);
        assert!(text.contains('2'), "got: {text}");
        assert!(text.to_lowercase().contains("conflict"), "got: {text}");
    }
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test --package filesyncer-cli`
Expected: FAIL — `not implemented`.

- [ ] **Step 5: Implement the renderers**

```rust
pub fn plan_summary(plan: &Plan) -> String {
    let mut out = String::new();

    let mut section = |title: &str, lines: Vec<String>| {
        if !lines.is_empty() {
            out.push_str(title);
            out.push('\n');
            for line in lines {
                out.push_str("    ");
                out.push_str(&line);
                out.push('\n');
            }
            out.push('\n');
        }
    };

    for (side, label) in [(Side::Remote, "Copy to remote"), (Side::Local, "Copy to local")] {
        let lines: Vec<String> = plan
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Copy { rel_path, from, size, .. } if from.other() == side => {
                    Some(format!("{rel_path}  ({})", human_bytes(*size)))
                }
                _ => None,
            })
            .collect();
        let header = format!(
            "{label}  {} files, {}",
            plan.copy_count(side),
            human_bytes(plan.copy_bytes(side))
        );
        section(&header, lines);
    }

    let trash: Vec<String> = plan
        .actions
        .iter()
        .filter_map(|a| match a {
            Action::Trash { rel_path, side } => Some(format!("{rel_path}  (on {side:?})")),
            _ => None,
        })
        .collect();
    section(&format!("Move to trash  {} files", trash.len()), trash);

    let conflicts: Vec<String> = plan
        .conflicts
        .iter()
        .map(|c| format!("{}  — {}", c.rel_path, describe_conflict(c.kind)))
        .collect();
    section(&format!("Conflicts  {} files", conflicts.len()), conflicts);

    let skips: Vec<String> = plan
        .skips
        .iter()
        .map(|s| match &s.reason {
            SkipReason::Symlink => format!("{}  — symlink", s.rel_path),
            SkipReason::Unreadable(why) => format!("{}  — unreadable: {why}", s.rel_path),
        })
        .collect();
    section(&format!("Skipped  {} entries", skips.len()), skips);

    if plan.is_noop() && plan.conflicts.is_empty() {
        out.push_str("Nothing to do.\n");
    }
    out
}

fn describe_conflict(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothEdited => "edited on both sides",
        ConflictKind::BothNew => "created on both sides with different contents",
        ConflictKind::EditedLocalDeletedRemote => "edited locally, deleted on remote",
        ConflictKind::EditedRemoteDeletedLocal => "edited on remote, deleted locally",
        ConflictKind::TypeMismatch => "file on one side, directory on the other",
    }
}

pub fn run_summary(summary: &RunSummary) -> String {
    let mut out = String::new();
    if summary.cancelled {
        out.push_str("Cancelled.\n");
    }
    out.push_str(&format!(
        "{} copied, {} moved to trash.\n",
        summary.copied, summary.trashed
    ));
    if summary.conflicts > 0 {
        out.push_str(&format!(
            "{} conflict(s) left unresolved; they will be reported again next run.\n",
            summary.conflicts
        ));
    }
    if !summary.errors.is_empty() {
        out.push_str(&format!("{} error(s):\n", summary.errors.len()));
        for (path, message) in &summary.errors {
            out.push_str(&format!("    {path}: {message}\n"));
        }
    }
    out
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}
```

- [ ] **Step 6: Wire the modules into main.rs**

`crates/filesyncer-cli/src/main.rs` — temporary stub so the crate compiles:

```rust
mod args;
mod render;

fn main() {
    let _ = <args::Cli as clap::Parser>::parse();
    eprintln!("not yet implemented");
    std::process::exit(1);
}
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test --package filesyncer-cli`
Expected: 10 passed.

- [ ] **Step 8: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: CLI argument definitions and plan rendering"
```

---

### Task 17: CLI — command execution and exit codes

**Files:**
- Modify: `crates/filesyncer-cli/src/main.rs`

**Interfaces:**
- Consumes: `args::{Cli, Command}`, `render::*`, `filesyncer_core::{Session, Profile, ProfileSet, profile, create_profile_roots}`.
- Produces: the binary. Exit codes `0` clean, `1` error, `2` conflicts present — used by any future systemd timer.

- [ ] **Step 1: Write main.rs**

```rust
mod args;
mod render;

use args::{Cli, Command};
use clap::Parser;
use filesyncer_core::{Profile, ProfileSet, Session, create_profile_roots, profile};

const EXIT_OK: i32 = 0;
const EXIT_ERROR: i32 = 1;
const EXIT_CONFLICTS: i32 = 2;

fn main() {
    let cli = Cli::parse();
    let code = match run(cli) {
        Ok(code) => code,
        Err(message) => {
            eprintln!("error: {message}");
            EXIT_ERROR
        }
    };
    std::process::exit(code);
}

fn run(cli: Cli) -> Result<i32, String> {
    match cli.command {
        Command::List => cmd_list(),
        Command::Add { id, name, local, remote } => cmd_add(id, name, local, remote),
        Command::Status { profile } => cmd_status(&profile),
        Command::Sync { profile, yes, dry_run, force } => cmd_sync(&profile, yes, dry_run, force),
    }
}

fn load_profile(id: &str) -> Result<Profile, String> {
    let path = profile::config_path();
    let set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    set.get(id)
        .cloned()
        .ok_or_else(|| format!("no profile `{id}` in {}", path.display()))
}

fn cmd_list() -> Result<i32, String> {
    let path = profile::config_path();
    let set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    if set.profiles.is_empty() {
        println!("No profiles configured. Add one with `filesyncer add <id> <local> <remote>`.");
        return Ok(EXIT_OK);
    }
    for p in &set.profiles {
        println!("{}  {}", p.id, p.name);
        println!("    local:  {}", p.local.display());
        println!("    remote: {}", p.remote.display());
    }
    Ok(EXIT_OK)
}

fn cmd_add(
    id: String,
    name: Option<String>,
    local: std::path::PathBuf,
    remote: std::path::PathBuf,
) -> Result<i32, String> {
    let path = profile::config_path();
    let mut set = ProfileSet::load(&path).map_err(|e| e.to_string())?;
    if set.get(&id).is_some() {
        return Err(format!("profile `{id}` already exists"));
    }

    let profile = Profile {
        name: name.unwrap_or_else(|| id.clone()),
        id,
        local,
        remote,
    };
    // Validates the roots and stamps both markers; fails before anything is saved.
    create_profile_roots(&profile).map_err(|e| e.to_string())?;

    set.profiles.push(profile.clone());
    set.save(&path).map_err(|e| e.to_string())?;
    println!("Added profile `{}`. Markers written to both roots.", profile.id);
    Ok(EXIT_OK)
}

fn cmd_status(id: &str) -> Result<i32, String> {
    let profile = load_profile(id)?;
    let session = Session::open(&profile).map_err(|e| e.to_string())?;
    println!("{}  {}", profile.id, profile.name);
    println!("    local:  {}", profile.local.display());
    println!("    remote: {}", profile.remote.display());
    match session.last_sync().map_err(|e| e.to_string())? {
        Some(ts) => println!("    last synced: unix {ts}"),
        None => println!("    last synced: never"),
    }
    println!(
        "    tracked paths: {}",
        session.tracked_count().map_err(|e| e.to_string())?
    );
    println!("\n(status does not scan; run `filesyncer sync {id} --dry-run` to see pending work)");
    Ok(EXIT_OK)
}

fn cmd_sync(id: &str, yes: bool, dry_run: bool, force: bool) -> Result<i32, String> {
    let profile = load_profile(id)?;
    let session = Session::open(&profile).map_err(|e| e.to_string())?;
    let plan = session.prepare(force).map_err(|e| e.to_string())?;

    print!("{}", render::plan_summary(&plan));

    let conflicts_present = !plan.conflicts.is_empty();

    if dry_run {
        return Ok(if conflicts_present { EXIT_CONFLICTS } else { EXIT_OK });
    }
    if plan.is_noop() {
        return Ok(if conflicts_present { EXIT_CONFLICTS } else { EXIT_OK });
    }
    if !yes && !render::confirm("Apply this plan?").map_err(|e| e.to_string())? {
        println!("Aborted.");
        return Ok(EXIT_OK);
    }

    let total = std::cell::Cell::new(0usize);
    let summary = session
        .run(
            &plan,
            &|p| match p {
                filesyncer_core::Progress::Started { total: n } => total.set(n),
                filesyncer_core::Progress::Item { index, rel_path, .. } => {
                    println!("[{}/{}] {rel_path}", index + 1, total.get());
                }
                filesyncer_core::Progress::Finished => {}
            },
            &|| false,
        )
        .map_err(|e| e.to_string())?;

    print!("{}", render::run_summary(&summary));

    if !summary.errors.is_empty() {
        return Ok(EXIT_ERROR);
    }
    Ok(if summary.conflicts > 0 { EXIT_CONFLICTS } else { EXIT_OK })
}
```

- [ ] **Step 2: Build and check the help output**

Run: `cargo run --package filesyncer-cli -- --help`
Expected: usage listing `sync`, `list`, `add`, `status`.

- [ ] **Step 3: Exercise the binary end to end by hand**

```bash
mkdir -p /tmp/fs-demo/local /tmp/fs-demo/remote
echo hello > /tmp/fs-demo/local/a.txt
cargo run --package filesyncer-cli -- add demo /tmp/fs-demo/local /tmp/fs-demo/remote
cargo run --package filesyncer-cli -- list
cargo run --package filesyncer-cli -- sync demo --dry-run
cargo run --package filesyncer-cli -- sync demo --yes
```

Expected: the dry run lists `Copy to remote  1 files`; the real run copies it; `/tmp/fs-demo/remote/a.txt` contains `hello`.

- [ ] **Step 4: Verify idempotence and the exit codes**

```bash
cargo run --package filesyncer-cli -- sync demo --dry-run; echo "exit=$?"
```

Expected: `Nothing to do.` and `exit=0`.

```bash
echo local-change > /tmp/fs-demo/local/a.txt
echo remote-change > /tmp/fs-demo/remote/a.txt
cargo run --package filesyncer-cli -- sync demo --yes; echo "exit=$?"
```

Expected: one conflict reported, nothing copied, `exit=2`.

- [ ] **Step 5: Verify the unmounted-share safeguard by hand**

```bash
rm -rf /tmp/fs-demo/remote && mkdir -p /tmp/fs-demo/remote
cargo run --package filesyncer-cli -- sync demo --dry-run; echo "exit=$?"
ls /tmp/fs-demo/local
```

Expected: an error naming `.filesyncer-root` and `exit=1`; the local tree is untouched. This is the single most important manual check in the plan.

- [ ] **Step 6: Clean up and commit**

```bash
rm -rf /tmp/fs-demo
cargo fmt
git add -A
git commit -m "feat: CLI sync, list, add, and status commands"
```

---

### Task 18: GUI — application shell and profile sidebar

**Files:**
- Modify: `crates/filesyncer-gui/Cargo.toml`
- Modify: `crates/filesyncer-gui/src/main.rs`
- Create: `crates/filesyncer-gui/src/window.rs`

**Interfaces:**
- Consumes: `filesyncer_core::{Profile, ProfileSet, profile}`.
- Produces:
  - `window::AppWindow` holding `window: adw::ApplicationWindow`, `content: adw::NavigationPage`, `profiles: Rc<RefCell<ProfileSet>>`
  - `window::build(app: &adw::Application) -> AppWindow`
  - `window::show_content(&self, page: &gtk::Widget, title: &str)` — Tasks 19–21 swap the content pane through this

The GUI has no automated tests, per the spec. Each GUI task therefore ends with explicit manual verification steps. Do not skip them; they are the only verification this crate gets.

- [ ] **Step 1: Add dependencies**

```bash
cargo add --package filesyncer-gui --path crates/filesyncer-core
cargo add --package filesyncer-gui gtk4 --rename gtk
cargo add --package filesyncer-gui libadwaita --rename adw --features v1_5
cargo add --package filesyncer-gui async-channel
```

Verify the system libraries are present before building:

Run: `pkg-config --modversion gtk4 libadwaita-1`
Expected: GTK ≥ 4.10 and libadwaita ≥ 1.5. On this host they are 4.22 and 1.9.

- [ ] **Step 2: Write the window shell**

`crates/filesyncer-gui/src/window.rs`:

```rust
use adw::prelude::*;
use filesyncer_core::{Profile, ProfileSet, profile};
use std::cell::RefCell;
use std::rc::Rc;

pub struct AppWindow {
    pub window: adw::ApplicationWindow,
    pub split: adw::NavigationSplitView,
    pub profiles: Rc<RefCell<ProfileSet>>,
    pub sidebar_list: gtk::ListBox,
}

impl AppWindow {
    /// Replace the content pane. Every later task routes its screens
    /// through here so navigation stays in one place.
    pub fn show_content(&self, child: &impl IsA<gtk::Widget>, title: &str) {
        let toolbar = adw::ToolbarView::new();
        toolbar.add_top_bar(&adw::HeaderBar::new());
        toolbar.set_content(Some(child));

        let page = adw::NavigationPage::new(&toolbar, title);
        self.split.set_content(Some(&page));
    }

    pub fn show_placeholder(&self) {
        let status = adw::StatusPage::builder()
            .icon_name("folder-symbolic")
            .title("No profile selected")
            .description("Choose a profile from the sidebar, or add one.")
            .build();
        self.show_content(&status, "FileSyncer");
    }

    pub fn reload_profiles(&self) {
        let loaded = ProfileSet::load(&profile::config_path()).unwrap_or_default();
        *self.profiles.borrow_mut() = loaded;
        self.rebuild_sidebar();
    }

    fn rebuild_sidebar(&self) {
        while let Some(child) = self.sidebar_list.first_child() {
            self.sidebar_list.remove(&child);
        }
        for p in &self.profiles.borrow().profiles {
            let row = adw::ActionRow::builder()
                .title(&p.name)
                .subtitle(&format!("{}", p.remote.display()))
                .activatable(true)
                .build();
            // The id is not shown, but every handler needs it.
            unsafe { row.set_data("profile-id", p.id.clone()) };
            self.sidebar_list.append(&row);
        }
    }

    pub fn selected_profile(&self, row: &gtk::ListBoxRow) -> Option<Profile> {
        let id: String = unsafe { row.data::<String>("profile-id")?.as_ref().clone() };
        self.profiles.borrow().get(&id).cloned()
    }
}

pub fn build(app: &adw::Application) -> Rc<AppWindow> {
    let sidebar_list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::Single)
        .css_classes(vec!["navigation-sidebar".to_string()])
        .build();

    let add_button = gtk::Button::from_icon_name("list-add-symbolic");
    add_button.set_tooltip_text(Some("Add profile"));

    let sidebar_header = adw::HeaderBar::new();
    sidebar_header.pack_end(&add_button);

    let sidebar_toolbar = adw::ToolbarView::new();
    sidebar_toolbar.add_top_bar(&sidebar_header);
    sidebar_toolbar.set_content(Some(
        &gtk::ScrolledWindow::builder().child(&sidebar_list).build(),
    ));

    let sidebar_page = adw::NavigationPage::new(&sidebar_toolbar, "Profiles");

    let split = adw::NavigationSplitView::builder()
        .sidebar(&sidebar_page)
        .min_sidebar_width(220.0)
        .build();

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("FileSyncer")
        .default_width(900)
        .default_height(600)
        .content(&split)
        .build();

    let app_window = Rc::new(AppWindow {
        window,
        split,
        profiles: Rc::new(RefCell::new(ProfileSet::default())),
        sidebar_list,
    });

    app_window.reload_profiles();
    app_window.show_placeholder();

    // Task 19 replaces this handler body with the profile page.
    {
        let aw = app_window.clone();
        app_window.sidebar_list.connect_row_activated(move |_, row| {
            if let Some(p) = aw.selected_profile(row) {
                let label = gtk::Label::new(Some(&format!("Selected {}", p.name)));
                aw.show_content(&label, &p.name);
            }
        });
    }

    // Task 21 replaces this handler body with the add-profile dialog.
    {
        let aw = app_window.clone();
        add_button.connect_clicked(move |_| {
            let dialog = adw::AlertDialog::new(Some("Not yet implemented"), None);
            dialog.add_response("ok", "OK");
            dialog.present(Some(&aw.window));
        });
    }

    app_window
}
```

`set_data`/`data` are `unsafe` in gtk-rs because they are untyped. The alternative — a full GObject subclass carrying a typed property — is a great deal of ceremony for stashing one string. Keep the unsafe usage confined to these two methods.

- [ ] **Step 2b: Write main.rs**

`crates/filesyncer-gui/src/main.rs`:

```rust
mod window;

use adw::prelude::*;

fn main() -> glib::ExitCode {
    let app = adw::Application::builder()
        .application_id("net.jjbasken.FileSyncer")
        .build();
    app.connect_activate(|app| {
        let w = window::build(app);
        w.window.present();
    });
    app.run()
}
```

Add `glib` to scope by re-exporting from gtk: replace `use adw::prelude::*;` with

```rust
use adw::prelude::*;
use gtk::glib;
```

- [ ] **Step 3: Build**

Run: `cargo build --package filesyncer-gui`
Expected: compiles. Common failures and fixes:
- `no method named set_data` → ensure `use adw::prelude::*` is in scope (it re-exports `ObjectExt`).
- `NavigationSplitView not found` → the `libadwaita` crate needs `--features v1_5`; re-run the `cargo add` from Step 1.

- [ ] **Step 4: Manual verification**

```bash
cargo run --package filesyncer-gui
```

Verify by looking at the window:
1. A two-pane window opens, titled FileSyncer, roughly 900×600.
2. The sidebar is headed "Profiles" with a `+` button top-right.
3. If you created the `demo` profile in Task 17, it appears in the sidebar; otherwise the sidebar is empty.
4. The content pane shows the "No profile selected" status page.
5. Clicking a profile replaces the content pane with "Selected <name>".
6. Clicking `+` shows a "Not yet implemented" dialog.
7. Narrowing the window collapses the split view to a single pane with a back button.

If you have no profiles, create one first:

```bash
mkdir -p /tmp/fs-demo/local /tmp/fs-demo/remote
cargo run --package filesyncer-cli -- add demo /tmp/fs-demo/local /tmp/fs-demo/remote
```

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: GUI application shell with profile sidebar"
```

---

### Task 19: GUI — profile page and plan review

**Files:**
- Create: `crates/filesyncer-gui/src/profile_page.rs`
- Create: `crates/filesyncer-gui/src/plan_view.rs`
- Modify: `crates/filesyncer-gui/src/window.rs`
- Modify: `crates/filesyncer-gui/src/main.rs`

**Interfaces:**
- Consumes: `AppWindow::show_content`, `filesyncer_core::{Session, Plan, Action, Conflict, ConflictKind, Side, Skip, SkipReason}`.
- Produces:
  - `profile_page::show(aw: &Rc<AppWindow>, profile: &Profile)`
  - `plan_view::show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan)`

Scanning runs on the GTK main thread here. At the spec's stated scale — thousands of files, a few GB — a scan is fast, and hashing only touches suspect files. If a scan ever feels slow enough to freeze the window, move it to a worker thread using the same `async_channel` pattern Task 20 establishes for `apply`.

- [ ] **Step 1: Write the profile page**

`crates/filesyncer-gui/src/profile_page.rs`:

```rust
use crate::window::AppWindow;
use crate::plan_view;
use adw::prelude::*;
use filesyncer_core::{Profile, Session};
use std::rc::Rc;

pub fn show(aw: &Rc<AppWindow>, profile: &Profile) {
    let group = adw::PreferencesGroup::builder().title(&profile.name).build();
    group.add(&detail_row("Local", &profile.local.display().to_string()));
    group.add(&detail_row("Remote", &profile.remote.display().to_string()));

    let status_row = adw::ActionRow::builder().title("Last synced").build();
    group.add(&status_row);

    let scan_button = gtk::Button::builder()
        .label("Scan")
        .css_classes(vec!["suggested-action".to_string(), "pill".to_string()])
        .halign(gtk::Align::Center)
        .build();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(24)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&group);
    column.append(&scan_button);

    let clamp = adw::Clamp::builder().child(&column).build();

    // Opening the session verifies both root markers, so a dropped mount
    // surfaces here rather than after the user presses Scan.
    match Session::open(profile) {
        Ok(session) => {
            let subtitle = match session.last_sync() {
                Ok(Some(ts)) => format!("unix {ts}"),
                Ok(None) => "never".to_string(),
                Err(e) => e.to_string(),
            };
            status_row.set_subtitle(&subtitle);

            let session = Rc::new(session);
            let aw2 = aw.clone();
            scan_button.connect_clicked(move |button| {
                button.set_sensitive(false);
                match session.prepare(false) {
                    Ok(plan) => plan_view::show(&aw2, session.clone(), plan),
                    Err(e) => show_error(&aw2, &e.to_string()),
                }
                button.set_sensitive(true);
            });
        }
        Err(e) => {
            scan_button.set_sensitive(false);
            status_row.set_subtitle(&e.to_string());
            status_row.add_css_class("error");
        }
    }

    aw.show_content(&clamp, &profile.name);
}

fn detail_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .css_classes(vec!["property".to_string()])
        .build()
}

pub fn show_error(aw: &Rc<AppWindow>, message: &str) {
    let dialog = adw::AlertDialog::new(Some("Sync cannot continue"), Some(message));
    dialog.add_response("ok", "OK");
    dialog.present(Some(&aw.window));
}
```

- [ ] **Step 2: Write the plan view**

`crates/filesyncer-gui/src/plan_view.rs`:

```rust
use crate::window::AppWindow;
use crate::run_view;
use adw::prelude::*;
use filesyncer_core::{Action, Conflict, ConflictKind, Plan, Session, Side, SkipReason};
use std::cell::RefCell;
use std::rc::Rc;

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let plan = Rc::new(RefCell::new(plan));
    render(aw, session, plan);
}

/// Rebuilt wholesale after each conflict resolution. At the scale this tool
/// targets that is cheaper than surgically updating rows, and it keeps the
/// displayed plan and the real plan impossible to desynchronise.
fn render(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Rc<RefCell<Plan>>) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    {
        let p = plan.borrow();

        for (side, label) in [(Side::Remote, "Copy to remote"), (Side::Local, "Copy to local")] {
            let paths: Vec<String> = p
                .actions
                .iter()
                .filter_map(|a| match a {
                    Action::Copy { rel_path, from, .. } if from.other() == side => {
                        Some(rel_path.clone())
                    }
                    _ => None,
                })
                .collect();
            if !paths.is_empty() {
                column.append(&expander(
                    label,
                    &format!("{} files, {}", paths.len(), human_bytes(p.copy_bytes(side))),
                    paths.iter().map(|s| plain_row(s)).collect(),
                ));
            }
        }

        let trashed: Vec<String> = p
            .actions
            .iter()
            .filter_map(|a| match a {
                Action::Trash { rel_path, side } => {
                    Some(format!("{rel_path} (on {})", side_name(*side)))
                }
                _ => None,
            })
            .collect();
        if !trashed.is_empty() {
            column.append(&expander(
                "Move to trash",
                &format!("{} files", trashed.len()),
                trashed.iter().map(|s| plain_row(s)).collect(),
            ));
        }

        if !p.conflicts.is_empty() {
            let rows: Vec<adw::ActionRow> = p
                .conflicts
                .iter()
                .map(|c| conflict_row(aw, &session, &plan, c))
                .collect();
            let group = expander("Conflicts", &format!("{} files", p.conflicts.len()), rows);
            group.set_expanded(true);
            column.append(&group);
        }

        if !p.skips.is_empty() {
            let rows: Vec<adw::ActionRow> = p
                .skips
                .iter()
                .map(|s| match &s.reason {
                    SkipReason::Symlink => labelled_row(&s.rel_path, "symlink"),
                    SkipReason::Unreadable(why) => labelled_row(&s.rel_path, why),
                })
                .collect();
            column.append(&expander("Skipped", &format!("{} entries", rows.len()), rows));
        }

        if p.is_noop() && p.conflicts.is_empty() {
            column.append(
                &adw::StatusPage::builder()
                    .icon_name("object-select-symbolic")
                    .title("Everything is in sync")
                    .build(),
            );
        }
    }

    let cancel = gtk::Button::with_label("Cancel");
    let apply = gtk::Button::builder()
        .label("Apply")
        .css_classes(vec!["suggested-action".to_string()])
        .build();
    apply.set_sensitive(!plan.borrow().is_noop());

    let buttons = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(12)
        .halign(gtk::Align::End)
        .build();
    buttons.append(&cancel);
    buttons.append(&apply);
    column.append(&buttons);

    {
        let aw = aw.clone();
        cancel.connect_clicked(move |_| aw.show_placeholder());
    }
    {
        let aw = aw.clone();
        let session = session.clone();
        let plan = plan.clone();
        apply.connect_clicked(move |_| {
            run_view::show(&aw, session.clone(), plan.borrow().clone());
        });
    }

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&adw::Clamp::builder().child(&column).build())
        .vexpand(true)
        .build();
    aw.show_content(&scrolled, "Review plan");
}

fn conflict_row(
    aw: &Rc<AppWindow>,
    session: &Rc<Session>,
    plan: &Rc<RefCell<Plan>>,
    conflict: &Conflict,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(&conflict.rel_path)
        .subtitle(describe(conflict.kind))
        .build();

    for (side, label) in [(Side::Local, "Keep local"), (Side::Remote, "Keep remote")] {
        let button = gtk::Button::builder()
            .label(label)
            .valign(gtk::Align::Center)
            .build();
        let aw = aw.clone();
        let session = session.clone();
        let plan = plan.clone();
        let rel_path = conflict.rel_path.clone();
        button.connect_clicked(move |_| {
            // Resolution rewrites the conflict into an ordinary action before
            // apply ever runs; the engine needs no special path for it.
            if plan.borrow_mut().resolve_conflict(&rel_path, side).is_ok() {
                render(&aw, session.clone(), plan.clone());
            }
        });
        row.add_suffix(&button);
    }
    row
}

fn describe(kind: ConflictKind) -> &'static str {
    match kind {
        ConflictKind::BothEdited => "edited on both sides",
        ConflictKind::BothNew => "created on both sides with different contents",
        ConflictKind::EditedLocalDeletedRemote => "edited locally, deleted on remote",
        ConflictKind::EditedRemoteDeletedLocal => "edited on remote, deleted locally",
        ConflictKind::TypeMismatch => "file on one side, directory on the other",
    }
}

fn side_name(side: Side) -> &'static str {
    match side {
        Side::Local => "local",
        Side::Remote => "remote",
    }
}

fn expander(title: &str, subtitle: &str, rows: Vec<adw::ActionRow>) -> adw::ExpanderRow {
    let expander = adw::ExpanderRow::builder().title(title).subtitle(subtitle).build();
    for row in rows {
        expander.add_row(&row);
    }
    let group = adw::PreferencesGroup::new();
    group.add(&expander);
    expander
}

fn plain_row(text: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(text).build()
}

fn labelled_row(title: &str, subtitle: &str) -> adw::ActionRow {
    adw::ActionRow::builder().title(title).subtitle(subtitle).build()
}

fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KB {
        format!("{bytes} B")
    } else if b < KB * KB {
        format!("{:.1} KB", b / KB)
    } else if b < KB * KB * KB {
        format!("{:.1} MB", b / (KB * KB))
    } else {
        format!("{:.1} GB", b / (KB * KB * KB))
    }
}
```

The `expander` helper as written creates a `PreferencesGroup` and returns the expander, which drops the group. Fix it to return the group instead and change the `column.append` calls to take the group; if you prefer, wrap each expander in its own `adw::PreferencesGroup` and return `adw::PreferencesGroup`. Adjust the two `set_expanded` callers accordingly. Make this change before building.

- [ ] **Step 2b: Add `Clone` to `Plan` if it is missing**

`run_view::show` takes an owned `Plan`. `Plan` already derives `Clone` from Task 8; confirm with `cargo build`.

- [ ] **Step 3: Wire the sidebar to the profile page**

In `window.rs`, replace the placeholder `connect_row_activated` body:

```rust
        app_window.sidebar_list.connect_row_activated(move |_, row| {
            if let Some(p) = aw.selected_profile(row) {
                crate::profile_page::show(&aw, &p);
            }
        });
```

In `main.rs`, declare the new modules:

```rust
mod plan_view;
mod profile_page;
mod run_view;
mod window;
```

Create a stub `run_view.rs` so this compiles; Task 20 fills it in:

```rust
use crate::window::AppWindow;
use filesyncer_core::{Plan, Session};
use std::rc::Rc;

pub fn show(_aw: &Rc<AppWindow>, _session: Rc<Session>, _plan: Plan) {
    todo!("Task 20")
}
```

- [ ] **Step 4: Build**

Run: `cargo build --package filesyncer-gui`
Expected: compiles. Fix the `expander` return-type issue flagged in Step 2 if you have not already.

- [ ] **Step 5: Manual verification**

```bash
mkdir -p /tmp/fs-demo/local/docs /tmp/fs-demo/remote
echo alpha > /tmp/fs-demo/local/a.txt
echo beta  > /tmp/fs-demo/local/docs/b.md
cargo run --package filesyncer-cli -- add demo2 /tmp/fs-demo/local /tmp/fs-demo/remote
cargo run --package filesyncer-gui
```

Verify:
1. Clicking `demo2` shows a page with the local path, remote path, and "Last synced: never".
2. Pressing **Scan** shows a "Copy to remote — 2 files" expander listing `a.txt` and `docs/b.md`.
3. **Apply** is enabled; **Cancel** returns to the placeholder.
4. Pressing Apply hits the `todo!()` and panics. That is expected until Task 20.

Now produce a conflict:

```bash
cargo run --package filesyncer-cli -- sync demo2 --yes
echo local-version  > /tmp/fs-demo/local/a.txt
echo remote-version > /tmp/fs-demo/remote/a.txt
cargo run --package filesyncer-gui
```

Verify:
5. Scan shows a **Conflicts — 1 files** group, expanded, with `a.txt` and the subtitle "edited on both sides".
6. The row has **Keep local** and **Keep remote** buttons.
7. Pressing **Keep local** removes the conflict group and adds `a.txt` under "Copy to remote".

Finally, verify the mount safeguard reaches the GUI:

```bash
rm -rf /tmp/fs-demo/remote && mkdir -p /tmp/fs-demo/remote
cargo run --package filesyncer-gui
```

8. Selecting `demo2` shows the marker error in the "Last synced" row and **Scan** is insensitive. Nothing is deleted.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add -A
git commit -m "feat: GUI profile page and plan review with conflict resolution"
```

---

### Task 20: GUI — applying with progress and cancellation

**Files:**
- Modify: `crates/filesyncer-gui/src/run_view.rs`

**Interfaces:**
- Consumes: `AppWindow::show_content`, `filesyncer_core::{Session, Plan, Progress, RunSummary, Profile}`.
- Produces: `run_view::show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan)` — replaces the Task 19 stub.

The threading shape matters and is easy to get wrong. `Session` owns a `rusqlite::Connection`, which is `Send` but not `Sync`, so it cannot be shared across threads behind an `Rc`. The worker therefore opens its **own** `Session` from the profile. The state database is in WAL mode, so a reader on the main thread and the writer on the worker coexist safely.

- [ ] **Step 1: Write the run view**

`crates/filesyncer-gui/src/run_view.rs`:

```rust
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Plan, Progress, RunSummary, Session};
use gtk::glib;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

enum Message {
    Progress(Progress),
    Done(Result<RunSummary, String>),
}

pub fn show(aw: &Rc<AppWindow>, session: Rc<Session>, plan: Plan) {
    let profile = session.profile().clone();

    let heading = gtk::Label::builder()
        .label("Syncing…")
        .css_classes(vec!["title-2".to_string()])
        .build();
    let current = gtk::Label::builder()
        .label("")
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .css_classes(vec!["dim-label".to_string()])
        .build();
    let bar = gtk::ProgressBar::builder().show_text(true).build();
    let cancel = gtk::Button::builder()
        .label("Cancel")
        .halign(gtk::Align::Center)
        .build();

    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .valign(gtk::Align::Center)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();
    column.append(&heading);
    column.append(&bar);
    column.append(&current);
    column.append(&cancel);

    aw.show_content(&adw::Clamp::builder().child(&column).build(), "Syncing");

    let cancelled = Arc::new(AtomicBool::new(false));
    {
        let cancelled = cancelled.clone();
        cancel.connect_clicked(move |button| {
            button.set_sensitive(false);
            button.set_label("Cancelling…");
            cancelled.store(true, Ordering::Relaxed);
        });
    }

    let (sender, receiver) = async_channel::unbounded::<Message>();

    // The worker opens its own Session: rusqlite connections are Send but not
    // Sync, so the main thread's Session cannot cross the boundary.
    {
        let sender = sender.clone();
        let cancelled = cancelled.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<RunSummary, String> {
                let worker_session = Session::open(&profile).map_err(|e| e.to_string())?;
                worker_session
                    .run(
                        &plan,
                        &|p| {
                            let _ = sender.send_blocking(Message::Progress(p));
                        },
                        &|| cancelled.load(Ordering::Relaxed),
                    )
                    .map_err(|e| e.to_string())
            })();
            let _ = sender.send_blocking(Message::Done(result));
        });
    }

    let aw = aw.clone();
    glib::spawn_future_local(async move {
        let mut total = 0usize;
        while let Ok(message) = receiver.recv().await {
            match message {
                Message::Progress(Progress::Started { total: n }) => {
                    total = n;
                    bar.set_fraction(0.0);
                    bar.set_text(Some(&format!("0 / {n}")));
                }
                Message::Progress(Progress::Item { index, rel_path, .. }) => {
                    let done = index + 1;
                    if total > 0 {
                        bar.set_fraction(done as f64 / total as f64);
                    }
                    bar.set_text(Some(&format!("{done} / {total}")));
                    current.set_label(&rel_path);
                }
                Message::Progress(Progress::Finished) => {
                    bar.set_fraction(1.0);
                }
                Message::Done(result) => {
                    show_summary(&aw, result);
                    break;
                }
            }
        }
    });
}

fn show_summary(aw: &Rc<AppWindow>, result: Result<RunSummary, String>) {
    let column = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(18)
        .margin_top(24)
        .margin_bottom(24)
        .margin_start(24)
        .margin_end(24)
        .build();

    match result {
        Err(message) => {
            column.append(
                &adw::StatusPage::builder()
                    .icon_name("dialog-error-symbolic")
                    .title("Sync failed")
                    .description(&message)
                    .build(),
            );
        }
        Ok(summary) => {
            let title = if summary.cancelled {
                "Sync cancelled"
            } else if summary.errors.is_empty() {
                "Sync complete"
            } else {
                "Sync finished with errors"
            };
            column.append(
                &adw::StatusPage::builder()
                    .icon_name(if summary.errors.is_empty() {
                        "object-select-symbolic"
                    } else {
                        "dialog-warning-symbolic"
                    })
                    .title(title)
                    .description(&format!(
                        "{} copied, {} moved to trash.",
                        summary.copied, summary.trashed
                    ))
                    .build(),
            );

            if summary.conflicts > 0 {
                let group = adw::PreferencesGroup::builder()
                    .title("Unresolved conflicts")
                    .description("These will be reported again on the next scan.")
                    .build();
                group.add(
                    &adw::ActionRow::builder()
                        .title(&format!("{} file(s) left untouched", summary.conflicts))
                        .build(),
                );
                column.append(&group);
            }

            // Errors are listed individually rather than collapsed into a count.
            if !summary.errors.is_empty() {
                let group = adw::PreferencesGroup::builder().title("Errors").build();
                for (path, message) in &summary.errors {
                    group.add(
                        &adw::ActionRow::builder().title(path).subtitle(message).build(),
                    );
                }
                column.append(&group);
            }
        }
    }

    let done = gtk::Button::builder()
        .label("Done")
        .halign(gtk::Align::Center)
        .css_classes(vec!["pill".to_string()])
        .build();
    {
        let aw = aw.clone();
        done.connect_clicked(move |_| aw.show_placeholder());
    }
    column.append(&done);

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&adw::Clamp::builder().child(&column).build())
        .vexpand(true)
        .build();
    aw.show_content(&scrolled, "Result");
}
```

- [ ] **Step 2: Build**

Run: `cargo build --package filesyncer-gui`
Expected: compiles. Likely failures:
- "`Rc<Session>` cannot be sent between threads" → you are capturing `session` in the `std::thread::spawn` closure. Only `profile` may cross; re-read Step 1.
- "`Plan` cannot be sent" → `Plan` is plain owned data and is `Send`; check you moved it rather than borrowing it.

- [ ] **Step 3: Manual verification of a normal run**

```bash
rm -rf /tmp/fs-demo && mkdir -p /tmp/fs-demo/local/docs /tmp/fs-demo/remote
for i in $(seq 1 300); do echo "contents $i" > /tmp/fs-demo/local/docs/f$i.txt; done
cargo run --package filesyncer-cli -- add demo3 /tmp/fs-demo/local /tmp/fs-demo/remote
cargo run --package filesyncer-gui
```

Verify:
1. Select `demo3`, press Scan, press Apply.
2. The progress bar advances and shows `n / 300`; the filename label updates.
3. The window stays responsive — drag it while the sync runs.
4. It finishes on "Sync complete — 300 copied, 0 moved to trash."
5. Press **Done**, select `demo3`, Scan again: "Everything is in sync."

- [ ] **Step 4: Manual verification of cancellation**

```bash
rm -rf /tmp/fs-demo/remote/docs
cargo run --package filesyncer-gui
```

Verify:
6. Scan, Apply, then press **Cancel** partway through.
7. The result page says "Sync cancelled" with a partial copy count.
8. Press Done, Scan again: the plan contains exactly the files that had not been copied — not all 300. This is the resumability property; if it re-plans all 300, per-file state commits are broken.

- [ ] **Step 5: Manual verification of error reporting**

```bash
rm -rf /tmp/fs-demo/remote/* 2>/dev/null
chmod 500 /tmp/fs-demo/remote
cargo run --package filesyncer-gui
```

Verify:
9. Scan, Apply. The result is "Sync finished with errors" and the Errors group lists individual paths with "permission denied", not a single collapsed message.

```bash
chmod 700 /tmp/fs-demo/remote
```

- [ ] **Step 6: Commit**

```bash
rm -rf /tmp/fs-demo
cargo fmt
git add -A
git commit -m "feat: GUI apply with threaded progress, cancellation, and summary"
```

---

### Task 21: GUI — add-profile dialog

**Files:**
- Create: `crates/filesyncer-gui/src/profile_dialog.rs`
- Modify: `crates/filesyncer-gui/src/window.rs`
- Modify: `crates/filesyncer-gui/src/main.rs`

**Interfaces:**
- Consumes: `AppWindow::{window, reload_profiles}`, `filesyncer_core::{Profile, ProfileSet, profile, create_profile_roots}`.
- Produces: `profile_dialog::present(aw: &Rc<AppWindow>)`.

Saving writes the root markers, which is also where validation errors — a missing directory, or nested roots — reach the user.

- [ ] **Step 1: Write the dialog**

`crates/filesyncer-gui/src/profile_dialog.rs`:

```rust
use crate::window::AppWindow;
use adw::prelude::*;
use filesyncer_core::{Profile, ProfileSet, create_profile_roots, profile};
use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

pub fn present(aw: &Rc<AppWindow>) {
    let name_row = adw::EntryRow::builder().title("Name").build();
    let id_row = adw::EntryRow::builder()
        .title("Id (used for the state database and root markers)")
        .build();

    let local = Rc::new(RefCell::new(None::<PathBuf>));
    let remote = Rc::new(RefCell::new(None::<PathBuf>));

    let local_row = adw::ActionRow::builder()
        .title("Local folder")
        .subtitle("Not chosen")
        .build();
    let remote_row = adw::ActionRow::builder()
        .title("Remote folder")
        .subtitle("Not chosen")
        .build();

    let group = adw::PreferencesGroup::new();
    group.add(&name_row);
    group.add(&id_row);
    group.add(&local_row);
    group.add(&remote_row);

    let dialog = adw::AlertDialog::new(Some("Add profile"), None);
    dialog.set_extra_child(Some(&group));
    dialog.add_response("cancel", "Cancel");
    dialog.add_response("add", "Add");
    dialog.set_response_appearance("add", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("add"));

    attach_chooser(&local_row, &local, "Choose local folder", aw);
    attach_chooser(&remote_row, &remote, "Choose remote folder", aw);

    {
        let aw = aw.clone();
        let local = local.clone();
        let remote = remote.clone();
        dialog.connect_response(None, move |dialog, response| {
            if response != "add" {
                return;
            }
            match build_and_save(&id_row.text(), &name_row.text(), &local, &remote) {
                Ok(id) => {
                    aw.reload_profiles();
                    let ok = adw::AlertDialog::new(
                        Some("Profile added"),
                        Some(&format!("Markers written to both roots for `{id}`.")),
                    );
                    ok.add_response("ok", "OK");
                    ok.present(Some(&aw.window));
                }
                Err(message) => {
                    // Keep the dialog's data by reporting the failure separately.
                    let err = adw::AlertDialog::new(Some("Could not add profile"), Some(&message));
                    err.add_response("ok", "OK");
                    err.present(Some(&aw.window));
                }
            }
            let _ = dialog;
        });
    }

    dialog.present(Some(&aw.window));
}

fn attach_chooser(
    row: &adw::ActionRow,
    slot: &Rc<RefCell<Option<PathBuf>>>,
    title: &str,
    aw: &Rc<AppWindow>,
) {
    let button = gtk::Button::builder()
        .icon_name("folder-open-symbolic")
        .valign(gtk::Align::Center)
        .build();
    row.add_suffix(&button);

    let slot = slot.clone();
    let row = row.clone();
    let window = aw.window.clone();
    let title = title.to_string();
    button.connect_clicked(move |_| {
        let chooser = gtk::FileDialog::builder().title(&title).build();
        let slot = slot.clone();
        let row = row.clone();
        chooser.select_folder(
            Some(&window),
            None::<&gtk::gio::Cancellable>,
            move |result| {
                if let Ok(file) = result {
                    if let Some(path) = file.path() {
                        row.set_subtitle(&path.display().to_string());
                        *slot.borrow_mut() = Some(path);
                    }
                }
            },
        );
    });
}

fn build_and_save(
    id: &str,
    name: &str,
    local: &Rc<RefCell<Option<PathBuf>>>,
    remote: &Rc<RefCell<Option<PathBuf>>>,
) -> Result<String, String> {
    let id = id.trim();
    if id.is_empty() {
        return Err("An id is required.".into());
    }
    if !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("Ids may contain only letters, digits, hyphens, and underscores.".into());
    }
    let local = local.borrow().clone().ok_or("Choose a local folder.")?;
    let remote = remote.borrow().clone().ok_or("Choose a remote folder.")?;

    let config = profile::config_path();
    let mut set = ProfileSet::load(&config).map_err(|e| e.to_string())?;
    if set.get(id).is_some() {
        return Err(format!("A profile with id `{id}` already exists."));
    }

    let new_profile = Profile {
        id: id.to_string(),
        name: if name.trim().is_empty() { id.to_string() } else { name.trim().to_string() },
        local,
        remote,
    };

    // Validates the roots (existence, nesting) and stamps the markers.
    // Nothing is written to the config file unless this succeeds.
    create_profile_roots(&new_profile).map_err(|e| e.to_string())?;

    set.profiles.push(new_profile);
    set.save(&config).map_err(|e| e.to_string())?;
    Ok(id.to_string())
}
```

- [ ] **Step 2: Wire the `+` button**

In `window.rs`, replace the placeholder `add_button.connect_clicked` body:

```rust
        add_button.connect_clicked(move |_| {
            crate::profile_dialog::present(&aw);
        });
```

In `main.rs`, add `mod profile_dialog;`.

- [ ] **Step 3: Build**

Run: `cargo build --package filesyncer-gui`
Expected: compiles. If `gtk::gio` is not in scope, add `use gtk::gio;` and use `None::<&gio::Cancellable>`.

- [ ] **Step 4: Manual verification**

```bash
rm -rf /tmp/fs-demo && mkdir -p /tmp/fs-demo/local /tmp/fs-demo/remote /tmp/fs-demo/local/inner
echo hello > /tmp/fs-demo/local/a.txt
cargo run --package filesyncer-gui
```

Verify:
1. Press `+`. The dialog shows Name, Id, and two folder rows reading "Not chosen".
2. Press **Add** with everything blank: "An id is required."
3. Enter id `gui-demo`, press Add: "Choose a local folder."
4. Choose `/tmp/fs-demo/local` and `/tmp/fs-demo/remote`, press Add: "Profile added."
5. The sidebar now lists the profile without restarting the app.
6. `ls -a /tmp/fs-demo/local /tmp/fs-demo/remote` shows `.filesyncer-root` in both, each containing `gui-demo`.
7. Press `+` again, reuse id `gui-demo`: "A profile with id `gui-demo` already exists."
8. Press `+`, id `nested`, local `/tmp/fs-demo/local`, remote `/tmp/fs-demo/local/inner`: rejected with a message containing "contains". No marker is written into `inner`, and nothing is added to the config file.
9. Press `+`, id `bad id!`: rejected for the invalid character.

- [ ] **Step 5: Full-workflow verification**

Select `gui-demo`, Scan, Apply. Confirm `a.txt` reaches `/tmp/fs-demo/remote`. Then:

```bash
cargo run --package filesyncer-cli -- status gui-demo
```

Verify the CLI reports the same profile, a non-`never` last sync time, and 1 tracked path — proving both frontends share one engine and one state database.

- [ ] **Step 6: Final check and commit**

```bash
rm -rf /tmp/fs-demo
cargo fmt
cargo clippy --workspace --all-targets
cargo test --workspace
```

Expected: tests all green. Address any clippy warnings that indicate real problems; `#[allow]` anything that is merely stylistic disagreement.

```bash
git add -A
git commit -m "feat: GUI add-profile dialog with validation"
```

---

## Plan Self-Review

Checked after writing, against `docs/superpowers/specs/2026-07-26-filesyncer-design.md`.

**Spec coverage**

| Spec section | Covered by |
|---|---|
| Workspace, three crates, no GTK in core | Task 1; Global Constraints |
| Scan/plan/apply split, progress callback | Tasks 6, 9–10, 13, 14 |
| Two-stage change detection (stat then hash) | Task 9 (`mtime_drift_without_content_change…`) |
| Full decision table, all 14 rows | Tasks 9 and 10 |
| File-vs-directory is a conflict | Task 9 |
| Directories untracked, created on demand | Tasks 6, 12 |
| Symlinks skipped and reported | Tasks 6, 9, 15 |
| Root markers | Tasks 5, 14, 17 Step 5, 19 Step 5 |
| Deletion threshold, 25%, `--force` | Task 11, Task 14 |
| Marker repair is manual, never automatic | Task 5 |
| Conflicts flagged, resurface every run | Tasks 13, 15 |
| Conflict resolution rewrites into a copy | Tasks 11, 19 |
| Trash layout with run timestamp | Tasks 12, 13 |
| Profiles TOML, id vs name | Tasks 3, 16, 21 |
| State DB per profile, XDG split | Tasks 3, 7 |
| Deleting state DB re-baselines safely | Task 15 |
| `.syncignore`, gitignore semantics, unioned | Tasks 6, 14 |
| Reserved paths always excluded | Task 6 |
| Profile validation incl. nesting | Task 4 |
| CLI commands and exit codes | Tasks 16, 17 |
| `status` does not scan | Task 17 |
| GUI split view, plan groups, conflict rows | Tasks 18, 19 |
| GUI progress, working cancel, per-file errors | Task 20 |
| Add-profile dialog writes markers | Task 21 |
| Per-file errors don't abort; row untouched | Task 13 |
| Atomic copy via temp + rename + mtime | Task 12 |
| Stale temp cleanup | Tasks 12, 13 |
| Testing strategy incl. idempotence | Task 15 |
| No GUI automated tests | Tasks 18–21 use manual steps |

No gaps found.

**Placeholder scan:** The only `todo!()` is the deliberate `run_view` stub in Task 19, replaced in Task 20 and called out as such. `unimplemented!()` bodies are the TDD red phase and are always replaced within the same task.

**Type consistency:** `Side::other()`, `Plan::copy_count(to:)`, `Action::Copy { hash }`, `Stat`, `Progress::{Started,Item,Finished}`, `RunSummary` fields, and `Session::{open, open_with_state_path, prepare, run}` are used identically everywhere they appear. Two things worth flagging for the implementer:

- Task 11 introduces `Copy { hash: String::new() }` for resolved conflicts, and Task 13 must hash at copy time. Both sides are tested.
- Task 19's `expander()` helper as written drops the `PreferencesGroup` it creates. The step says to fix it before building rather than silently shipping a widget that never appears.
