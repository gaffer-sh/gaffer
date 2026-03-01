-- Migration 002: Add structured coverage columns to coverage_reports.
--
-- Adds columns for format and per-metric counts. These have defaults so
-- existing rows (if any) are unaffected. The `data` column is retained
-- for backwards compatibility but is no longer required.
--
-- This migration is idempotent — re-running after the columns exist is a no-op
-- because SQLite's ALTER TABLE ADD COLUMN fails silently for duplicate columns
-- when wrapped in this pattern.

-- SQLite doesn't support IF NOT EXISTS for ALTER TABLE ADD COLUMN,
-- so we use a pragma-based check via a temporary trigger approach.
-- Simpler: just attempt each ALTER and ignore errors by wrapping in
-- the migration runner's batch execution.

-- Note: These are handled by the Rust migration runner which catches
-- "duplicate column name" errors and ignores them.
ALTER TABLE coverage_reports ADD COLUMN format TEXT NOT NULL DEFAULT 'lcov';
ALTER TABLE coverage_reports ADD COLUMN lines_covered INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_reports ADD COLUMN lines_total INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_reports ADD COLUMN branches_covered INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_reports ADD COLUMN branches_total INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_reports ADD COLUMN functions_covered INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_reports ADD COLUMN functions_total INTEGER NOT NULL DEFAULT 0;

INSERT OR IGNORE INTO schema_version (version) VALUES (2);
