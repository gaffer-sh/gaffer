//! SQLite persistence for the import graph.
//!
//! On first run, the cache is empty and `incremental_update` walks the
//! whole project, extracts edges per file, and writes everything to
//! `.gaffer/graph.db`. On subsequent runs, only files whose mtime has
//! changed are re-extracted; unchanged files reuse their cached edges.
//! Files present in the cache but missing from the walk are deleted
//! (their edges go too — this is critical for correctness, not just
//! hygiene, since stale edges from a deleted file would produce false
//! positives in reverse-reachability).
//!
//! ## Schema
//!
//! ```sql
//! CREATE TABLE files (
//!     path TEXT PRIMARY KEY,           -- project-relative
//!     mtime_ns INTEGER NOT NULL,       -- file modification time
//!     scanned_at INTEGER NOT NULL      -- when we last extracted (audit)
//! );
//! CREATE TABLE edges (
//!     from_path TEXT NOT NULL,
//!     to_path TEXT NOT NULL,
//!     PRIMARY KEY (from_path, to_path),
//!     FOREIGN KEY (from_path) REFERENCES files(path) ON DELETE CASCADE
//! );
//! CREATE INDEX edges_to_idx ON edges(to_path);
//! ```
//!
//! Schema version is tracked via SQLite's built-in `PRAGMA user_version`.
//! On version mismatch we drop and recreate — cheap since we can rebuild
//! from scratch.
//!
//! ## Failure handling
//!
//! Cache operations can fail (permission denied, corrupt DB, full disk).
//! The public API returns `Result`; the CLI wrapper logs a warning to
//! stderr and falls back to an in-memory build, so users never see a
//! cache error block their command.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection};

const SCHEMA_VERSION: u32 = 1;

/// SQLite-backed graph cache.
pub struct GraphCache {
    conn: Connection,
}

