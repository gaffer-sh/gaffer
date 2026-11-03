use std::collections::BTreeMap;
use std::fmt;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ParsedReport {
    pub framework: String,
    pub summary: Summary,
    pub test_cases: Vec<TestCase>,
    pub metadata: serde_json::Value,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub flaky: usize,
    pub total: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    pub id: String,
    pub name: String,
    pub full_name: String,
    /// The group the format files the test under: a JUnit `classname` or module path.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suite: Option<String>,
    pub status: TestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    /// Stack trace, when the format reports one separately from the message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_stack: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_attempt: Option<u32>,
    /// Wall-clock start of the test, RFC3339. Only formats that record it
    /// (currently Playwright JSON) populate this; everything else leaves it
    /// None rather than guessing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<String>,
    /// Index of the parallel worker that ran the test, when the format reports it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub worker_index: Option<u32>,
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub enum TestStatus {
    Passed,
    Failed,
    Skipped,
    TimedOut,
    Flaky,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ResultType {
    TestReport,
    Coverage,
    Mutation,
    Findings,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DetectionMatch {
    pub parser_id: String,
    pub parser_name: String,
    pub score: u8,
    pub result_type: ResultType,
}

#[derive(Debug)]
pub struct ParseError {
    pub message: String,
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for ParseError {}

impl From<String> for ParseError {
    fn from(message: String) -> Self {
        ParseError { message }
    }
}

// =============================================================================
// Coverage types
// =============================================================================

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoverageMetrics {
    pub covered: i32,
    pub total: i32,
    pub percentage: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FileCoverage {
    pub path: String,
    pub lines: CoverageMetrics,
    pub branches: CoverageMetrics,
    pub functions: CoverageMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReportSummary {
    pub lines: CoverageMetrics,
    pub branches: CoverageMetrics,
    pub functions: CoverageMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CoverageReport {
    pub format: String,
    pub summary: CoverageReportSummary,
    pub files: Vec<FileCoverage>,
}

// =============================================================================
// Mutation types (Stryker mutation-testing-report-schema)
// =============================================================================

/// A mutation testing report, normalised from the Stryker
/// `mutation-testing-report-schema` JSON. Counts and scores are computed here
/// so every consumer agrees on them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MutationReport {
    pub format: String,
    /// As written by the emitter (`"1"`, `"1.0"`, `"2"`); a numeric value becomes its string.
    pub schema_version: String,
    pub thresholds: MutationThresholds,
    pub summary: MutationSummary,
    pub files: Vec<MutationFile>,
    pub tests: Vec<MutationTest>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MutationThresholds {
    pub high: u32,
    pub low: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MutationSummary {
    pub total: usize,
    pub killed: usize,
    pub survived: usize,
    pub no_coverage: usize,
    pub timeout: usize,
    pub compile_error: usize,
    pub runtime_error: usize,
    pub ignored: usize,
    pub pending: usize,
    /// Stryker's mutation score, 0 to 100 with two decimals; `null` when no
    /// mutant was killed, survived, timed out or went uncovered.
    pub score: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MutationFile {
    pub path: String,
    pub score: Option<f64>,
    pub mutants: Vec<Mutant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Mutant {
    pub id: String,
    pub mutator_name: String,
    pub status: MutantStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replacement: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub location: MutationLocation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub covered_by: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub killed_by: Option<Vec<String>>,
}

/// The Stryker `MutantStatus` enum, serialised with its schema spelling.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum MutantStatus {
    Killed,
    Survived,
    NoCoverage,
    Timeout,
    CompileError,
    RuntimeError,
    Ignored,
    Pending,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MutationPosition {
    pub line: u32,
    pub column: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MutationLocation {
    pub start: MutationPosition,
    pub end: MutationPosition,
}

/// A test location. The schema's `openEndLocation` makes `end` optional here.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MutationTestLocation {
    pub start: MutationPosition,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end: Option<MutationPosition>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MutationTest {
    pub id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub location: Option<MutationTestLocation>,
}

// =============================================================================
// Findings types (SARIF 2.1.0 static-analysis output)
// =============================================================================

/// Static-analysis findings normalised from one SARIF 2.1.0 log, with every
/// `runs[]` entry merged. Counts are computed here so every consumer agrees.
///
/// Each finding carries a `fingerprint` used to follow it across uploads:
/// `s1:` plus the tool's `primaryLocationLineHash/v1` partial fingerprint when
/// the tool wrote one, else `g1:` plus a hash of rule id, uri, start line and
/// start column. The `g1:` form shifts when lines above the finding move,
/// which is accepted for now.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FindingsReport {
    pub format: String,
    pub version: String,
    pub tools: Vec<FindingsTool>,
    pub summary: FindingsSummary,
    pub rules: Vec<FindingsRule>,
    pub findings: Vec<Finding>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FindingsTool {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub information_uri: Option<String>,
}

/// `total` includes suppressed findings; `rules_count` counts distinct rule
/// ids among findings, not rules declared by the tools.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FindingsSummary {
    pub total: usize,
    pub error: usize,
    pub warning: usize,
    pub note: usize,
    pub none: usize,
    pub rules_count: usize,
    pub files_count: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct FindingsRule {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub short_description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub help_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_level: Option<FindingLevel>,
}

/// The SARIF `level` enum.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum FindingLevel {
    Error,
    Warning,
    Note,
    None,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Finding {
    pub fingerprint: String,
    pub rule_id: String,
    pub tool_name: String,
    pub level: FindingLevel,
    pub message: String,
    /// `null` when the result has no physical location.
    pub location: Option<FindingLocation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub partial_fingerprints: Option<BTreeMap<String, String>>,
    pub suppressed: bool,
}

/// `uri` has a leading `file:///`, `file://` or `./` stripped; a `uriBaseId`
/// is not resolved, so the uri may be relative to an unknown root.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FindingLocation {
    pub uri: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_column: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_line: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub end_column: Option<u32>,
}

// =============================================================================
// Parse result enum
// =============================================================================

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum ParseResult {
    TestReport(ParsedReport),
    Coverage(CoverageReport),
    Mutation(MutationReport),
    Findings(FindingsReport),
}
