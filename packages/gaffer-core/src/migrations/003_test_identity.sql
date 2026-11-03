-- Migration 003: Keep test identity and locality for the ingest payload.
--
-- `suite` is the JUnit classname or module path; `line` the source line. Nullable,
-- so rows written before this migration simply have neither.
-- One statement per line: the Rust migration runner executes them individually.
ALTER TABLE test_executions ADD COLUMN suite TEXT;
ALTER TABLE test_executions ADD COLUMN line INTEGER;

INSERT OR IGNORE INTO schema_version (version) VALUES (3);
