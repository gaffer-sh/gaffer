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
    pub status: TestStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_attempt: Option<u32>,
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
// Parse result enum
// =============================================================================

#[derive(Debug, Serialize, PartialEq)]
#[serde(tag = "type", content = "data", rename_all = "camelCase")]
pub enum ParseResult {
    TestReport(ParsedReport),
    Coverage(CoverageReport),
}
