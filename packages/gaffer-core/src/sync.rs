//! Cloud sync — uploads pending test runs to the Gaffer dashboard.
//!
//! ## Design
//! After `end_run()` inserts a pending_upload row, the JS shim calls `sync()`.
//! This module reads all pending uploads from SQLite and POSTs them to the
//! `/api/v1/ingest` endpoint. On success, the pending_upload is deleted and
//! `synced_at` is set on the test run. On failure, the attempt count is
//! incremented (max 5 retries).
//!
//! Uses `ureq` (blocking HTTP) instead of `reqwest` (async) because `sync()`
//! runs after test output — no need for tokio/async complexity.

use serde::Serialize;

use crate::db::Database;
use crate::types::{CoverageMetrics, FileCoverageEntry, SyncResult};

const MAX_ATTEMPTS: i32 = 5;
const DEFAULT_API_URL: &str = "https://app.gaffer.sh";

/// JSON payload matching the dashboard's IngestPayload type.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestPayload {
    pub run_id: String,
    pub framework: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit_sha: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ci_provider: Option<String>,
    pub started_at: String,
    pub finished_at: String,
    pub summary: IngestSummary,
    pub tests: Vec<IngestTest>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<IngestCoverage>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestCoverage {
    pub format: String,
    pub lines: CoverageMetrics,
    pub branches: CoverageMetrics,
    pub functions: CoverageMetrics,
    pub files: Vec<FileCoverageEntry>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestSummary {
    pub total: i32,
    pub passed: i32,
    pub failed: i32,
    pub skipped: i32,
    pub duration_ms: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IngestTest {
    pub name: String,
    pub status: String,
    pub duration_ms: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flaky: Option<bool>,
}

/// Build an IngestPayload from a completed run's data in the database.
pub fn build_ingest_payload(
    db: &Database,
    run_id: &str,
    summary: &crate::types::RunSummary,
) -> Result<IngestPayload, String> {
    let (branch, commit_sha, ci_provider, framework, started_at, finished_at_opt) = db
        .get_run_metadata(run_id)
        .map_err(|e| format!("Failed to get run metadata: {}", e))?;

    let finished_at = finished_at_opt.ok_or("Run has no finished_at timestamp")?;

    let tests = db
        .get_test_executions_for_run(run_id)
        .map_err(|e| format!("Failed to get test executions: {}", e))?;

    let ingest_tests: Vec<IngestTest> = tests
        .into_iter()
        .map(|t| IngestTest {
            name: t.name,
            status: t.status,
            duration_ms: t.duration,
            file_path: t.file_path,
            error: t.error,
            retry_count: t.retry_count,
            flaky: t.flaky,
        })
        .collect();

    // Build optional coverage payload
    let coverage = match db.get_coverage_for_run(run_id) {
        Ok(Some(cov)) => {
            // Try to load per-file data
            let files: Vec<FileCoverageEntry> = match db.get_coverage_files_json(run_id) {
                Ok(Some(json)) => match serde_json::from_str(&json) {
                    Ok(f) => f,
                    Err(e) => {
                        eprintln!("[gaffer] Warning: failed to deserialize coverage files: {}", e);
                        Vec::new()
                    }
                },
                Ok(None) => Vec::new(),
                Err(e) => {
                    eprintln!("[gaffer] Warning: failed to read coverage files from db: {}", e);
                    Vec::new()
                }
            };

            Some(IngestCoverage {
                format: cov.format,
                lines: cov.lines,
                branches: cov.branches,
                functions: cov.functions,
                files,
            })
        }
        Ok(None) => None,
        Err(e) => {
            eprintln!("[gaffer] Warning: failed to read coverage for run {}: {}", run_id, e);
            None
        }
    };

    Ok(IngestPayload {
        run_id: run_id.to_string(),
        framework,
        branch,
        commit_sha,
        ci_provider,
        started_at,
        finished_at,
        summary: IngestSummary {
            total: summary.total,
            passed: summary.passed,
            failed: summary.failed,
            skipped: summary.skipped,
            duration_ms: summary.duration,
        },
        tests: ingest_tests,
        coverage,
    })
}

/// Attempt to sync all pending uploads to the Gaffer dashboard.
pub fn try_sync(db: &Database, token: &str, api_url: Option<&str>) -> SyncResult {
    let base_url = api_url.unwrap_or(DEFAULT_API_URL).trim_end_matches('/');
    let url = format!("{}/api/v1/ingest", base_url);

    let pending = match db.get_pending_uploads(MAX_ATTEMPTS) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[gaffer] Failed to read pending uploads: {}", e);
            return SyncResult {
                synced: 0,
                failed: 1,
            };
        }
    };

    let mut result = SyncResult {
        synced: 0,
        failed: 0,
    };

    // 30-second timeout prevents sync from hanging indefinitely
    let config = ureq::config::Config::builder()
        .timeout_global(Some(std::time::Duration::from_secs(30)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let payload_count = pending.len();
    let sync_start = std::time::Instant::now();

    for (i, upload) in pending.into_iter().enumerate() {
        let payload_bytes = upload.payload.len();
        let req_start = std::time::Instant::now();

        let request = agent
            .post(&url)
            .header("X-API-Key", token)
            .header("Content-Type", "application/json");

        match request.send(upload.payload.as_bytes()) {
            Ok(response) => {
                let elapsed = req_start.elapsed();
                let status = response.status();
                if status == 201 || status == 202 {
                    eprintln!(
                        "[gaffer] Synced run {} ({}/{}, {}KB in {:.1}s)",
                        upload.run_id, i + 1, payload_count,
                        payload_bytes / 1024, elapsed.as_secs_f64()
                    );
                    if let Err(e) = db.mark_synced(upload.id, &upload.run_id) {
                        eprintln!("[gaffer] Failed to mark upload {} as synced: {}", upload.id, e);
                        result.failed += 1;
                    } else {
                        result.synced += 1;
                    }
                } else {
                    let body = match response.into_body().read_to_string() {
                        Ok(b) => b,
                        Err(read_err) => format!("<failed to read response body: {}>", read_err),
                    };
                    let error = format!("HTTP {} ({:.1}s): {}", status, elapsed.as_secs_f64(), body);
                    eprintln!("[gaffer] Sync failed for run {}: {}", upload.run_id, error);
                    if let Err(db_err) = db.record_sync_failure(upload.id, &error) {
                        eprintln!("[gaffer] Failed to record sync failure for upload {}: {}", upload.id, db_err);
                    }
                    result.failed += 1;
                }
            }
            Err(e) => {
                let elapsed = req_start.elapsed();
                let error = format!("{} ({:.1}s)", e, elapsed.as_secs_f64());
                eprintln!("[gaffer] Sync failed for run {}: {}", upload.run_id, error);
                if let Err(db_err) = db.record_sync_failure(upload.id, &error) {
                    eprintln!("[gaffer] Failed to record sync failure for upload {}: {}", upload.id, db_err);
                }
                result.failed += 1;
            }
        }
    }

    if payload_count > 0 {
        eprintln!(
            "[gaffer] Sync complete: {} synced, {} failed in {:.1}s",
            result.synced, result.failed, sync_start.elapsed().as_secs_f64()
        );
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{RunMetadata, RunSummary, TestEvent, status};
    use tempfile::TempDir;

    fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().unwrap();
        let db_path = dir.path().join("data.db");
        let db = Database::open(&db_path).unwrap();
        (db, dir)
    }

    #[test]
    fn build_ingest_payload_from_run() {
        let (db, _dir) = test_db();

        let metadata = RunMetadata {
            branch: Some("main".to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: Some("github-actions".to_string()),
            framework: "vitest".to_string(),
        };
        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();

        db.insert_test(
            "run-1",
            &TestEvent {
                name: "test_a".to_string(),
                status: status::PASSED.to_string(),
                duration: 100.0,
                file_path: Some("src/a.test.ts".to_string()),
                error: None,
                retry_count: None,
                flaky: None,
            },
        )
        .unwrap();

        let summary = RunSummary {
            total: 1,
            passed: 1,
            failed: 0,
            skipped: 0,
            duration: 100.0,
        };
        db.finish_run("run-1", &summary, "2026-02-22T10:01:00Z")
            .unwrap();

        let payload = build_ingest_payload(&db, "run-1", &summary).unwrap();
        assert_eq!(payload.run_id, "run-1");
        assert_eq!(payload.framework, "vitest");
        assert_eq!(payload.branch, Some("main".to_string()));
        assert_eq!(payload.tests.len(), 1);
        assert_eq!(payload.tests[0].name, "test_a");
        assert_eq!(payload.summary.total, 1);
    }

    #[test]
    fn build_ingest_payload_fails_without_finished_at() {
        let (db, _dir) = test_db();

        let metadata = RunMetadata {
            branch: None,
            commit: None,
            ci_provider: None,
            framework: "vitest".to_string(),
        };
        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z")
            .unwrap();

        let summary = RunSummary {
            total: 0,
            passed: 0,
            failed: 0,
            skipped: 0,
            duration: 0.0,
        };

        let result = build_ingest_payload(&db, "run-1", &summary);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("finished_at"));
    }

    #[test]
    fn ingest_payload_serializes_to_camel_case() {
        let payload = IngestPayload {
            run_id: "test-uuid".to_string(),
            framework: "vitest".to_string(),
            branch: Some("main".to_string()),
            commit_sha: None,
            ci_provider: None,
            started_at: "2026-02-22T10:00:00Z".to_string(),
            finished_at: "2026-02-22T10:01:00Z".to_string(),
            summary: IngestSummary {
                total: 1,
                passed: 1,
                failed: 0,
                skipped: 0,
                duration_ms: 100.0,
            },
            tests: vec![IngestTest {
                name: "test_a".to_string(),
                status: "passed".to_string(),
                duration_ms: 100.0,
                file_path: Some("src/a.test.ts".to_string()),
                error: None,
                retry_count: None,
                flaky: None,
            }],
            coverage: None,
        };

        let json = serde_json::to_string(&payload).unwrap();
        // Verify camelCase serialization
        assert!(json.contains("\"runId\""));
        assert!(json.contains("\"startedAt\""));
        assert!(json.contains("\"durationMs\""));
        assert!(json.contains("\"filePath\""));
        // Verify skip_serializing_if works
        assert!(!json.contains("\"commitSha\""));
        assert!(!json.contains("\"ciProvider\""));
        assert!(!json.contains("\"coverage\""));
    }

    #[test]
    fn ingest_payload_includes_coverage_when_present() {
        let (db, _dir) = test_db();

        let metadata = RunMetadata {
            branch: Some("main".to_string()),
            commit: Some("abc123".to_string()),
            ci_provider: None,
            framework: "vitest".to_string(),
        };
        db.insert_run("run-1", &metadata, "2026-02-22T10:00:00Z").unwrap();
        db.insert_test("run-1", &TestEvent {
            name: "test_a".to_string(),
            status: status::PASSED.to_string(),
            duration: 100.0,
            file_path: Some("src/a.test.ts".to_string()),
            error: None,
            retry_count: None,
            flaky: None,
        }).unwrap();

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        db.finish_run("run-1", &summary, "2026-02-22T10:01:00Z").unwrap();

        // Record coverage
        use crate::types::{CoverageMetrics, CoverageSummary};
        let cov = CoverageSummary {
            format: "lcov".to_string(),
            lines: CoverageMetrics { covered: 80, total: 100 },
            branches: CoverageMetrics { covered: 20, total: 30 },
            functions: CoverageMetrics { covered: 15, total: 20 },
        };
        db.record_coverage("run-1", &cov).unwrap();

        let payload = build_ingest_payload(&db, "run-1", &summary).unwrap();
        assert!(payload.coverage.is_some());
        let coverage = payload.coverage.unwrap();
        assert_eq!(coverage.format, "lcov");
        assert_eq!(coverage.lines.covered, 80);
        assert_eq!(coverage.lines.total, 100);
        assert_eq!(coverage.branches.covered, 20);
        assert_eq!(coverage.functions.covered, 15);
    }
}
