-- Migration 004: Constrain test_runs.status to the RunStatus enum (types.rs).
--
-- SQLite cannot add a CHECK to an existing column, so the table is rebuilt and its
-- rows copied. The runner runs this in one transaction with foreign_keys OFF, so
-- dropping the old table does not cascade-delete test_executions and friends.
-- Keep the CHECK list in step with RunStatus::as_str.
CREATE TABLE test_runs_new (
    id TEXT PRIMARY KEY,
    branch TEXT,
    commit_sha TEXT,
    ci_provider TEXT,
    framework TEXT NOT NULL,
    started_at TEXT NOT NULL,
    finished_at TEXT,
    status TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'finished', 'failed', 'interrupted')),
    total INTEGER,
    passed INTEGER,
    failed INTEGER,
    skipped INTEGER,
    duration_ms REAL,
    synced_at TEXT
);

INSERT INTO test_runs_new (id, branch, commit_sha, ci_provider, framework, started_at,
                           finished_at, status, total, passed, failed, skipped, duration_ms, synced_at)
SELECT id, branch, commit_sha, ci_provider, framework, started_at,
       finished_at, status, total, passed, failed, skipped, duration_ms, synced_at
FROM test_runs;

DROP TABLE test_runs;
ALTER TABLE test_runs_new RENAME TO test_runs;

CREATE INDEX IF NOT EXISTS idx_test_runs_started_at ON test_runs(started_at);
CREATE INDEX IF NOT EXISTS idx_test_runs_branch ON test_runs(branch);

INSERT OR IGNORE INTO schema_version (version) VALUES (4);
