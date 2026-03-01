//! Gaffer Core — pure Rust library for test result storage and intelligence.
//!
//! This crate provides the core logic shared by the CLI (`gaffer` binary) and
//! potentially the NAPI module. It has zero Node.js / NAPI dependencies.

pub mod db;
pub mod error;
pub mod intel;
pub mod parsers;
pub mod sync;
pub mod types;

use std::path::PathBuf;
use std::sync::Mutex;

use db::Database;
use error::GafferError;
use types::*;

pub struct GafferCore {
    db: Mutex<Database>,
    config: GafferConfig,
}

impl GafferCore {
    /// Create a new GafferCore instance. Opens (or creates) the SQLite database at
    /// `{config.project_root}/.gaffer/data.db`.
    pub fn new(config: GafferConfig) -> Result<Self, GafferError> {
        let db_path = PathBuf::from(&config.project_root)
            .join(".gaffer")
            .join("data.db");

        let db = Database::open(&db_path)?;

        Ok(GafferCore {
            db: Mutex::new(db),
            config,
        })
    }

    pub fn config(&self) -> &GafferConfig {
        &self.config
    }

    /// Returns true if a cloud sync token is configured.
    pub fn has_token(&self) -> bool {
        self.config.token.is_some()
    }

    fn lock_db(&self) -> Result<std::sync::MutexGuard<'_, Database>, GafferError> {
        self.db.lock().map_err(|_| {
            GafferError::Database(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_LOCKED),
                Some("Database lock was poisoned by a previous failure".to_string()),
            ))
        })
    }

    /// Start a new test run. Returns the generated UUID run_id.
    pub fn start_run(&self, metadata: RunMetadata) -> Result<String, GafferError> {
        let run_id = uuid::Uuid::new_v4().to_string();
        let started_at = chrono::Utc::now().to_rfc3339();

        let db = self.lock_db()?;
        db.insert_run(&run_id, &metadata, &started_at)?;

        Ok(run_id)
    }

    /// Update the framework for a run (detected from parsed report files).
    pub fn update_framework(&self, run_id: &str, framework: &str) -> Result<(), GafferError> {
        let db = self.lock_db()?;
        db.update_run_framework(run_id, framework)?;
        Ok(())
    }

    /// Record a single test result.
    pub fn record_test(&self, run_id: &str, test: &TestEvent) -> Result<(), GafferError> {
        let valid = [status::PASSED, status::FAILED, status::SKIPPED, status::TODO];
        if !valid.contains(&test.status.as_str()) {
            return Err(GafferError::Parse(format!(
                "Invalid test status '{}'. Must be one of: passed, failed, skipped, todo",
                test.status
            )));
        }

        let db = self.lock_db()?;
        db.insert_test(run_id, test)?;

        Ok(())
    }

    /// Finalize a test run. Persists the summary, computes intelligence analytics,
    /// runs cleanup on old data, and queues a cloud sync upload if a token is configured.
    pub fn end_run(&self, run_id: &str, summary: &RunSummary) -> Result<RunReport, GafferError> {
        let finished_at = chrono::Utc::now().to_rfc3339();

        let db = self.lock_db()?;
        db.finish_run(run_id, summary, &finished_at)?;

        let started_at = db.get_run_started_at(run_id)?;

        // Compute intelligence — graceful degradation if any step fails
        let (intelligence, health) = self.compute_intelligence(&db, run_id, summary);

        // Queue cloud sync if token is configured
        if self.config.token.is_some() {
            match sync::build_ingest_payload(&db, run_id, summary) {
                Ok(payload) => {
                    match serde_json::to_string(&payload) {
                        Ok(payload_json) => {
                            if let Err(e) = db.insert_pending_upload(run_id, &payload_json) {
                                eprintln!("[gaffer] Warning: failed to queue sync: {}", e);
                            }
                        }
                        Err(e) => {
                            eprintln!("[gaffer] Warning: failed to serialize sync payload: {}", e);
                        }
                    }
                }
                Err(e) => {
                    eprintln!("[gaffer] Warning: failed to build sync payload: {}", e);
                }
            }
        }

        // Cleanup old runs (retain last 100 or 90 days)
        if let Err(e) = db.cleanup_old_runs(100, 90) {
            eprintln!("[gaffer] Warning: cleanup failed: {}", e);
        }

        Ok(RunReport {
            run_id: run_id.to_string(),
            summary: summary.clone(),
            started_at,
            finished_at,
            intelligence,
            health,
        })
    }

    /// Sync pending uploads to the Gaffer dashboard. No-op if no token is configured.
    pub fn sync(&self) -> Result<SyncResult, GafferError> {
        let token = match &self.config.token {
            Some(t) => t.clone(),
            None => {
                return Ok(SyncResult {
                    synced: 0,
                    failed: 0,
                });
            }
        };

        let db = self.lock_db()?;
        Ok(sync::try_sync(
            &db,
            &token,
            self.config.api_url.as_deref(),
        ))
    }

    /// Record coverage data for a run. Auto-detects coverage format and stores summary.
    pub fn record_coverage(
        &self,
        run_id: &str,
        content: &str,
        filename: &str,
    ) -> Result<CoverageSummary, GafferError> {
        use gaffer_parsers::{ParserRegistry, ParseResult};

        let registry = ParserRegistry::with_defaults();
        let result = registry.parse(content, filename)
            .ok_or_else(|| GafferError::Parse(format!("No coverage parser matched: {}", filename)))?
            .map_err(|e| GafferError::Parse(format!("Failed to parse coverage: {}", e)))?;

        let coverage_report = match result {
            ParseResult::Coverage(report) => report,
            ParseResult::TestReport(report) => {
                return Err(GafferError::Parse(format!(
                    "File '{}' is a test report ({}), not a coverage format. \
                     Check your report_patterns configuration.",
                    filename, report.framework
                )));
            }
        };

        let summary = convert_coverage_summary(&coverage_report);
        let file_entries = convert_coverage_files(&coverage_report);

        let db = self.lock_db()?;
        db.record_coverage(run_id, &summary)?;

        match serde_json::to_string(&file_entries) {
            Ok(files_json) => {
                if let Err(e) = db.store_coverage_files(run_id, &files_json) {
                    eprintln!("[gaffer] Warning: failed to store per-file coverage data: {}", e);
                }
            }
            Err(e) => {
                eprintln!("[gaffer] Warning: failed to serialize coverage files: {}", e);
            }
        }

        Ok(summary)
    }

    /// Query intelligence for a past run (or the latest run if run_id is None).
    pub fn get_test_intelligence(
        &self,
        run_id: Option<&str>,
    ) -> Result<TestIntelligence, GafferError> {
        let db = self.lock_db()?;

        let target_run_id = match run_id {
            Some(id) => id.to_string(),
            None => {
                db.get_latest_finished_run_id()?
                    .ok_or_else(|| GafferError::Parse("No finished runs found".to_string()))?
            }
        };

        self.try_compute_intelligence(&db, &target_run_id)
            .ok_or_else(|| GafferError::Parse("Failed to compute intelligence".to_string()))
    }
}