impl GraphCache {
    /// Open the cache at `path`, creating + migrating the schema if needed.
    /// Caller is responsible for ensuring the parent directory exists.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            // Best-effort create. If this fails (read-only FS, EACCES) the
            // SQLite open below will return a clearer error like
            // "unable to open database file" — but log here so the original
            // cause isn't masked by the downstream symptom.
            if let Err(e) = fs::create_dir_all(parent) {
                eprintln!(
                    "[gaffer] graph cache: could not create {}: {}",
                    parent.display(),
                    e,
                );
            }
        }

        let conn = Connection::open(path)?;
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;",
        )?;

        let cache = Self { conn };
        cache.migrate()?;
        Ok(cache)
    }

    fn migrate(&self) -> rusqlite::Result<()> {
        let current_version: u32 =
            self.conn
                .query_row("PRAGMA user_version;", [], |row| row.get(0))?;

        if current_version == SCHEMA_VERSION {
            return Ok(());
        }

        // Visible distinguish between "first run" (current_version=0, no
        // existing tables) and "existing cache nuked because schema changed".
        // The latter means the next call will pay full-rebuild cost; users
        // should see *why* their incremental run got slow.
        if current_version != 0 {
            eprintln!(
                "[gaffer] graph cache: schema v{} → v{}, rebuilding from scratch",
                current_version, SCHEMA_VERSION,
            );
        }

        // Schema mismatch (or fresh DB). Drop everything and rebuild.
        // Rebuild cost on a fresh repo is the full graph build (~0.65s on
        // this monorepo); cheaper than maintaining migration paths for v0.1.
        // Note: SCHEMA_VERSION should bump whenever the *set of extractors*
        // changes (adding a language, changing resolution semantics), not
        // just when SQL changes — otherwise old caches contain a partial
        // graph relative to what the current code would produce.
        self.conn.execute_batch(
            "DROP TABLE IF EXISTS edges;
             DROP TABLE IF EXISTS files;
             CREATE TABLE files (
                 path TEXT PRIMARY KEY,
                 mtime_ns INTEGER NOT NULL,
                 scanned_at INTEGER NOT NULL
             );
             CREATE TABLE edges (
                 from_path TEXT NOT NULL,
                 to_path TEXT NOT NULL,
                 PRIMARY KEY (from_path, to_path),
                 FOREIGN KEY (from_path) REFERENCES files(path) ON DELETE CASCADE
             );
             CREATE INDEX edges_to_idx ON edges(to_path);",
        )?;
        self.conn
            .pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(())
    }

    /// Snapshot of cached file metadata: `path → mtime_ns`. Used by the
    /// incremental walker to decide which files need re-extraction.
    pub fn list_files(&self) -> rusqlite::Result<HashMap<String, i64>> {
        let mut stmt = self.conn.prepare("SELECT path, mtime_ns FROM files")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut out = HashMap::new();
        for r in rows {
            let (p, m) = r?;
            out.insert(p, m);
        }
        Ok(out)
    }

    /// Edges originating at the given file. For incremental updates: when
    /// a file's mtime is unchanged, we skip re-extraction and instead
    /// load these edges directly into the graph being built.
    pub fn edges_from(&self, from: &str) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT to_path FROM edges WHERE from_path = ?1")?;
        let rows = stmt.query_map(params![from], |row| row.get::<_, String>(0))?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// Replace all edges from `from_path` with `to_paths` and update the
    /// file's mtime to `mtime_ns`. Atomic via transaction so a partial
    /// write can't leave the cache inconsistent.
    pub fn replace_file_edges(
        &mut self,
        from_path: &str,
        mtime_ns: i64,
        to_paths: &[String],
    ) -> rusqlite::Result<()> {
        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO files (path, mtime_ns, scanned_at) VALUES (?1, ?2, ?3)
             ON CONFLICT(path) DO UPDATE SET mtime_ns = excluded.mtime_ns,
                                            scanned_at = excluded.scanned_at",
            params![from_path, mtime_ns, now_ns()],
        )?;
        tx.execute("DELETE FROM edges WHERE from_path = ?1", params![from_path])?;
        {
            // OR IGNORE because the same target can resolve from multiple
            // specs in one file (e.g. Go's resolve_many fans out a package
            // import to every .go file, and two different imports may
            // overlap on the same target). Deduping at the SQL layer
            // means callers don't have to.
            let mut insert = tx.prepare(
                "INSERT OR IGNORE INTO edges (from_path, to_path) VALUES (?1, ?2)",
            )?;
            for to in to_paths {
                insert.execute(params![from_path, to])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Delete a file and all its outgoing edges (cascaded by the FK).
    /// Used when the walker finds a path in the cache that no longer
    /// exists in the working tree.
    pub fn delete_files(&mut self, paths: &HashSet<String>) -> rusqlite::Result<()> {
        if paths.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare("DELETE FROM files WHERE path = ?1")?;
            for p in paths {
                stmt.execute(params![p])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn now_ns() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Convert a `SystemTime` (file mtime) to nanoseconds since Unix epoch.
/// Returns 0 for times before the epoch (won't happen in practice).
pub fn mtime_ns(mtime: SystemTime) -> i64 {
    mtime
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

/// Convert a project-relative `Path` to the canonical String key the
/// cache uses. Centralized so the walker and the cache agree on
/// encoding (forward slashes, no leading `./`).
pub fn path_to_key(p: &Path) -> String {
    p.to_string_lossy().replace('\\', "/")
}

/// Inverse: parse a cache key back to PathBuf.
pub fn key_to_path(s: &str) -> PathBuf {
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn roundtrip_open_and_replace_edges() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("graph.db");
        let mut cache = GraphCache::open(&path).unwrap();

        cache
            .replace_file_edges(
                "src/a.ts",
                1234567890,
                &["src/b.ts".to_string(), "src/c.ts".to_string()],
            )
            .unwrap();

        let edges = cache.edges_from("src/a.ts").unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.contains(&"src/b.ts".to_string()));
        assert!(edges.contains(&"src/c.ts".to_string()));

        let files = cache.list_files().unwrap();
        assert_eq!(files.get("src/a.ts"), Some(&1234567890));
    }

    #[test]
    fn replace_overwrites_old_edges() {
        let dir = tempdir().unwrap();
        let mut cache = GraphCache::open(&dir.path().join("g.db")).unwrap();

        cache
            .replace_file_edges("a", 1, &["b".to_string(), "c".to_string()])
            .unwrap();
        cache
            .replace_file_edges("a", 2, &["d".to_string()])
            .unwrap();

        let edges = cache.edges_from("a").unwrap();
        assert_eq!(edges, vec!["d".to_string()]);
        let files = cache.list_files().unwrap();
        assert_eq!(files.get("a"), Some(&2));
    }

    #[test]
    fn delete_files_cascades_edges() {
        let dir = tempdir().unwrap();
        let mut cache = GraphCache::open(&dir.path().join("g.db")).unwrap();

        cache
            .replace_file_edges("a", 1, &["b".to_string(), "c".to_string()])
            .unwrap();
        cache
            .replace_file_edges("x", 1, &["y".to_string()])
            .unwrap();

        let mut to_delete = HashSet::new();
        to_delete.insert("a".to_string());
        cache.delete_files(&to_delete).unwrap();

        assert!(cache.edges_from("a").unwrap().is_empty());
        // Untouched file's edges survive.
        assert_eq!(cache.edges_from("x").unwrap(), vec!["y".to_string()]);
        assert!(!cache.list_files().unwrap().contains_key("a"));
    }

    #[test]
    fn open_creates_parent_directory() {
        let dir = tempdir().unwrap();
        let nested = dir.path().join(".gaffer/sub/graph.db");
        let cache = GraphCache::open(&nested).unwrap();
        // Should not panic; basic query should work.
        assert!(cache.list_files().unwrap().is_empty());
    }

    #[test]
    fn schema_version_is_tracked() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("g.db");
        // First open creates the schema.
        {
            let _cache = GraphCache::open(&path).unwrap();
        }
        // Second open should be a no-op for migration (version matches).
        {
            let cache = GraphCache::open(&path).unwrap();
            let v: u32 = cache
                .conn
                .query_row("PRAGMA user_version;", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, SCHEMA_VERSION);
        }
    }

    #[test]
    fn schema_mismatch_drops_and_recreates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("g.db");
        // Manually create a DB with a stale schema version.
        {
            let conn = Connection::open(&path).unwrap();
            conn.pragma_update(None, "user_version", 999u32).unwrap();
            conn.execute("CREATE TABLE old_table (x INTEGER)", []).unwrap();
        }
        // Opening via GraphCache should reset to current schema.
        let cache = GraphCache::open(&path).unwrap();
        let v: u32 = cache
            .conn
            .query_row("PRAGMA user_version;", [], |r| r.get(0))
            .unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        // The old table should be gone after rebuild — but we don't drop
        // arbitrary tables, only known ones. Just verify the new tables exist.
        assert!(cache.list_files().unwrap().is_empty());
    }
}
