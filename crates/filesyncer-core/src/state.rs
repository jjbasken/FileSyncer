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

#[derive(Debug)]
pub struct StateStore {
    conn: rusqlite::Connection,
}

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
        assert_eq!(
            all.keys().cloned().collect::<Vec<_>>(),
            vec!["b.txt".to_string()]
        );
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
        assert!(
            s.load_all()
                .unwrap()
                .contains_key("dossiers/été 2026/note ✓.md")
        );
    }
}