impl GafferCore {
    fn compute_intelligence(
        &self,
        db: &db::Database,
        run_id: &str,
        summary: &RunSummary,
    ) -> (Option<TestIntelligence>, Option<HealthScore>) {
        let intelligence = self.try_compute_intelligence(db, run_id);
        let health = self.try_compute_health(db, run_id, summary, &intelligence);
        (intelligence, health)
    }

    fn try_compute_intelligence(
        &self,
        db: &db::Database,
        run_id: &str,
    ) -> Option<TestIntelligence> {
        let history = match db.get_historical_test_results(20) {
            Ok(h) => h,
            Err(e) => {
                eprintln!("[gaffer] Warning: failed to query history: {}", e);
                return None;
            }
        };

        let flaky_tests = intel::flaky::detect_flaky_tests(&history);

        let failures = match db.get_failed_tests_for_run(run_id) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("[gaffer] Warning: failed to query failures: {}", e);
                Vec::new()
            }
        };

        let failure_clusters = intel::cluster::cluster_failures(&failures);

        let current_tests: Vec<(String, String, f64)> = history
            .iter()
            .filter(|t| t.run_id == run_id)
            .map(|t| (t.name.clone(), t.file_path.clone(), t.duration_ms))
            .collect();

        let duration_analysis = intel::duration::analyze_duration(&current_tests);

        Some(TestIntelligence {
            flaky_tests,
            failure_clusters,
            duration_analysis,
        })
    }

    fn try_compute_health(
        &self,
        db: &db::Database,
        run_id: &str,
        summary: &RunSummary,
        intelligence: &Option<TestIntelligence>,
    ) -> Option<HealthScore> {
        let flaky_count = intelligence
            .as_ref()
            .map(|i| i.flaky_tests.len() as u32)
            .unwrap_or(0);

        let previous_score = match db.get_previous_health_score(run_id) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gaffer] Warning: failed to query previous health: {}", e);
                None
            }
        };

        Some(intel::health::calculate_health_score(
            summary.total,
            summary.passed,
            flaky_count,
            previous_score,
        ))
    }
}

/// Convert gaffer-parsers CoverageReport → gaffer-core CoverageSummary.
/// Drops the `percentage` field (gaffer-core computes it on read).
fn convert_coverage_summary(report: &gaffer_parsers::CoverageReport) -> CoverageSummary {
    CoverageSummary {
        lines: CoverageMetrics {
            covered: report.summary.lines.covered,
            total: report.summary.lines.total,
        },
        branches: CoverageMetrics {
            covered: report.summary.branches.covered,
            total: report.summary.branches.total,
        },
        functions: CoverageMetrics {
            covered: report.summary.functions.covered,
            total: report.summary.functions.total,
        },
        format: report.format.clone(),
    }
}

