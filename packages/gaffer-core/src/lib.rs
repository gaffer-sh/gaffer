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

    // -----------------------------------------------------------------------
    // Query subcommands — on-demand queries for `gaffer query`
    // -----------------------------------------------------------------------

    /// Compute health score from the latest finished run.
    pub fn query_health(&self) -> Result<HealthScore, GafferError> {
        let db = self.lock_db()?;

        let (run_id, total, passed) = db
            .get_latest_run_summary()?
            .ok_or_else(|| GafferError::NotFound("No finished runs found. Run `gaffer test` first.".to_string()))?;

        // Get flaky count from historical data
        let history = db.get_historical_test_results(20)?;
        let flaky_count = intel::flaky::detect_flaky_tests(&history).len() as u32;

        let previous_score = db.get_previous_health_score(&run_id)?;

        Ok(intel::health::calculate_health_score(
            total,
            passed,
            flaky_count,
            previous_score,
        ))
    }

    /// Detect flaky tests from historical runs.
    pub fn query_flaky(&self) -> Result<Vec<FlakyTestResult>, GafferError> {
        let db = self.lock_db()?;
        let history = db.get_historical_test_results(20)?;
        Ok(intel::flaky::detect_flaky_tests(&history))
    }

    /// Analyze test durations from the latest finished run.
    pub fn query_slowest(&self, limit: u32) -> Result<DurationAnalysis, GafferError> {
        let db = self.lock_db()?;

        let history = db.get_historical_test_results(1)?;
        if history.is_empty() {
            return Err(GafferError::NotFound(
                "No finished runs found. Run `gaffer test` first.".to_string(),
            ));
        }

        let current_tests: Vec<(String, String, f64)> = history
            .iter()
            .map(|t| (t.name.clone(), t.file_path.clone(), t.duration_ms))
            .collect();

        let mut analysis = intel::duration::analyze_duration(&current_tests);
        analysis.slowest_tests.truncate(limit as usize);
        Ok(analysis)
    }

    /// List recent finished test runs.
    pub fn query_runs(&self, limit: u32) -> Result<Vec<RecentRun>, GafferError> {
        let db = self.lock_db()?;
        Ok(db.get_recent_runs(limit)?)
    }

    /// Get pass/fail history for a specific test.
    pub fn query_history(
        &self,
        test_pattern: &str,
        limit: u32,
    ) -> Result<Vec<TestHistoryEntry>, GafferError> {
        let db = self.lock_db()?;
        Ok(db.get_test_history(test_pattern, limit)?)
    }

    /// Search failures across runs by name or error pattern.
    pub fn query_failures(
        &self,
        pattern: &str,
        limit: u32,
    ) -> Result<Vec<FailureSearchResult>, GafferError> {
        let db = self.lock_db()?;
        Ok(db.search_failures(pattern, limit)?)
    }

    // -----------------------------------------------------------------------
    // Comparison — `--compare=<branch>` baseline diffing
    // -----------------------------------------------------------------------

    /// Compare the current run against the latest finished run on a baseline branch.
    /// Returns None if no runs exist on the baseline branch.
    pub fn compare_run(
        &self,
        current_run_id: &str,
        current_summary: &RunSummary,
        baseline_branch: &str,
    ) -> Result<Option<ComparisonResult>, GafferError> {
        let db = self.lock_db()?;

        let (baseline_run_id, baseline_summary) = match db
            .get_latest_run_for_branch(baseline_branch, current_run_id)?
        {
            Some(pair) => pair,
            None => return Ok(None),
        };

        let current_statuses = db.get_test_statuses_for_run(current_run_id)?;
        let baseline_statuses = db.get_test_statuses_for_run(&baseline_run_id)?;

        let baseline_map: std::collections::HashMap<String, String> =
            baseline_statuses.into_iter().collect();
        // Current map: last insert wins (handles retries with duplicate names)
        let current_map: std::collections::HashMap<String, String> =
            current_statuses.into_iter().collect();

        let mut new_failures = Vec::new();
        let mut fixed = Vec::new();
        let mut pre_existing_failures = Vec::new();

        for (name, cur_status) in &current_map {
            let cur_failed = cur_status == status::FAILED;
            let baseline_failed = baseline_map
                .get(name)
                .map(|s| s == status::FAILED)
                .unwrap_or(false);

            if cur_failed && baseline_failed {
                pre_existing_failures.push(name.clone());
            } else if cur_failed {
                new_failures.push(name.clone());
            } else if !cur_failed && baseline_failed {
                fixed.push(name.clone());
            }
        }

        // Also check for tests that were in baseline but not in current and were failed
        // (these are "fixed" by removal — but we skip them per plan: only track current tests)

        // Sort for deterministic output
        new_failures.sort();
        fixed.sort();
        pre_existing_failures.sort();

        // Compute deltas
        let pass_rate = |s: &RunSummary| {
            if s.total > 0 { (s.passed as f64 / s.total as f64) * 100.0 } else { 0.0 }
        };
        let current_pass_rate = pass_rate(current_summary);
        let baseline_pass_rate = pass_rate(&baseline_summary);

        Ok(Some(ComparisonResult {
            baseline_branch: baseline_branch.to_string(),
            baseline_run_id,
            new_failures,
            fixed,
            pre_existing_failures,
            pass_rate_delta: current_pass_rate - baseline_pass_rate,
            duration_delta: current_summary.duration - baseline_summary.duration,
            total_delta: current_summary.total - baseline_summary.total,
        }))
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
                    .ok_or_else(|| GafferError::NotFound("No finished runs found".to_string()))?
            }
        };

        self.try_compute_intelligence(&db, &target_run_id)
            .ok_or_else(|| GafferError::NotFound("Failed to compute intelligence".to_string()))
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

    // -----------------------------------------------------------------------
    // Query subcommand tests
    // -----------------------------------------------------------------------

    /// Helper to create a finished run with one passing test.
    fn create_finished_run(core: &GafferCore, test_name: &str, test_status: &str, duration: f64) {
        let run_id = core.start_run(default_metadata()).unwrap();
        let error = if test_status == status::FAILED {
            Some("assertion failed".to_string())
        } else {
            None
        };
        core.record_test(
            &run_id,
            &TestEvent {
                name: test_name.to_string(),
                status: test_status.to_string(),
                duration,
                file_path: Some("src/test.ts".to_string()),
                error,
                retry_count: None,
                flaky: None,
            },
        )
        .unwrap();
        let passed = if test_status == status::PASSED { 1 } else { 0 };
        let failed = if test_status == status::FAILED { 1 } else { 0 };
        let summary = RunSummary {
            total: 1,
            passed,
            failed,
            skipped: 0,
            duration,
        };
        core.end_run(&run_id, &summary).unwrap();
    }

    #[test]
    fn query_health_returns_score() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "test_a", status::PASSED, 100.0);

        let health = core.query_health().unwrap();
        // 1 passed, 0 failed, 0 flaky → near-perfect score
        assert!(health.score >= 80.0, "Expected score >= 80 for all-passing run, got {}", health.score);
        assert!(!health.label.is_empty());
    }

    #[test]
    fn query_health_no_data_returns_error() {
        let (core, _dir) = test_core();
        assert!(core.query_health().is_err());
    }

    #[test]
    fn query_flaky_returns_empty_for_stable() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "test_a", status::PASSED, 100.0);

        let flaky = core.query_flaky().unwrap();
        assert!(flaky.is_empty());
    }

    #[test]
    fn query_slowest_returns_analysis() {
        let (core, _dir) = test_core();

        let run_id = core.start_run(default_metadata()).unwrap();
        for i in 1..=5 {
            core.record_test(
                &run_id,
                &TestEvent {
                    name: format!("test_{}", i),
                    status: status::PASSED.to_string(),
                    duration: i as f64 * 100.0,
                    file_path: Some("src/test.ts".to_string()),
                    error: None,
                    retry_count: None,
                    flaky: None,
                },
            )
            .unwrap();
        }
        let summary = RunSummary {
            total: 5,
            passed: 5,
            failed: 0,
            skipped: 0,
            duration: 1500.0,
        };
        core.end_run(&run_id, &summary).unwrap();

        let analysis = core.query_slowest(3).unwrap();
        assert_eq!(analysis.slowest_tests.len(), 3);
        assert_eq!(analysis.slowest_tests[0].duration_ms, 500.0);
    }

    #[test]
    fn query_slowest_no_data_returns_error() {
        let (core, _dir) = test_core();
        let err = core.query_slowest(10).unwrap_err();
        assert!(err.to_string().contains("No finished runs found"));
    }

    #[test]
    fn query_runs_lists_recent() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "test_a", status::PASSED, 100.0);
        create_finished_run(&core, "test_b", status::PASSED, 200.0);

        let runs = core.query_runs(10).unwrap();
        assert_eq!(runs.len(), 2);
    }

    #[test]
    fn query_runs_empty_db() {
        let (core, _dir) = test_core();
        let runs = core.query_runs(10).unwrap();
        assert!(runs.is_empty());
    }

    #[test]
    fn query_history_finds_test() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "auth > login", status::PASSED, 100.0);
        create_finished_run(&core, "auth > login", status::FAILED, 200.0);
        create_finished_run(&core, "auth > logout", status::PASSED, 50.0);

        let history = core.query_history("login", 50).unwrap();
        assert_eq!(history.len(), 2);
    }

    #[test]
    fn query_history_no_match() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "test_a", status::PASSED, 100.0);

        let history = core.query_history("nonexistent", 50).unwrap();
        assert!(history.is_empty());
    }

    #[test]
    fn query_failures_finds_by_name() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "auth > login", status::FAILED, 100.0);
        create_finished_run(&core, "auth > logout", status::PASSED, 50.0);

        let failures = core.query_failures("login", 50).unwrap();
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].name, "auth > login");
    }

    #[test]
    fn query_failures_no_match() {
        let (core, _dir) = test_core();
        create_finished_run(&core, "test_a", status::PASSED, 100.0);

        let failures = core.query_failures("anything", 50).unwrap();
        assert!(failures.is_empty());
    }

    // -----------------------------------------------------------------------
    // Comparison tests
    // -----------------------------------------------------------------------

    /// Helper to create a finished run on a specific branch with multiple tests.
    fn create_run_on_branch(
        core: &GafferCore,
        branch: &str,
        tests: &[(&str, &str, f64)],
    ) -> String {
        let run_id = core
            .start_run(RunMetadata {
                branch: Some(branch.to_string()),
                commit: Some("abc123".to_string()),
                ci_provider: None,
                framework: "vitest".to_string(),
            })
            .unwrap();

        let mut passed = 0i32;
        let mut failed = 0i32;
        for (name, test_status, duration) in tests {
            let error = if *test_status == status::FAILED {
                Some("assertion failed".to_string())
            } else {
                None
            };
            core.record_test(
                &run_id,
                &TestEvent {
                    name: name.to_string(),
                    status: test_status.to_string(),
                    duration: *duration,
                    file_path: Some("src/test.ts".to_string()),
                    error,
                    retry_count: None,
                    flaky: None,
                },
            )
            .unwrap();
            if *test_status == status::PASSED {
                passed += 1;
            } else if *test_status == status::FAILED {
                failed += 1;
            }
        }
        let total = tests.len() as i32;
        let duration: f64 = tests.iter().map(|(_, _, d)| d).sum();
        let summary = RunSummary {
            total,
            passed,
            failed,
            skipped: total - passed - failed,
            duration,
        };
        core.end_run(&run_id, &summary).unwrap();
        run_id
    }

    #[test]
    fn compare_run_returns_none_when_no_baseline() {
        let (core, _dir) = test_core();
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_a", status::PASSED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn compare_run_detects_new_failures() {
        let (core, _dir) = test_core();
        // Baseline on main: test_a passes, test_b passes
        create_run_on_branch(
            &core,
            "main",
            &[
                ("test_a", status::PASSED, 100.0),
                ("test_b", status::PASSED, 100.0),
            ],
        );

        // Current run on feat: test_a fails, test_b passes
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[
                ("test_a", status::FAILED, 100.0),
                ("test_b", status::PASSED, 100.0),
            ],
        );

        let summary = RunSummary { total: 2, passed: 1, failed: 1, skipped: 0, duration: 200.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert_eq!(result.new_failures, vec!["test_a"]);
        assert!(result.fixed.is_empty());
        assert!(result.pre_existing_failures.is_empty());
    }

    #[test]
    fn compare_run_detects_fixed_tests() {
        let (core, _dir) = test_core();
        // Baseline on main: test_a fails
        create_run_on_branch(
            &core,
            "main",
            &[("test_a", status::FAILED, 100.0)],
        );

        // Current: test_a passes
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_a", status::PASSED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert!(result.new_failures.is_empty());
        assert_eq!(result.fixed, vec!["test_a"]);
        assert!(result.pre_existing_failures.is_empty());
    }

    #[test]
    fn compare_run_detects_pre_existing_failures() {
        let (core, _dir) = test_core();
        // Baseline: test_a fails
        create_run_on_branch(
            &core,
            "main",
            &[("test_a", status::FAILED, 100.0)],
        );

        // Current: test_a still fails
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_a", status::FAILED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 0, failed: 1, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert!(result.new_failures.is_empty());
        assert!(result.fixed.is_empty());
        assert_eq!(result.pre_existing_failures, vec!["test_a"]);
    }

    #[test]
    fn compare_run_computes_deltas() {
        let (core, _dir) = test_core();
        // Baseline: 10 tests, 8 passed, 2 failed, 1000ms
        create_run_on_branch(
            &core,
            "main",
            &[
                ("t1", status::PASSED, 100.0),
                ("t2", status::PASSED, 100.0),
                ("t3", status::PASSED, 100.0),
                ("t4", status::PASSED, 100.0),
                ("t5", status::PASSED, 100.0),
                ("t6", status::PASSED, 100.0),
                ("t7", status::PASSED, 100.0),
                ("t8", status::PASSED, 100.0),
                ("t9", status::FAILED, 100.0),
                ("t10", status::FAILED, 100.0),
            ],
        );

        // Current: 12 tests, 11 passed, 1 failed, 1500ms
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[
                ("t1", status::PASSED, 125.0),
                ("t2", status::PASSED, 125.0),
                ("t3", status::PASSED, 125.0),
                ("t4", status::PASSED, 125.0),
                ("t5", status::PASSED, 125.0),
                ("t6", status::PASSED, 125.0),
                ("t7", status::PASSED, 125.0),
                ("t8", status::PASSED, 125.0),
                ("t9", status::PASSED, 125.0),
                ("t10", status::FAILED, 125.0),
                ("t11", status::PASSED, 125.0),
                ("t12", status::PASSED, 125.0),
            ],
        );

        let summary = RunSummary { total: 12, passed: 11, failed: 1, skipped: 0, duration: 1500.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        // Pass rate: current 11/12 = 91.67%, baseline 8/10 = 80% → delta = +11.67
        assert!((result.pass_rate_delta - 11.67).abs() < 0.1);
        // Duration: 1500 - 1000 = 500
        assert!((result.duration_delta - 500.0).abs() < 0.01);
        // Total: 12 - 10 = 2
        assert_eq!(result.total_delta, 2);
    }

    #[test]
    fn compare_run_same_branch_excludes_current() {
        let (core, _dir) = test_core();
        // First run on main
        create_run_on_branch(
            &core,
            "main",
            &[("test_a", status::PASSED, 100.0)],
        );

        // Second run also on main — should compare against first, not itself
        let run_id = create_run_on_branch(
            &core,
            "main",
            &[("test_a", status::FAILED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 0, failed: 1, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert_eq!(result.new_failures, vec!["test_a"]);
    }

    #[test]
    fn compare_run_new_test_absent_from_baseline_is_new_failure() {
        let (core, _dir) = test_core();
        // Baseline: only test_a
        create_run_on_branch(&core, "main", &[("test_a", status::PASSED, 100.0)]);

        // Current: test_a passes, test_b (brand new) fails
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[
                ("test_a", status::PASSED, 100.0),
                ("test_b", status::FAILED, 100.0),
            ],
        );

        let summary = RunSummary { total: 2, passed: 1, failed: 1, skipped: 0, duration: 200.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert_eq!(result.new_failures, vec!["test_b"]);
        assert!(result.fixed.is_empty());
        assert!(result.pre_existing_failures.is_empty());
    }

    #[test]
    fn compare_run_removed_test_not_in_fixed() {
        let (core, _dir) = test_core();
        // Baseline: test_a fails, test_b passes
        create_run_on_branch(
            &core,
            "main",
            &[
                ("test_a", status::FAILED, 100.0),
                ("test_b", status::PASSED, 100.0),
            ],
        );

        // Current: only test_b (test_a removed entirely)
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_b", status::PASSED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        // test_a was failing on baseline but removed — should NOT appear in fixed
        assert!(result.new_failures.is_empty());
        assert!(result.fixed.is_empty());
        assert!(result.pre_existing_failures.is_empty());
    }

    #[test]
    fn compare_run_populates_baseline_fields() {
        let (core, _dir) = test_core();
        let baseline_run_id = create_run_on_branch(
            &core,
            "main",
            &[("test_a", status::PASSED, 100.0)],
        );

        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_a", status::PASSED, 100.0)],
        );

        let summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 100.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        assert_eq!(result.baseline_branch, "main");
        assert_eq!(result.baseline_run_id, baseline_run_id);
    }

    #[test]
    fn compare_run_skipped_test_not_classified_as_failure() {
        let (core, _dir) = test_core();
        // Baseline: test_a passes
        create_run_on_branch(&core, "main", &[("test_a", status::PASSED, 100.0)]);

        // Current: test_a is skipped
        let run_id = create_run_on_branch(
            &core,
            "feat",
            &[("test_a", status::SKIPPED, 0.0)],
        );

        let summary = RunSummary { total: 1, passed: 0, failed: 0, skipped: 1, duration: 0.0 };
        let result = core.compare_run(&run_id, &summary, "main").unwrap().unwrap();

        // Skipped is not failed — should not appear in any failure bucket
        assert!(result.new_failures.is_empty());
        assert!(result.fixed.is_empty());
        assert!(result.pre_existing_failures.is_empty());
    }

    #[test]
    fn compare_run_retry_uses_last_status() {
        let (core, _dir) = test_core();
        create_run_on_branch(&core, "main", &[("test_a", status::PASSED, 100.0)]);

        // Current: test_a fails first, then passes on retry (two records, same name)
        let meta = RunMetadata {
            branch: Some("feat".to_string()),
            commit: None,
            ci_provider: None,
            framework: "test".to_string(),
        };
        let run_id = core.start_run(meta).unwrap();
        // First attempt: failed
        core.record_test(&run_id, &TestEvent {
            name: "test_a".to_string(),
            status: status::FAILED.to_string(),
            duration: 100.0,
            file_path: Some("test.rs".to_string()),
            error: Some("timeout".to_string()),
            retry_count: Some(0),
            flaky: Some(false),
        }).unwrap();
        // Retry: passed
        core.record_test(&run_id, &TestEvent {
            name: "test_a".to_string(),
            status: status::PASSED.to_string(),
            duration: 50.0,
            file_path: Some("test.rs".to_string()),
            error: None,
            retry_count: Some(1),
            flaky: Some(true),
        }).unwrap();
        let run_summary = RunSummary { total: 1, passed: 1, failed: 0, skipped: 0, duration: 150.0 };
        core.end_run(&run_id, &run_summary).unwrap();

        let result = core.compare_run(&run_id, &run_summary, "main").unwrap().unwrap();

        // Last status is PASSED — should not be a new failure
        assert!(result.new_failures.is_empty());
        assert!(result.fixed.is_empty());
    }
}
