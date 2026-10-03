//! HistoryStore: local SQLite, file mode 0600.
use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::path::Path;

#[derive(Serialize, Clone, Debug, PartialEq)]
pub struct Entry {
    pub id: i64,
    pub created_at: i64,
    pub raw: String,
    pub cleaned: Option<String>,
    pub mode: String,
    pub stt_model: String,
    pub cleanup_model: Option<String>,
    pub error: Option<String>,
    pub duration_ms: i64,
    pub latency_ms: i64,
}

pub struct HistoryStore {
    connection: Connection,
}

impl HistoryStore {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        crate::settings::private_file(path)?;
        connection.execute_batch(
            "PRAGMA journal_mode=WAL;
             CREATE TABLE IF NOT EXISTS history (
               id INTEGER PRIMARY KEY,
               created_at INTEGER NOT NULL,
               raw TEXT NOT NULL,
               cleaned TEXT,
               mode TEXT NOT NULL,
               stt_model TEXT NOT NULL,
               cleanup_model TEXT,
               error TEXT,
               duration_ms INTEGER NOT NULL DEFAULT 0,
               latency_ms INTEGER NOT NULL DEFAULT 0
             );
             CREATE INDEX IF NOT EXISTS history_created ON history(created_at);",
        )?;
        Ok(Self { connection })
    }

    pub fn add(&self, entry: &Entry) -> Result<i64> {
        self.connection.execute(
            "INSERT INTO history (created_at, raw, cleaned, mode, stt_model, cleanup_model, error, duration_ms, latency_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                entry.created_at,
                entry.raw,
                entry.cleaned,
                entry.mode,
                entry.stt_model,
                entry.cleanup_model,
                entry.error,
                entry.duration_ms,
                entry.latency_ms
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn list(&self, limit: u32) -> Result<Vec<Entry>> {
        let mut statement = self.connection.prepare(
            "SELECT id, created_at, raw, cleaned, mode, stt_model, cleanup_model, error, duration_ms, latency_ms
             FROM history ORDER BY created_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = statement.query_map([limit], |row| {
            Ok(Entry {
                id: row.get(0)?,
                created_at: row.get(1)?,
                raw: row.get(2)?,
                cleaned: row.get(3)?,
                mode: row.get(4)?,
                stt_model: row.get(5)?,
                cleanup_model: row.get(6)?,
                error: row.get(7)?,
                duration_ms: row.get(8)?,
                latency_ms: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn delete(&self, id: i64) -> Result<()> {
        self.connection
            .execute("DELETE FROM history WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn clear(&self) -> Result<()> {
        self.connection.execute("DELETE FROM history", [])?;
        self.connection.execute_batch("VACUUM")?;
        Ok(())
    }

    /// Deletes entries older than `days` (0 keeps everything).
    pub fn prune(&self, days: u32, now: i64) -> Result<usize> {
        if days == 0 {
            return Ok(0);
        }
        let cutoff = now - days as i64 * 86_400_000;
        Ok(self
            .connection
            .execute("DELETE FROM history WHERE created_at < ?1", [cutoff])?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn entry(created_at: i64) -> Entry {
        Entry {
            id: 0,
            created_at,
            raw: "raw".into(),
            cleaned: Some("Clean.".into()),
            mode: "clean".into(),
            stt_model: "m".into(),
            cleanup_model: None,
            error: None,
            duration_ms: 1,
            latency_ms: 2,
        }
    }

    #[test]
    fn add_list_delete_prune_clear() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("h.db");
        let store = HistoryStore::open(&path).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let old = store.add(&entry(0)).unwrap();
        let new = store.add(&entry(100 * 86_400_000)).unwrap();
        let list = store.list(10).unwrap();
        assert_eq!(
            list.iter().map(|e| e.id).collect::<Vec<_>>(),
            vec![new, old]
        );
        assert_eq!(store.prune(30, 100 * 86_400_000).unwrap(), 1);
        assert_eq!(store.prune(0, i64::MAX).unwrap(), 0);
        store.delete(new).unwrap();
        assert!(store.list(10).unwrap().is_empty());
        store.add(&entry(5)).unwrap();
        store.clear().unwrap();
        assert!(store.list(10).unwrap().is_empty());
    }
}
