//! Type definitions shared between the core library and consumers (CLI, NAPI, etc.).

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GafferConfig {
    /// Authentication token for cloud sync
    pub token: Option<String>,
    /// API URL for cloud sync (defaults to https://app.gaffer.sh)
    pub api_url: Option<String>,
    /// Project root directory — the DB is created at `{project_root}/.gaffer/data.db`
    pub project_root: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunMetadata {
    /// Git branch name. None when not in a git repo or branch can't be detected.
    pub branch: Option<String>,
    /// Git commit SHA. None when not in a git repo.
    pub commit: Option<String>,
    /// CI provider name (e.g. "github-actions", "gitlab-ci"). None for local runs.
    pub ci_provider: Option<String>,
    /// Test framework identifier (e.g. "vitest", "playwright", "jest")
    pub framework: String,
}

/// String constants for test execution status values.
pub mod status {
    pub const PASSED: &str = "passed";
    pub const FAILED: &str = "failed";
    pub const SKIPPED: &str = "skipped";
    pub const TODO: &str = "todo";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestEvent {
    /// Fully qualified test name including describe blocks
    pub name: String,
    /// One of: "passed", "failed", "skipped", "todo" — see `status` module constants
    pub status: String,
    /// Duration in milliseconds
    pub duration: f64,
    /// File path relative to project root
    pub file_path: Option<String>,
    /// Error message including stack trace (if failed)
    pub error: Option<String>,
    /// Number of retries attempted by the framework's retry mechanism
    pub retry_count: Option<i32>,
    /// Whether the framework detected this as flaky (passed after retry)
    pub flaky: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunSummary {
    pub total: i32,
    pub passed: i32,
    pub failed: i32,
    pub skipped: i32,
    /// Total wall-clock duration of the test run in milliseconds
    pub duration: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunReport {
    pub run_id: String,
    pub summary: RunSummary,
    /// ISO 8601 timestamp when the run started
    pub started_at: String,
    /// ISO 8601 timestamp when the run finished
    pub finished_at: String,
    /// Test intelligence results. None if computation fails or no data available.
    pub intelligence: Option<TestIntelligence>,
    /// Health score for this run. None if computation fails.
    pub health: Option<HealthScore>,
}

// =============================================================================
// Intelligence types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestIntelligence {
    pub flaky_tests: Vec<FlakyTestResult>,
    pub failure_clusters: Vec<FailureCluster>,
    pub duration_analysis: DurationAnalysis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FlakyTestResult {
    pub test_name: String,
    pub file_path: String,
    pub flip_rate: f64,
    pub flip_count: u32,
    pub total_runs: u32,
    pub composite_score: f64,
    pub last_flipped_at: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureCluster {
    pub pattern: String,
    pub count: u32,
    pub test_names: Vec<String>,
    pub file_paths: Vec<String>,
    pub similarity: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DurationAnalysis {
    pub p50: f64,
    pub p75: f64,
    pub p90: f64,
    pub p95: f64,
    pub p99: f64,
    pub mean: f64,
    pub slowest_tests: Vec<SlowestTest>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SlowestTest {
    pub test_name: String,
    pub file_path: String,
    pub duration_ms: f64,
    pub percentile: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthScore {
    pub score: f64,
    pub label: String,
    pub trend: String,
    pub previous_score: Option<f64>,
}

// =============================================================================
// Query types — returned by `gaffer query` subcommands
// =============================================================================

/// A recent test run summary, returned by `gaffer query runs`.
/// Only includes finished runs (status = 'finished'), so `finished_at` is always present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecentRun {
    pub id: String,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub framework: String,
    pub started_at: String,
    pub finished_at: String,
    pub total: i32,
    pub passed: i32,
    pub failed: i32,
    pub skipped: i32,
    pub duration_ms: f64,
}

/// A single test execution in historical context, returned by `gaffer query history`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TestHistoryEntry {
    pub name: String,
    /// One of: "passed", "failed", "skipped", "todo" — see `status` module constants
    pub status: String,
    pub duration_ms: f64,
    pub error_message: Option<String>,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub started_at: String,
}

/// A failure search result across runs, returned by `gaffer query failures`.
/// Status is implicitly always "failed" (the query filters by `te.status = 'failed'`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FailureSearchResult {
    pub name: String,
    pub file_path: Option<String>,
    pub error_message: Option<String>,
    pub duration_ms: f64,
    pub branch: Option<String>,
    pub commit_sha: Option<String>,
    pub started_at: String,
}

// =============================================================================
// Comparison types
// =============================================================================

/// Result of comparing current run against a baseline branch.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonResult {
    pub baseline_branch: String,
    pub baseline_run_id: String,
    /// Failed now, passed (or absent) on baseline
    pub new_failures: Vec<String>,
    /// Passed now, failed on baseline
    pub fixed: Vec<String>,
    /// Failed in both current and baseline
    pub pre_existing_failures: Vec<String>,
    /// Change in pass rate (percentage points, current - baseline)
    pub pass_rate_delta: f64,
    /// Change in total duration in ms (current - baseline)
    pub duration_delta: f64,
    /// Change in total test count (current - baseline)
    pub total_delta: i32,
}

// =============================================================================
// Cloud sync types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncResult {
    pub synced: u32,
    pub failed: u32,
}

/// A pending upload row from SQLite (internal).
#[derive(Debug, Clone)]
pub struct PendingUpload {
    pub id: i64,
    pub run_id: String,
    pub payload: String,
    pub attempts: i32,
}

// =============================================================================
// Coverage types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageMetrics {
    pub covered: i32,
    pub total: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageSummary {
    pub lines: CoverageMetrics,
    pub branches: CoverageMetrics,
    pub functions: CoverageMetrics,
    pub format: String,
}

/// Per-file coverage entry for cloud sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileCoverageEntry {
    pub path: String,
    pub lines: CoverageMetrics,
    pub branches: CoverageMetrics,
    pub functions: CoverageMetrics,
}
