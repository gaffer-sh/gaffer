//! SQLite database layer for local test result storage.
//!
//! ## Schema
//! The schema is defined in `migrations/001_init.sql` and embedded at compile time.
//! Tables: test_runs, test_executions, coverage_reports, pending_uploads.
//! Uses WAL mode for concurrent read access (e.g. future MCP server reads while
//! reporter writes).
//!
//! ## Testing pattern
//! Tests use `tempfile::TempDir` for isolated databases. The TempDir must be kept alive
//! (held in a variable) for the duration of the test — dropping it deletes the directory.
//! See `test_db()` helper.
//!
//! ## Adding new queries
//! - Add the method to `impl Database`
//! - Add a test that exercises it (including edge cases like empty results)
//! - If it's a query used by intelligence algorithms, add it near the
//!   existing query methods and document which algorithm consumes it

use rusqlite::{params, Connection};
use std::path::Path;

use crate::types::{
    CoverageSummary, FailureSearchResult, PendingUpload, RecentRun, RunMetadata, RunSummary,
    TestEvent, TestHistoryEntry,
};

// Internal types for intelligence queries — these don't cross the NAPI boundary.

/// A single test execution from historical runs, used by flaky detection and duration analytics.
#[derive(Debug, Clone)]
pub struct HistoricalTest {
    pub name: String,
    pub status: String,
    pub duration_ms: f64,
    pub file_path: String,
    pub run_id: String,
    pub started_at: String,
}

/// A failed test from the current run, used by failure clustering.
#[derive(Debug, Clone)]
pub struct FailedTest {
    pub name: String,
    pub file_path: String,
    pub error: String,
}

const MIGRATION_001: &str = include_str!("migrations/001_init.sql");
const MIGRATION_002: &str = include_str!("migrations/002_coverage_columns.sql");