/// Convert gaffer-parsers FileCoverage → gaffer-core FileCoverageEntry.
fn convert_coverage_files(report: &gaffer_parsers::CoverageReport) -> Vec<FileCoverageEntry> {
    report
        .files
        .iter()
        .map(|f| FileCoverageEntry {
            path: f.path.clone(),
            lines: CoverageMetrics {
                covered: f.lines.covered,
                total: f.lines.total,
            },
            branches: CoverageMetrics {
                covered: f.branches.covered,
                total: f.branches.total,
            },
            functions: CoverageMetrics {
                covered: f.functions.covered,
                total: f.functions.total,
            },
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::status;
    use tempfile::TempDir;

    fn test_core() -> (GafferCore, TempDir) {
        let dir = TempDir::new().unwrap();
        let config = GafferConfig {
            token: None,
            api_url: None,
            project_root: dir.path().to_string_lossy().to_string(),
        };
        let core = GafferCore::new(config).unwrap();
        (core, dir)
    }

    #[test]
    fn full_lifecycle_start_record_end() {
        let (core, _dir) = test_core();

        let run_id = core
            .start_run(RunMetadata {
                branch: Some("main".to_string()),
                commit: Some("abc123".to_string()),
                ci_provider: None,
                framework: "vitest".to_string(),
            })
            .unwrap();

        assert_eq!(run_id.len(), 36);

        core.record_test(
            &run_id,
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

        core.record_test(
            &run_id,
            &TestEvent {
                name: "test_b".to_string(),
                status: status::FAILED.to_string(),
                duration: 250.0,
                file_path: Some("src/b.test.ts".to_string()),
                error: Some("assertion failed".to_string()),
                retry_count: Some(1),
                flaky: Some(false),
            },
        )
        .unwrap();

        let summary = RunSummary {
            total: 2,
            passed: 1,
            failed: 1,
            skipped: 0,
            duration: 350.0,
        };
        let report = core.end_run(&run_id, &summary).unwrap();

        assert_eq!(report.run_id, run_id);
        assert_eq!(report.summary.total, 2);
        assert!(!report.started_at.is_empty());
        assert!(!report.finished_at.is_empty());
    }

    #[test]
    fn creates_gaffer_directory_and_db() {
        let dir = TempDir::new().unwrap();
        let gaffer_dir = dir.path().join(".gaffer");
        let db_path = gaffer_dir.join("data.db");

        assert!(!gaffer_dir.exists());

        let _core = GafferCore::new(GafferConfig {
            token: None,
            api_url: None,
            project_root: dir.path().to_string_lossy().to_string(),
        })
        .unwrap();

        assert!(gaffer_dir.exists());
        assert!(db_path.exists());
    }

    fn default_metadata() -> RunMetadata {
        RunMetadata {
            branch: None,
            commit: None,
            ci_provider: None,
            framework: "vitest".to_string(),
        }
    }

    #[test]
    fn end_run_returns_intelligence_and_health() {
        let (core, _dir) = test_core();

        let run_id = core.start_run(default_metadata()).unwrap();
        core.record_test(
            &run_id,
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

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        let report = core.end_run(&run_id, &summary).unwrap();

        assert!(report.intelligence.is_some());
        let intel = report.intelligence.unwrap();
        assert!(intel.flaky_tests.is_empty());
        assert!(intel.failure_clusters.is_empty());
        assert_eq!(intel.duration_analysis.slowest_tests.len(), 1);

        assert!(report.health.is_some());
        let health = report.health.unwrap();
        assert!(health.score > 0.0);
        assert_eq!(health.trend, "stable");
    }

    #[test]
    fn end_run_detects_flaky_tests_across_runs() {
        let (core, _dir) = test_core();

        for i in 0..6 {
            let run_id = core.start_run(default_metadata()).unwrap();
            let flaky_status = if i % 2 == 0 { status::PASSED } else { status::FAILED };
            let error = if flaky_status == status::FAILED {
                Some("assertion failed".to_string())
            } else {
                None
            };

            core.record_test(
                &run_id,
                &TestEvent {
                    name: "test_flaky".to_string(),
                    status: flaky_status.to_string(),
                    duration: 100.0,
                    file_path: Some("src/flaky.test.ts".to_string()),
                    error,
                    retry_count: None,
                    flaky: None,
                },
            )
            .unwrap();

            core.record_test(
                &run_id,
                &TestEvent {
                    name: "test_stable".to_string(),
                    status: status::PASSED.to_string(),
                    duration: 50.0,
                    file_path: Some("src/stable.test.ts".to_string()),
                    error: None,
                    retry_count: None,
                    flaky: None,
                },
            )
            .unwrap();

            let total = 2;
            let passed = if flaky_status == status::PASSED { 2 } else { 1 };
            let failed = if flaky_status == status::FAILED { 1 } else { 0 };

            let summary = RunSummary { total, passed, failed, skipped: 0, duration: 150.0 };
            core.end_run(&run_id, &summary).unwrap();
        }

        let intel = core.get_test_intelligence(None).unwrap();
        assert_eq!(intel.flaky_tests.len(), 1);
        assert_eq!(intel.flaky_tests[0].test_name, "test_flaky");
        assert!(intel.flaky_tests[0].flip_rate > 0.5);
    }
}
