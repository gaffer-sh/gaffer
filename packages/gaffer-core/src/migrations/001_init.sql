-- Gaffer Reporter Core — SQLite schema v1
--
-- This file is embedded at compile time via include_str!() in db.rs.
-- It runs on every Database::open() call, so all statements MUST be idempotent
-- (CREATE IF NOT EXISTS, INSERT OR IGNORE).
--
-- Foreign keys use ON DELETE CASCADE — deleting a test_run automatically cleans up
-- its test_executions, coverage_reports, and pending_uploads. This requires
-- PRAGMA foreign_keys = ON (set in Database::open).
--
-- Adding new migrations:
-- For additive changes (new tables, new columns with defaults), add statements to
-- this file. For breaking changes, create a new migration file and update the
-- migration runner in db.rs to apply them sequentially.

CREATE TABLE IF NOT EXISTS schema_version (
    version INTEGER PRIMARY KEY,
    applied_at TEXT NOT NULL DEFAULT (datetime('now'))
);

INSERT OR IGNORE INTO schema_version (version) VALUES (1);

CREATE TABLE IF NOT EXISTS test_runs (
    id TEXT PRIMARY KEY,
    branch TEXT,
    commit_sha TEXT,
    ci_provider TEXT,
    framework TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL DEFAULT 'running',
    total INTEGER,
    passed INTEGER,
    failed INTEGER,
    skipped INTEGER,
    duration_ms REAL,
    synced_at TEXT
);

CREATE TABLE IF NOT EXISTS test_executions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES test_runs(id) ON DELETE CASCADE,
    name TEXT NOT NULL,
    status TEXT NOT NULL,
    duration_ms REAL NOT NULL,
    file_path TEXT,
    error_message TEXT,
    retry_count INTEGER NOT NULL DEFAULT 0,
    flaky BOOLEAN NOT NULL DEFAULT 0
);

CREATE INDEX IF NOT EXISTS idx_test_executions_run_id ON test_executions(run_id);
CREATE INDEX IF NOT EXISTS idx_test_executions_name ON test_executions(name);
CREATE INDEX IF NOT EXISTS idx_test_runs_started_at ON test_runs(started_at);
CREATE INDEX IF NOT EXISTS idx_test_runs_branch ON test_runs(branch);

CREATE TABLE IF NOT EXISTS coverage_reports (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES test_runs(id) ON DELETE CASCADE,
    data TEXT,
    format TEXT NOT NULL DEFAULT 'lcov',
    lines_covered INTEGER NOT NULL DEFAULT 0,
    lines_total INTEGER NOT NULL DEFAULT 0,
    branches_covered INTEGER NOT NULL DEFAULT 0,
    branches_total INTEGER NOT NULL DEFAULT 0,
    functions_covered INTEGER NOT NULL DEFAULT 0,
    functions_total INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pending_uploads (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    run_id TEXT NOT NULL REFERENCES test_runs(id) ON DELETE CASCADE,
    payload TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    attempts INTEGER NOT NULL DEFAULT 0,
    last_error TEXT
);