/// Escape SQL LIKE wildcards (`%`, `_`, `\`) in user input so they are matched literally.
fn escape_like(input: &str) -> String {
    input
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

pub struct Database {
    conn: Connection,
}

impl Database {
    /// Open or create the SQLite database at the given path.
    /// Creates parent directories if they don't exist (e.g. `.gaffer/`).
    /// Runs migrations on every open — they're idempotent (CREATE IF NOT EXISTS).
    pub fn open(db_path: &Path) -> Result<Self, rusqlite::Error> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| {
                rusqlite::Error::SqliteFailure(
                    rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_CANTOPEN),
                    Some(format!("Failed to create directory '{}': {}", parent.display(), e)),
                )
            })?;
        }

        let conn = Connection::open(db_path)?;

        // WAL mode allows concurrent readers (future: MCP server) + single writer (reporter)
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // CASCADE deletes require foreign_keys to be ON (SQLite has it off by default)
        conn.pragma_update(None, "foreign_keys", "ON")?;

        let db = Database { conn };
        db.migrate()?;
        Ok(db)
    }

    fn migrate(&self) -> Result<(), rusqlite::Error> {
        // Always run 001 — it uses CREATE IF NOT EXISTS so it's idempotent
        self.conn.execute_batch(MIGRATION_001)?;

        // Run 002 only if not already applied. ALTER TABLE ADD COLUMN for
        // already-existing columns would error, so check schema_version first.
        let version: i32 = match self.conn.query_row(
            "SELECT COALESCE(MAX(version), 0) FROM schema_version",
            [],
            |row| row.get(0),
        ) {
            Ok(v) => v,
            Err(e) => {
                let msg = e.to_string();
                if msg.contains("no such table") {
                    0
                } else {
                    return Err(e);
                }
            }
        };

        if version < 2 {
            // Execute each ALTER individually — ignore "duplicate column" errors
            // that occur when 001 already included the columns (fresh install).
            for line in MIGRATION_002.lines() {
                let line = line.trim();
                if line.is_empty() || line.starts_with("--") {
                    continue;
                }
                match self.conn.execute(line, []) {
                    Ok(_) => {}
                    Err(e) => {
                        let msg = e.to_string();
                        if !msg.contains("duplicate column name") {
                            return Err(e);
                        }
                    }
                }
            }
        }

        Ok(())
    }

    /// Insert a new test run in "running" state. Called by GafferCore::start_run.
    pub fn insert_run(
        &self,
        run_id: &str,
        metadata: &RunMetadata,
        started_at: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO test_runs (id, branch, commit_sha, ci_provider, framework, started_at, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'running')",
            params![
                run_id,
                metadata.branch,
                metadata.commit,
                metadata.ci_provider,
                metadata.framework,
                started_at,
            ],
        )?;
        Ok(())
    }

    /// Insert a single test execution result. Called once per test by GafferCore::record_test.
    /// Optional fields (retry_count, flaky) default to 0/false when None.
    pub fn insert_test(&self, run_id: &str, test: &TestEvent) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO test_executions (run_id, name, status, duration_ms, file_path, error_message, retry_count, flaky)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run_id,
                test.name,
                test.status,
                test.duration,
                test.file_path,
                test.error,
                test.retry_count.unwrap_or(0),
                test.flaky.unwrap_or(false),
            ],
        )?;
        Ok(())
    }

    /// Transition a run from "running" to "finished" and persist the summary stats.
    /// Called by GafferCore::end_run.
    pub fn finish_run(
        &self,
        run_id: &str,
        summary: &RunSummary,
        finished_at: &str,
    ) -> Result<(), rusqlite::Error> {
        let rows_affected = self.conn.execute(
            "UPDATE test_runs SET
                status = 'finished',
                total = ?2,
                passed = ?3,
                failed = ?4,
                skipped = ?5,
                duration_ms = ?6,
                finished_at = ?7
             WHERE id = ?1 AND status = 'running'",
            params![
                run_id,
                summary.total,
                summary.passed,
                summary.failed,
                summary.skipped,
                summary.duration,
                finished_at,
            ],
        )?;
        if rows_affected == 0 {
            return Err(rusqlite::Error::QueryReturnedNoRows);
        }
        Ok(())
    }

    /// Update the framework field for a run (detected from parsed reports).
    pub fn update_run_framework(&self, run_id: &str, framework: &str) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE test_runs SET framework = ?2 WHERE id = ?1",
            params![run_id, framework],
        )?;
        Ok(())
    }

    /// Read back the started_at timestamp for a run (needed for RunReport).
    pub fn get_run_started_at(&self, run_id: &str) -> Result<String, rusqlite::Error> {
        self.conn.query_row(
            "SELECT started_at FROM test_runs WHERE id = ?1",
            params![run_id],
            |row| row.get(0),
        )
    }

    pub fn get_run_count(&self) -> Result<i64, rusqlite::Error> {
        self.conn
            .query_row("SELECT COUNT(*) FROM test_runs", [], |row| row.get(0))
    }

    pub fn get_test_count(&self, run_id: &str) -> Result<i64, rusqlite::Error> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM test_executions WHERE run_id = ?1",
            params![run_id],
            |row| row.get(0),
        )
    }

    pub fn get_test_count_by_status(
        &self,
        run_id: &str,
        status: &str,
    ) -> Result<i64, rusqlite::Error> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM test_executions WHERE run_id = ?1 AND status = ?2",
            params![run_id, status],
            |row| row.get(0),
        )
    }

    // -----------------------------------------------------------------------
    // Intelligence queries — consumed by intel/ modules via lib.rs
    // -----------------------------------------------------------------------

    /// Fetch test results from the most recent N finished runs.
    /// Returns all test executions (name, status, duration, file_path, run_id, started_at)
    /// ordered by run start time ascending (oldest first) for correct flip detection.
    pub fn get_historical_test_results(
        &self,
        run_limit: u32,
    ) -> Result<Vec<HistoricalTest>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT te.name, te.status, te.duration_ms, COALESCE(te.file_path, ''), tr.id, tr.started_at
             FROM test_executions te
             JOIN test_runs tr ON te.run_id = tr.id
             WHERE tr.status = 'finished'
               AND tr.id IN (
                   SELECT id FROM test_runs
                   WHERE status = 'finished'
                   ORDER BY started_at DESC
                   LIMIT ?1
               )
             ORDER BY tr.started_at ASC, te.name ASC",
        )?;

        let rows = stmt.query_map(params![run_limit], |row| {
            Ok(HistoricalTest {
                name: row.get(0)?,
                status: row.get(1)?,
                duration_ms: row.get(2)?,
                file_path: row.get(3)?,
                run_id: row.get(4)?,
                started_at: row.get(5)?,
            })
        })?;

        rows.collect()
    }

    /// Fetch failed tests for a specific run (for failure clustering).
    pub fn get_failed_tests_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<FailedTest>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT name, COALESCE(file_path, ''), COALESCE(error_message, '')
             FROM test_executions
             WHERE run_id = ?1 AND status = 'failed'",
        )?;

        let rows = stmt.query_map(params![run_id], |row| {
            Ok(FailedTest {
                name: row.get(0)?,
                file_path: row.get(1)?,
                error: row.get(2)?,
            })
        })?;

        rows.collect()
    }

    /// Compute a simplified health score from the most recent finished run
    /// (excluding `exclude_run_id`). Used for trend comparison (improving/declining/stable).
    /// Returns None if no previous finished run exists.
    pub fn get_previous_health_score(
        &self,
        exclude_run_id: &str,
    ) -> Result<Option<f64>, rusqlite::Error> {
        let result: Result<(i32, i32), _> = self.conn.query_row(
            "SELECT total, passed
             FROM test_runs
             WHERE status = 'finished' AND id != ?1
             ORDER BY started_at DESC
             LIMIT 1",
            params![exclude_run_id],
            |row| {
                Ok((
                    row.get::<_, Option<i32>>(0)?.unwrap_or(0),
                    row.get::<_, Option<i32>>(1)?.unwrap_or(0),
                ))
            },
        );

        match result {
            Ok((total, passed)) => {
                if total == 0 {
                    Ok(None)
                } else {
                    // Simplified health score for trend comparison:
                    // pass_rate * 0.6 + stability(100) * 0.3 + neutral_trend(50) * 0.1
                    let pass_rate = (passed as f64 / total as f64) * 100.0;
                    let score = pass_rate * 0.6 + 100.0 * 0.3 + 50.0 * 0.1;
                    Ok(Some(score.clamp(0.0, 100.0)))
                }
            }
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get the most recent finished run's ID. Returns None if no finished runs exist.
    pub fn get_latest_finished_run_id(&self) -> Result<Option<String>, rusqlite::Error> {
        let result: Result<String, _> = self.conn.query_row(
            "SELECT id FROM test_runs WHERE status = 'finished' ORDER BY started_at DESC LIMIT 1",
            [],
            |row| row.get(0),
        );
        match result {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Delete old runs beyond retention limits. Retention policy: keep the most recent
    /// `max_runs` runs, AND delete anything older than `max_age_days`. A run is only
    /// deleted if it violates BOTH conditions (i.e. it's old AND beyond the count limit).
    ///
    /// Related test_executions, coverage_reports, and pending_uploads are cascade-deleted
    /// via foreign key constraints (requires PRAGMA foreign_keys = ON).
    pub fn cleanup_old_runs(
        &self,
        max_runs: i64,
        max_age_days: i64,
    ) -> Result<usize, rusqlite::Error> {
        let deleted = self.conn.execute(
            "DELETE FROM test_runs WHERE id NOT IN (
                SELECT id FROM test_runs
                ORDER BY started_at DESC
                LIMIT ?1
            ) AND started_at < datetime('now', ?2)",
            params![max_runs, format!("-{} days", max_age_days)],
        )?;
        Ok(deleted)
    }

    // -----------------------------------------------------------------------
    // Cloud sync queries — consumed by sync module
    // -----------------------------------------------------------------------

    /// Get all test executions for a specific run (for building the ingest payload).
    pub fn get_test_executions_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<TestEvent>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT name, status, duration_ms, file_path, error_message, retry_count, flaky
             FROM test_executions WHERE run_id = ?1",
        )?;

        let rows = stmt.query_map(params![run_id], |row| {
            Ok(TestEvent {
                name: row.get(0)?,
                status: row.get(1)?,
                duration: row.get(2)?,
                file_path: row.get(3)?,
                error: row.get(4)?,
                retry_count: row.get(5)?,
                flaky: row.get(6)?,
            })
        })?;

        rows.collect()
    }

    /// Get run metadata needed for the ingest payload.
    /// Returns (branch, commit_sha, ci_provider, framework, started_at, finished_at).
    pub fn get_run_metadata(
        &self,
        run_id: &str,
    ) -> Result<(Option<String>, Option<String>, Option<String>, String, String, Option<String>), rusqlite::Error> {
        self.conn.query_row(
            "SELECT branch, commit_sha, ci_provider, framework, started_at, finished_at
             FROM test_runs WHERE id = ?1",
            params![run_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
        )
    }

    /// Insert a pending upload for cloud sync.
    pub fn insert_pending_upload(
        &self,
        run_id: &str,
        payload_json: &str,
    ) -> Result<i64, rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO pending_uploads (run_id, payload) VALUES (?1, ?2)",
            params![run_id, payload_json],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get pending uploads that haven't exceeded the max retry count.
    pub fn get_pending_uploads(
        &self,
        max_attempts: i32,
    ) -> Result<Vec<PendingUpload>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, run_id, payload, attempts FROM pending_uploads
             WHERE attempts < ?1
             ORDER BY created_at ASC",
        )?;

        let rows = stmt.query_map(params![max_attempts], |row| {
            Ok(PendingUpload {
                id: row.get(0)?,
                run_id: row.get(1)?,
                payload: row.get(2)?,
                attempts: row.get(3)?,
            })
        })?;

        rows.collect()
    }

    /// Mark a pending upload as synced: delete the pending_upload row and set
    /// `synced_at` on the corresponding test run. Wrapped in a transaction so
    /// both operations succeed or fail atomically.
    pub fn mark_synced(
        &self,
        upload_id: i64,
        run_id: &str,
    ) -> Result<(), rusqlite::Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM pending_uploads WHERE id = ?1",
            params![upload_id],
        )?;
        tx.execute(
            "UPDATE test_runs SET synced_at = datetime('now') WHERE id = ?1",
            params![run_id],
        )?;
        tx.commit()
    }

    /// Record a sync failure: increment attempts and store the error message.
    pub fn record_sync_failure(
        &self,
        upload_id: i64,
        error: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE pending_uploads SET attempts = attempts + 1, last_error = ?2 WHERE id = ?1",
            params![upload_id, error],
        )?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Coverage queries
    // -----------------------------------------------------------------------

    /// Insert a coverage report for a run.
    pub fn record_coverage(
        &self,
        run_id: &str,
        summary: &CoverageSummary,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "INSERT INTO coverage_reports (run_id, format, lines_covered, lines_total, branches_covered, branches_total, functions_covered, functions_total)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run_id,
                summary.format,
                summary.lines.covered,
                summary.lines.total,
                summary.branches.covered,
                summary.branches.total,
                summary.functions.covered,
                summary.functions.total,
            ],
        )?;
        Ok(())
    }

    /// Store per-file coverage JSON in the data column for cloud sync.
    pub fn store_coverage_files(
        &self,
        run_id: &str,
        files_json: &str,
    ) -> Result<(), rusqlite::Error> {
        self.conn.execute(
            "UPDATE coverage_reports SET data = ?2 WHERE run_id = ?1",
            params![run_id, files_json],
        )?;
        Ok(())
    }

    /// Get per-file coverage JSON for a run. Returns None if no coverage exists.
    pub fn get_coverage_files_json(
        &self,
        run_id: &str,
    ) -> Result<Option<String>, rusqlite::Error> {
        let result = self.conn.query_row(
            "SELECT data FROM coverage_reports WHERE run_id = ?1 ORDER BY created_at DESC LIMIT 1",
            params![run_id],
            |row| row.get::<_, Option<String>>(0),
        );

        match result {
            Ok(data) => Ok(data),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    // -----------------------------------------------------------------------
    // Query subcommand queries — consumed by `gaffer query` via GafferCore
    // -----------------------------------------------------------------------

    /// List recent finished runs, ordered by most recent first.
    pub fn get_recent_runs(&self, limit: u32) -> Result<Vec<RecentRun>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT id, branch, commit_sha, framework, started_at,
                    COALESCE(finished_at, started_at),
                    COALESCE(total, 0), COALESCE(passed, 0), COALESCE(failed, 0),
                    COALESCE(skipped, 0), COALESCE(duration_ms, 0.0)
             FROM test_runs WHERE status = 'finished'
             ORDER BY started_at DESC LIMIT ?1",
        )?;

        let rows = stmt.query_map(params![limit], |row| {
            Ok(RecentRun {
                id: row.get(0)?,
                branch: row.get(1)?,
                commit_sha: row.get(2)?,
                framework: row.get(3)?,
                started_at: row.get(4)?,
                finished_at: row.get(5)?,
                total: row.get(6)?,
                passed: row.get(7)?,
                failed: row.get(8)?,
                skipped: row.get(9)?,
                duration_ms: row.get(10)?,
            })
        })?;

        rows.collect()
    }

    /// Get pass/fail history for a specific test (name matched with LIKE).
    pub fn get_test_history(
        &self,
        test_pattern: &str,
        limit: u32,
    ) -> Result<Vec<TestHistoryEntry>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT te.name, te.status, te.duration_ms, te.error_message,
                    tr.branch, tr.commit_sha, tr.started_at
             FROM test_executions te
             JOIN test_runs tr ON te.run_id = tr.id
             WHERE tr.status = 'finished' AND te.name LIKE ?1 ESCAPE '\\'
             ORDER BY tr.started_at DESC LIMIT ?2",
        )?;

        let like_pattern = format!("%{}%", escape_like(test_pattern));
        let rows = stmt.query_map(params![like_pattern, limit], |row| {
            Ok(TestHistoryEntry {
                name: row.get(0)?,
                status: row.get(1)?,
                duration_ms: row.get(2)?,
                error_message: row.get(3)?,
                branch: row.get(4)?,
                commit_sha: row.get(5)?,
                started_at: row.get(6)?,
            })
        })?;

        rows.collect()
    }

    /// Search failures across runs by name or error message pattern.
    pub fn search_failures(
        &self,
        pattern: &str,
        limit: u32,
    ) -> Result<Vec<FailureSearchResult>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT te.name, te.file_path, te.error_message, te.duration_ms,
                    tr.branch, tr.commit_sha, tr.started_at
             FROM test_executions te
             JOIN test_runs tr ON te.run_id = tr.id
             WHERE tr.status = 'finished' AND te.status = 'failed'
               AND (te.name LIKE ?1 ESCAPE '\\' OR te.error_message LIKE ?1 ESCAPE '\\')
             ORDER BY tr.started_at DESC LIMIT ?2",
        )?;

        let like_pattern = format!("%{}%", escape_like(pattern));
        let rows = stmt.query_map(params![like_pattern, limit], |row| {
            Ok(FailureSearchResult {
                name: row.get(0)?,
                file_path: row.get(1)?,
                error_message: row.get(2)?,
                duration_ms: row.get(3)?,
                branch: row.get(4)?,
                commit_sha: row.get(5)?,
                started_at: row.get(6)?,
            })
        })?;

        rows.collect()
    }

    // -----------------------------------------------------------------------
    // Comparison queries — consumed by compare_run() in lib.rs
    // -----------------------------------------------------------------------

    /// Get the most recent finished run on a specific branch.
    /// Excludes `exclude_run_id` to handle same-branch comparison
    /// where the current run would otherwise be its own baseline.
    pub fn get_latest_run_for_branch(
        &self,
        branch: &str,
        exclude_run_id: &str,
    ) -> Result<Option<(String, RunSummary)>, rusqlite::Error> {
        let result = self.conn.query_row(
            "SELECT id, total, passed, failed, skipped, COALESCE(duration_ms, 0.0)
             FROM test_runs
             WHERE status = 'finished' AND branch = ?1 AND id != ?2
               AND total IS NOT NULL
             ORDER BY started_at DESC LIMIT 1",
            params![branch, exclude_run_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    RunSummary {
                        total: row.get(1)?,
                        passed: row.get(2)?,
                        failed: row.get(3)?,
                        skipped: row.get(4)?,
                        duration: row.get(5)?,
                    },
                ))
            },
        );
        match result {
            Ok(tuple) => Ok(Some(tuple)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get (name, status) pairs for a run — lightweight, for comparison only.
    pub fn get_test_statuses_for_run(
        &self,
        run_id: &str,
    ) -> Result<Vec<(String, String)>, rusqlite::Error> {
        let mut stmt = self.conn.prepare(
            "SELECT name, status FROM test_executions WHERE run_id = ?1 ORDER BY id ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
        rows.collect()
    }

    /// Get the latest finished run's summary counts for health score computation.
    /// Returns (run_id, total, passed) or None if no finished runs exist.
    pub fn get_latest_run_summary(&self) -> Result<Option<(String, i32, i32)>, rusqlite::Error> {
        let result = self.conn.query_row(
            "SELECT id, COALESCE(total, 0), COALESCE(passed, 0)
             FROM test_runs WHERE status = 'finished'
             ORDER BY started_at DESC LIMIT 1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        );
        match result {
            Ok(tuple) => Ok(Some(tuple)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Get coverage data for a run. Returns None if no coverage exists.
    pub fn get_coverage_for_run(
        &self,
        run_id: &str,
    ) -> Result<Option<CoverageSummary>, rusqlite::Error> {
        let result = self.conn.query_row(
            "SELECT format, lines_covered, lines_total, branches_covered, branches_total, functions_covered, functions_total
             FROM coverage_reports WHERE run_id = ?1
             ORDER BY created_at DESC LIMIT 1",
            params![run_id],
            |row| {
                Ok(CoverageSummary {
                    format: row.get(0)?,
                    lines: crate::types::CoverageMetrics {
                        covered: row.get(1)?,
                        total: row.get(2)?,
                    },
                    branches: crate::types::CoverageMetrics {
                        covered: row.get(3)?,
                        total: row.get(4)?,
                    },
                    functions: crate::types::CoverageMetrics {
                        covered: row.get(5)?,
                        total: row.get(6)?,
                    },
                })
            },
        );

        match result {
            Ok(summary) => Ok(Some(summary)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::status;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    //
    // `test_db()` creates an isolated database in a temp directory.
    // The TempDir is returned alongside the Database — dropping it cleans up the files.
    // Always bind the TempDir to `_dir` (not `_`) to prevent immediate cleanup.
    // -----------------------------------------------------------------------

    fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("data.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    fn sample_metadata() -> RunMetadata {
        RunMetadata {
            branch: Some("main".to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: None,
            framework: "vitest".to_string(),
        }
    }

    fn sample_test(name: &str, status: &str, duration: f64) -> TestEvent {
        TestEvent {
            name: name.to_string(),
            status: status.to_string(),
            duration,
            file_path: Some("src/auth.test.ts".to_string()),
            error: None,
            retry_count: None,
            flaky: None,
        }
    }

    /// Helper to read a full row from test_runs for detailed assertions.
    /// Returns (status, total, passed, failed, skipped, duration_ms, finished_at, branch, commit_sha).
    fn get_run_row(
        db: &Database,
        run_id: &str,
    ) -> (
        String,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<i32>,
        Option<f64>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        db.conn
            .query_row(
                "SELECT status, total, passed, failed, skipped, duration_ms, finished_at, branch, commit_sha
                 FROM test_runs WHERE id = ?1",
                params![run_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                    ))
                },
            )
            .unwrap()
    }

    // -----------------------------------------------------------------------
    // Database lifecycle
    // -----------------------------------------------------------------------

    #[test]
    fn creates_empty_database() {
        let (db, _dir) = test_db();
        assert_eq!(db.get_run_count().unwrap(), 0);
    }

    #[test]
    fn creates_parent_directories() {
        let dir = TempDir::new().unwrap();
        // Nested path that doesn't exist yet
        let db_path = dir.path().join("deeply").join("nested").join("data.db");
        let db = Database::open(&db_path).unwrap();
        assert_eq!(db.get_run_count().unwrap(), 0);
        assert!(db_path.exists());
    }

    #[test]
    fn migration_is_idempotent() {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("data.db");

        // Open twice — second open re-runs CREATE IF NOT EXISTS migrations
        let db1 = Database::open(&db_path).unwrap();
        db1.insert_run("run-1", &sample_metadata(), "2026-01-01T00:00:00Z")
            .unwrap();
        drop(db1);

        let db2 = Database::open(&db_path).unwrap();
        // Data from first session should still be there
        assert_eq!(db2.get_run_count().unwrap(), 1);
    }

    // -----------------------------------------------------------------------
    // Run insertion
    // -----------------------------------------------------------------------

    #[test]
    fn inserts_run_with_all_metadata() {
        let (db, _dir) = test_db();
        let metadata = RunMetadata {
            branch: Some("feat/login".to_string()),
            commit: Some("deadbeef".to_string()),
            ci_provider: Some("github-actions".to_string()),
            framework: "playwright".to_string(),
        };

        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();
        let row = get_run_row(&db, "run-1");

        assert_eq!(row.0, "running"); // status
        assert_eq!(row.7, Some("feat/login".to_string())); // branch
        assert_eq!(row.8, Some("deadbeef".to_string())); // commit_sha
    }

    #[test]
    fn inserts_run_with_null_optional_fields() {
        let (db, _dir) = test_db();
        // Simulates a local run outside git — no branch, commit, or CI provider
        let metadata = RunMetadata {
            branch: None,
            commit: None,
            ci_provider: None,
            framework: "vitest".to_string(),
        };

        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();
        let row = get_run_row(&db, "run-1");

        assert_eq!(row.0, "running");
        assert_eq!(row.7, None); // branch is NULL
        assert_eq!(row.8, None); // commit_sha is NULL
    }

    #[test]
    fn rejects_duplicate_run_id() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();
        let result = db.insert_run("run-1", &metadata, "2026-02-22T11:00:00Z");

        assert!(result.is_err()); // PRIMARY KEY violation
    }

    #[test]
    fn inserts_multiple_runs() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();
        db.insert_run("run-2", &metadata, "2026-02-22T11:00:00Z")
            .unwrap();
        db.insert_run("run-3", &metadata, "2026-02-22T12:00:00Z")
            .unwrap();

        assert_eq!(db.get_run_count().unwrap(), 3);
    }

    // -----------------------------------------------------------------------
    // Test insertion
    // -----------------------------------------------------------------------

    #[test]
    fn inserts_tests_with_all_statuses() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 150.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("test_b", status::FAILED, 200.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("test_c", status::SKIPPED, 0.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("test_d", status::TODO, 0.0))
            .unwrap();

        assert_eq!(db.get_test_count("run-1").unwrap(), 4);
        assert_eq!(db.get_test_count_by_status("run-1", "passed").unwrap(), 1);
        assert_eq!(db.get_test_count_by_status("run-1", "failed").unwrap(), 1);
        assert_eq!(
            db.get_test_count_by_status("run-1", "skipped").unwrap(),
            1
        );
        assert_eq!(db.get_test_count_by_status("run-1", "todo").unwrap(), 1);
    }

    #[test]
    fn persists_error_and_retry_fields() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let test = TestEvent {
            name: "test_login".to_string(),
            status: status::FAILED.to_string(),
            duration: 200.0,
            file_path: Some("src/auth.test.ts".to_string()),
            error: Some("Expected 200, got 401".to_string()),
            retry_count: Some(2),
            flaky: Some(true),
        };
        db.insert_test("run-1", &test).unwrap();

        let (error, retry_count, flaky): (Option<String>, i32, bool) = db
            .conn
            .query_row(
                "SELECT error_message, retry_count, flaky FROM test_executions WHERE run_id = 'run-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();

        assert_eq!(error, Some("Expected 200, got 401".to_string()));
        assert_eq!(retry_count, 2);
        assert!(flaky);
    }

    #[test]
    fn defaults_optional_test_fields_to_zero_and_false() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        // TestEvent with all optional fields as None
        let test = TestEvent {
            name: "test_basic".to_string(),
            status: status::PASSED.to_string(),
            duration: 50.0,
            file_path: None,
            error: None,
            retry_count: None,
            flaky: None,
        };
        db.insert_test("run-1", &test).unwrap();

        let (file_path, error, retry_count, flaky): (Option<String>, Option<String>, i32, bool) =
            db.conn
                .query_row(
                    "SELECT file_path, error_message, retry_count, flaky FROM test_executions WHERE run_id = 'run-1'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .unwrap();

        assert_eq!(file_path, None);
        assert_eq!(error, None);
        assert_eq!(retry_count, 0);
        assert!(!flaky);
    }

    #[test]
    fn test_count_returns_zero_for_unknown_run() {
        let (db, _dir) = test_db();
        assert_eq!(db.get_test_count("nonexistent").unwrap(), 0);
        assert_eq!(
            db.get_test_count_by_status("nonexistent", "passed").unwrap(),
            0
        );
    }

    // -----------------------------------------------------------------------
    // Finishing runs
    // -----------------------------------------------------------------------

    #[test]
    fn finish_run_persists_all_summary_fields() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let summary = RunSummary {
            total: 42,
            passed: 38,
            failed: 3,
            skipped: 1,
            duration: 12345.6,
        };
        db.finish_run("run-1", &summary, "2026-02-22T10:05:00Z")
            .unwrap();

        let row = get_run_row(&db, "run-1");
        assert_eq!(row.0, "finished");
        assert_eq!(row.1, Some(42)); // total
        assert_eq!(row.2, Some(38)); // passed
        assert_eq!(row.3, Some(3)); // failed
        assert_eq!(row.4, Some(1)); // skipped
        assert!((row.5.unwrap() - 12345.6).abs() < 0.01); // duration_ms
        assert_eq!(row.6, Some("2026-02-22T10:05:00Z".to_string())); // finished_at
    }

    #[test]
    fn finish_run_errors_for_nonexistent_run() {
        let (db, _dir) = test_db();
        let summary = RunSummary {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            duration: 100.0,
        };
        let result = db.finish_run("nonexistent", &summary, "2026-02-22T10:05:00Z");
        assert!(result.is_err());
    }

    #[test]
    fn finish_run_errors_for_already_finished_run() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let summary = RunSummary {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            duration: 100.0,
        };
        // First finish succeeds
        db.finish_run("run-1", &summary, "2026-02-22T10:05:00Z")
            .unwrap();
        // Second finish fails — already in 'finished' state
        let result = db.finish_run("run-1", &summary, "2026-02-22T10:06:00Z");
        assert!(result.is_err());
    }

    #[test]
    fn get_run_started_at_returns_timestamp() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let started_at = db.get_run_started_at("run-1").unwrap();
        assert_eq!(started_at, "2026-02-22T10:00:00Z");
    }

    #[test]
    fn get_run_started_at_errors_for_unknown_run() {
        let (db, _dir) = test_db();
        let result = db.get_run_started_at("nonexistent");
        assert!(result.is_err());
    }

    // -----------------------------------------------------------------------
    // Cascade deletes
    //
    // Foreign key ON DELETE CASCADE ensures that deleting a test_run also deletes
    // its child test_executions, coverage_reports, and pending_uploads. This is
    // critical for cleanup_old_runs — without it, orphaned rows would accumulate.
    // -----------------------------------------------------------------------

    #[test]
    fn cascade_deletes_test_executions_when_run_deleted() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();
        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 100.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("test_b", status::FAILED, 200.0))
            .unwrap();

        assert_eq!(db.get_test_count("run-1").unwrap(), 2);

        // Directly delete the run
        db.conn
            .execute("DELETE FROM test_runs WHERE id = 'run-1'", [])
            .unwrap();

        // Child rows should be cascade-deleted
        assert_eq!(db.get_test_count("run-1").unwrap(), 0);
    }

    #[test]
    fn cascade_deletes_coverage_and_uploads_when_run_deleted() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        // Insert coverage and pending upload rows directly
        db.conn
            .execute(
                "INSERT INTO coverage_reports (run_id, format, lines_covered, lines_total) VALUES ('run-1', 'lcov', 10, 20)",
                [],
            )
            .unwrap();
        db.conn
            .execute(
                "INSERT INTO pending_uploads (run_id, payload) VALUES ('run-1', '{}')",
                [],
            )
            .unwrap();

        // Verify they exist
        let coverage_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM coverage_reports WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(coverage_count, 1);

        // Delete the run
        db.conn
            .execute("DELETE FROM test_runs WHERE id = 'run-1'", [])
            .unwrap();

        // All child rows should be gone
        let coverage_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM coverage_reports WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let upload_count: i64 = db
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pending_uploads WHERE run_id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(coverage_count, 0);
        assert_eq!(upload_count, 0);
    }

    // -----------------------------------------------------------------------
    // Cleanup / retention
    //
    // cleanup_old_runs implements the retention policy: keep the N most recent
    // runs, and delete anything older than M days. A run must violate BOTH
    // conditions to be deleted (old AND beyond the count limit).
    //
    // This is important because:
    // - A project with few runs should never lose data just because it's old
    // - A project with many runs per day shouldn't keep months of stale data
    // -----------------------------------------------------------------------

    #[test]
    fn cleanup_keeps_recent_runs_within_limit() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        // Insert 5 runs
        for i in 1..=5 {
            db.insert_run(
                &format!("run-{}", i),
                &metadata,
                &format!("2026-02-{:02}T10:00:00Z", i),
            )
            .unwrap();
        }

        // Keep max 3 — but set max_age_days high so nothing is "old"
        let deleted = db.cleanup_old_runs(3, 36500).unwrap(); // 100 years

        // Nothing deleted because nothing is older than 100 years
        assert_eq!(deleted, 0);
        assert_eq!(db.get_run_count().unwrap(), 5);
    }

    #[test]
    fn cleanup_deletes_old_runs_beyond_limit() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        // Insert 5 runs with old timestamps (well beyond 1 day ago)
        for i in 1..=5 {
            db.insert_run(
                &format!("run-{}", i),
                &metadata,
                &format!("2020-01-{:02}T10:00:00Z", i),
            )
            .unwrap();
        }

        // Keep max 3, delete anything older than 1 day
        let deleted = db.cleanup_old_runs(3, 1).unwrap();

        // The 2 oldest runs should be deleted (beyond the 3-run limit AND older than 1 day)
        assert_eq!(deleted, 2);
        assert_eq!(db.get_run_count().unwrap(), 3);
    }

    #[test]
    fn cleanup_cascades_to_child_rows() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        // Old run with tests
        db.insert_run("old-run", &metadata, "2020-01-01T10:00:00Z")
            .unwrap();
        db.insert_test("old-run", &sample_test("test_a", status::PASSED, 100.0))
            .unwrap();

        // Recent run with tests
        db.insert_run("new-run", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();
        db.insert_test("new-run", &sample_test("test_b", status::PASSED, 100.0))
            .unwrap();

        // Keep max 1, delete old
        db.cleanup_old_runs(1, 1).unwrap();

        assert_eq!(db.get_run_count().unwrap(), 1);
        assert_eq!(db.get_test_count("old-run").unwrap(), 0); // cascade deleted
        assert_eq!(db.get_test_count("new-run").unwrap(), 1); // preserved
    }

    #[test]
    fn cleanup_is_noop_when_under_limits() {
        let (db, _dir) = test_db();
        let metadata = sample_metadata();

        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();

        let deleted = db.cleanup_old_runs(100, 90).unwrap();
        assert_eq!(deleted, 0);
        assert_eq!(db.get_run_count().unwrap(), 1);
    }

    #[test]
    fn cleanup_on_empty_database() {
        let (db, _dir) = test_db();
        let deleted = db.cleanup_old_runs(100, 90).unwrap();
        assert_eq!(deleted, 0);
    }

    // -----------------------------------------------------------------------
    // Cloud sync DB methods
    // -----------------------------------------------------------------------

    #[test]
    fn get_test_executions_for_run_returns_all_tests() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();
        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 100.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("test_b", status::FAILED, 200.0))
            .unwrap();

        let tests = db.get_test_executions_for_run("run-1").unwrap();
        assert_eq!(tests.len(), 2);
        assert_eq!(tests[0].name, "test_a");
        assert_eq!(tests[1].name, "test_b");
    }

    #[test]
    fn get_test_executions_for_run_returns_empty_for_unknown_run() {
        let (db, _dir) = test_db();
        let tests = db.get_test_executions_for_run("nonexistent").unwrap();
        assert!(tests.is_empty());
    }

    #[test]
    fn get_run_metadata_returns_all_fields() {
        let (db, _dir) = test_db();
        let metadata = RunMetadata {
            branch: Some("main".to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: Some("github-actions".to_string()),
            framework: "vitest".to_string(),
        };
        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();

        let (branch, commit, ci, framework, started_at, finished_at) =
            db.get_run_metadata("run-1").unwrap();
        assert_eq!(branch, Some("main".to_string()));
        assert_eq!(commit, Some("abc123".to_string()));
        assert_eq!(ci, Some("github-actions".to_string()));
        assert_eq!(framework, "vitest");
        assert_eq!(started_at, "2026-02-22T10:00:00Z");
        assert_eq!(finished_at, None);
    }

    #[test]
    fn pending_upload_lifecycle() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        // Insert pending upload
        let upload_id = db
            .insert_pending_upload("run-1", r#"{"runId":"run-1"}"#)
            .unwrap();
        assert!(upload_id > 0);

        // Get pending uploads
        let pending = db.get_pending_uploads(5).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].run_id, "run-1");
        assert_eq!(pending[0].attempts, 0);

        // Mark synced
        db.mark_synced(upload_id, "run-1").unwrap();

        // Should be gone
        let pending = db.get_pending_uploads(5).unwrap();
        assert!(pending.is_empty());

        // synced_at should be set
        let synced_at: Option<String> = db
            .conn
            .query_row(
                "SELECT synced_at FROM test_runs WHERE id = 'run-1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(synced_at.is_some());
    }

    #[test]
    fn record_sync_failure_increments_attempts() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();
        let upload_id = db
            .insert_pending_upload("run-1", r#"{"runId":"run-1"}"#)
            .unwrap();

        db.record_sync_failure(upload_id, "connection refused")
            .unwrap();
        db.record_sync_failure(upload_id, "timeout").unwrap();

        let pending = db.get_pending_uploads(5).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].attempts, 2);

        // Exceeding max_attempts should exclude it
        for _ in 0..3 {
            db.record_sync_failure(upload_id, "still failing").unwrap();
        }
        let pending = db.get_pending_uploads(5).unwrap();
        assert!(pending.is_empty());
    }

    // -----------------------------------------------------------------------
    // Coverage DB methods
    // -----------------------------------------------------------------------

    fn sample_coverage() -> CoverageSummary {
        CoverageSummary {
            format: "lcov".to_string(),
            lines: crate::types::CoverageMetrics { covered: 80, total: 100 },
            branches: crate::types::CoverageMetrics { covered: 20, total: 30 },
            functions: crate::types::CoverageMetrics { covered: 15, total: 20 },
        }
    }

    #[test]
    fn record_and_retrieve_coverage() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let coverage = sample_coverage();
        db.record_coverage("run-1", &coverage).unwrap();

        let retrieved = db.get_coverage_for_run("run-1").unwrap().unwrap();
        assert_eq!(retrieved.format, "lcov");
        assert_eq!(retrieved.lines.covered, 80);
        assert_eq!(retrieved.lines.total, 100);
        assert_eq!(retrieved.branches.covered, 20);
        assert_eq!(retrieved.branches.total, 30);
        assert_eq!(retrieved.functions.covered, 15);
        assert_eq!(retrieved.functions.total, 20);
    }

    #[test]
    fn get_coverage_returns_none_for_no_coverage() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();

        let result = db.get_coverage_for_run("run-1").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn coverage_cascade_deletes_with_run() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-02-22T10:00:00Z")
            .unwrap();
        db.record_coverage("run-1", &sample_coverage()).unwrap();

        db.conn
            .execute("DELETE FROM test_runs WHERE id = 'run-1'", [])
            .unwrap();

        let result = db.get_coverage_for_run("run-1").unwrap();
        assert!(result.is_none());
    }

    // -----------------------------------------------------------------------
    // Query subcommand DB methods
    // -----------------------------------------------------------------------

    /// Helper: create a finished run with given counts.
    fn insert_finished_run(
        db: &Database,
        run_id: &str,
        started_at: &str,
        total: i32,
        passed: i32,
        failed: i32,
    ) {
        let metadata = RunMetadata {
            branch: Some("main".to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: None,
            framework: "vitest".to_string(),
        };
        db.insert_run(run_id, &metadata, started_at).unwrap();
        let summary = RunSummary {
            total,
            passed,
            failed,
            skipped: total - passed - failed,
            duration: 1000.0,
        };
        let finished_at = format!("{}1", started_at); // slightly after
        db.finish_run(run_id, &summary, &finished_at).unwrap();
    }

    #[test]
    fn get_recent_runs_returns_finished_runs_newest_first() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 10, 9, 1);
        insert_finished_run(&db, "run-2", "2026-01-02T10:00:00Z", 20, 18, 2);
        insert_finished_run(&db, "run-3", "2026-01-03T10:00:00Z", 5, 5, 0);

        let runs = db.get_recent_runs(10).unwrap();
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].id, "run-3"); // newest first
        assert_eq!(runs[1].id, "run-2");
        assert_eq!(runs[2].id, "run-1");
        assert_eq!(runs[0].total, 5);
        assert_eq!(runs[0].passed, 5);
        assert_eq!(runs[0].failed, 0);
    }

    #[test]
    fn get_recent_runs_respects_limit() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 10, 10, 0);
        insert_finished_run(&db, "run-2", "2026-01-02T10:00:00Z", 10, 10, 0);
        insert_finished_run(&db, "run-3", "2026-01-03T10:00:00Z", 10, 10, 0);

        let runs = db.get_recent_runs(2).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn get_recent_runs_excludes_running() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 10, 10, 0);
        // Insert a running (not finished) run
        db.insert_run("run-2", &sample_metadata(), "2026-01-02T10:00:00Z")
            .unwrap();

        let runs = db.get_recent_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].id, "run-1");
    }

    #[test]
    fn get_recent_runs_empty_db() {
        let (db, _dir) = test_db();
        let runs = db.get_recent_runs(10).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn get_test_history_matches_by_name() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 2, 2, 0);
        db.insert_test("run-1", &sample_test("auth > login", status::PASSED, 100.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("auth > logout", status::PASSED, 50.0))
            .unwrap();

        insert_finished_run(&db, "run-2", "2026-01-02T10:00:00Z", 2, 1, 1);
        let mut failed_test = sample_test("auth > login", status::FAILED, 200.0);
        failed_test.error = Some("timeout".to_string());
        db.insert_test("run-2", &failed_test).unwrap();
        db.insert_test("run-2", &sample_test("auth > logout", status::PASSED, 50.0))
            .unwrap();

        // Search for "login" — should get 2 entries
        let history = db.get_test_history("login", 50).unwrap();
        assert_eq!(history.len(), 2);
        assert_eq!(history[0].status, "failed"); // newest first
        assert_eq!(history[0].error_message, Some("timeout".to_string()));
        assert_eq!(history[1].status, "passed");
    }

    #[test]
    fn get_test_history_no_matches() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 1, 1, 0);
        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 100.0))
            .unwrap();

        let history = db.get_test_history("nonexistent", 50).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn search_failures_matches_name_and_error() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 3, 1, 2);

        let mut test1 = sample_test("auth > login", status::FAILED, 100.0);
        test1.error = Some("connection refused".to_string());
        db.insert_test("run-1", &test1).unwrap();

        let mut test2 = sample_test("api > fetch_users", status::FAILED, 200.0);
        test2.error = Some("timeout error".to_string());
        db.insert_test("run-1", &test2).unwrap();

        db.insert_test("run-1", &sample_test("test_ok", status::PASSED, 50.0))
            .unwrap();

        // Search by error pattern
        let results = db.search_failures("connection", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "auth > login");

        // Search by name pattern
        let results = db.search_failures("fetch", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "api > fetch_users");
    }

    #[test]
    fn search_failures_excludes_passing_tests() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 1, 1, 0);
        db.insert_test("run-1", &sample_test("login_test", status::PASSED, 100.0))
            .unwrap();

        let results = db.search_failures("login", 50).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_failures_empty_db() {
        let (db, _dir) = test_db();
        let results = db.search_failures("anything", 50).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_failures_matches_name_when_error_is_null() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 1, 0, 1);
        // Insert a failed test with no error message
        let mut t = sample_test("auth > login", status::FAILED, 100.0);
        t.error = None;
        db.insert_test("run-1", &t).unwrap();

        let results = db.search_failures("login", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].error_message.is_none());
    }

    #[test]
    fn search_failures_underscore_is_literal() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 2, 0, 2);
        let mut t1 = sample_test("test_a", status::FAILED, 100.0);
        t1.error = Some("err".to_string());
        db.insert_test("run-1", &t1).unwrap();
        let mut t2 = sample_test("testXa", status::FAILED, 100.0);
        t2.error = Some("err".to_string());
        db.insert_test("run-1", &t2).unwrap();

        // "test_a" with literal underscore should NOT match "testXa"
        let results = db.search_failures("test_a", 50).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "test_a");
    }

    #[test]
    fn get_test_history_underscore_is_literal() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 2, 2, 0);
        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 100.0))
            .unwrap();
        db.insert_test("run-1", &sample_test("testXa", status::PASSED, 100.0))
            .unwrap();

        // "test_a" with literal underscore should NOT match "testXa"
        let history = db.get_test_history("test_a", 50).unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].name, "test_a");
    }

    #[test]
    fn get_latest_run_summary_returns_most_recent() {
        let (db, _dir) = test_db();
        insert_finished_run(&db, "run-1", "2026-01-01T10:00:00Z", 10, 8, 2);
        insert_finished_run(&db, "run-2", "2026-01-02T10:00:00Z", 20, 19, 1);

        let (id, total, passed) = db.get_latest_run_summary().unwrap().unwrap();
        assert_eq!(id, "run-2");
        assert_eq!(total, 20);
        assert_eq!(passed, 19);
    }

    #[test]
    fn get_latest_run_summary_empty_db() {
        let (db, _dir) = test_db();
        assert!(db.get_latest_run_summary().unwrap().is_none());
    }

    // -----------------------------------------------------------------------
    // Comparison DB methods
    // -----------------------------------------------------------------------

    /// Helper: create a finished run on a specific branch.
    fn insert_finished_run_on_branch(
        db: &Database,
        run_id: &str,
        branch: &str,
        started_at: &str,
        total: i32,
        passed: i32,
        failed: i32,
    ) {
        let metadata = RunMetadata {
            branch: Some(branch.to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: None,
            framework: "vitest".to_string(),
        };
        db.insert_run(run_id, &metadata, started_at).unwrap();
        let summary = RunSummary {
            total,
            passed,
            failed,
            skipped: total - passed - failed,
            duration: 1000.0,
        };
        let finished_at = format!("{}1", started_at);
        db.finish_run(run_id, &summary, &finished_at).unwrap();
    }

    #[test]
    fn get_latest_run_for_branch_returns_most_recent() {
        let (db, _dir) = test_db();
        insert_finished_run_on_branch(&db, "run-1", "main", "2026-01-01T10:00:00Z", 10, 9, 1);
        insert_finished_run_on_branch(&db, "run-2", "main", "2026-01-02T10:00:00Z", 20, 18, 2);
        insert_finished_run_on_branch(&db, "run-3", "feat", "2026-01-03T10:00:00Z", 5, 5, 0);

        let result = db.get_latest_run_for_branch("main", "exclude-none").unwrap();
        assert!(result.is_some());
        let (id, summary) = result.unwrap();
        assert_eq!(id, "run-2");
        assert_eq!(summary.total, 20);
        assert_eq!(summary.passed, 18);
        assert_eq!(summary.failed, 2);
    }

    #[test]
    fn get_latest_run_for_branch_returns_none_for_unknown_branch() {
        let (db, _dir) = test_db();
        insert_finished_run_on_branch(&db, "run-1", "main", "2026-01-01T10:00:00Z", 10, 10, 0);

        let result = db.get_latest_run_for_branch("nonexistent", "exclude-none").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_latest_run_for_branch_excludes_running_runs() {
        let (db, _dir) = test_db();
        // Insert a running (not finished) run on main
        let metadata = RunMetadata {
            branch: Some("main".to_string()),
            commit: None,
            ci_provider: None,
            framework: "vitest".to_string(),
        };
        db.insert_run("run-1", &metadata, "2026-01-01T10:00:00Z").unwrap();

        let result = db.get_latest_run_for_branch("main", "exclude-none").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn get_latest_run_for_branch_excludes_specified_run() {
        let (db, _dir) = test_db();
        insert_finished_run_on_branch(&db, "run-1", "main", "2026-01-01T10:00:00Z", 10, 9, 1);
        insert_finished_run_on_branch(&db, "run-2", "main", "2026-01-02T10:00:00Z", 20, 18, 2);

        // Exclude run-2 — should fall back to run-1
        let result = db.get_latest_run_for_branch("main", "run-2").unwrap();
        assert!(result.is_some());
        let (id, _) = result.unwrap();
        assert_eq!(id, "run-1");
    }

    #[test]
    fn get_test_statuses_for_run_returns_all_statuses() {
        let (db, _dir) = test_db();
        db.insert_run("run-1", &sample_metadata(), "2026-01-01T10:00:00Z").unwrap();
        db.insert_test("run-1", &sample_test("test_a", status::PASSED, 100.0)).unwrap();
        db.insert_test("run-1", &sample_test("test_b", status::FAILED, 200.0)).unwrap();
        db.insert_test("run-1", &sample_test("test_c", status::SKIPPED, 0.0)).unwrap();

        let statuses = db.get_test_statuses_for_run("run-1").unwrap();
        assert_eq!(statuses.len(), 3);

        let map: std::collections::HashMap<_, _> = statuses.into_iter().collect();
        assert_eq!(map["test_a"], "passed");
        assert_eq!(map["test_b"], "failed");
        assert_eq!(map["test_c"], "skipped");
    }

    #[test]
    fn get_test_statuses_for_run_empty_for_unknown() {
        let (db, _dir) = test_db();
        let statuses = db.get_test_statuses_for_run("nonexistent").unwrap();
        assert!(statuses.is_empty());
    }
}
