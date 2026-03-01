use serde::Deserialize;

use crate::detect::{extract_json_keys_at_depth, extract_json_top_level_keys};
use crate::registry::Parser;
use crate::types::{ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus};

pub struct CtrfParser;

impl Parser for CtrfParser {
    fn id(&self) -> &str {
        "ctrf"
    }

    fn name(&self) -> &str {
        "CTRF JSON Report"
    }

    fn priority(&self) -> u8 {
        84
    }

    fn result_type(&self) -> ResultType {
        ResultType::TestReport
    }

    fn detect(&self, content: &str, filename: &str) -> u8 {
        if !filename.ends_with(".json") {
            return 0;
        }

        let top_keys = extract_json_top_level_keys(content);

        // Tier 1: explicit reportFormat marker with "CTRF" value nearby
        if top_keys.iter().any(|k| k == "reportFormat") && content.contains("\"CTRF\"") {
            return 95;
        }

        // Tier 2: structural match — "results" at depth 1, "tests" at depth 2
        if top_keys.iter().any(|k| k == "results") {
            let depth2_keys = extract_json_keys_at_depth(content, 2);
            if depth2_keys.iter().any(|k| k == "tests") {
                return 70;
            }
        }

        0
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let report: CtrfFile = serde_json::from_str(content)
            .map_err(|e| ParseError::from(format!("Invalid CTRF JSON: {}", e)))?;

        let framework = report.results.tool.name.clone();

        let test_cases: Vec<TestCase> = report.results.tests
            .iter()
            .enumerate()
            .map(|(index, test)| {
                let status = map_status(&test.status, test.flaky);
                let full_name = build_full_name(&test.name, &test.suite);

                let duration_ms = if test.duration.is_finite() && test.duration >= 0.0 {
                    Some(test.duration.round() as u64)
                } else {
                    None
                };

                let error_message = if let Some(ref msg) = test.message {
                    if !msg.is_empty() {
                        Some(msg.clone())
                    } else {
                        trace_first_line(&test.trace)
                    }
                } else {
                    trace_first_line(&test.trace)
                };

                TestCase {
                    id: test.id.clone().unwrap_or_else(|| format!("tc-{}", index + 1)),
                    name: test.name.clone(),
                    full_name,
                    status,
                    duration_ms,
                    error_message,
                    file_path: test.file_path.clone(),
                    line: test.line,
                    retry_attempt: test.retries.map(|r| r as u32),
                }
            })
            .collect();

        let summary = calculate_summary(&report.results.summary, &test_cases);

        let metadata = extract_metadata(&report);

        Ok(ParseResult::TestReport(ParsedReport {
            framework,
            summary,
            test_cases,
            metadata,
        }))
    }
}

fn map_status(status: &str, flaky: Option<bool>) -> TestStatus {
    if flaky == Some(true) {
        return TestStatus::Flaky;
    }
    match status {
        "passed" => TestStatus::Passed,
        "failed" => TestStatus::Failed,
        "skipped" | "pending" | "other" => TestStatus::Skipped,
        _ => TestStatus::Skipped,
    }
}

fn build_full_name(name: &str, suite: &CtrfSuite) -> String {
    match suite {
        CtrfSuite::Array(parts) if !parts.is_empty() => {
            let mut full: Vec<&str> = parts.iter().map(|s| s.as_str()).collect();
            full.push(name);
            full.join(" > ")
        }
        _ => name.to_string(),
    }
}

fn trace_first_line(trace: &Option<String>) -> Option<String> {
    trace.as_ref().and_then(|t| {
        if t.is_empty() {
            None
        } else {
            Some(t.lines().next().unwrap_or(t).to_string())
        }
    })
}

fn calculate_summary(summary: &CtrfSummary, test_cases: &[TestCase]) -> Summary {
    // Duration: explicit > stop-start > sum of test durations
    let mut duration_ms = summary.duration.unwrap_or(0.0);
    if duration_ms == 0.0 && summary.stop > summary.start && summary.start > 0.0 {
        duration_ms = summary.stop - summary.start;
    }
    if duration_ms == 0.0 {
        duration_ms = test_cases.iter()
            .map(|tc| tc.duration_ms.unwrap_or(0) as f64)
            .sum();
    }

    let duration_ms_val = if duration_ms > 0.0 && duration_ms.is_finite() {
        Some(duration_ms.round() as u64)
    } else {
        None
    };

    // Flaky: summary.flaky if present, else count from test cases
    let flaky_count = test_cases.iter().filter(|tc| tc.status == TestStatus::Flaky).count();
    let flaky = summary.flaky.unwrap_or(flaky_count);

    Summary {
        passed: summary.passed,
        failed: summary.failed,
        skipped: summary.skipped + summary.pending + summary.other,
        flaky,
        total: summary.tests,
        duration_ms: duration_ms_val,
    }
}

fn extract_metadata(report: &CtrfFile) -> serde_json::Value {
    let mut metadata = serde_json::Map::new();

    metadata.insert("specVersion".to_string(), serde_json::json!(report.spec_version));
    metadata.insert("toolName".to_string(), serde_json::json!(report.results.tool.name));

    if let Some(ref id) = report.report_id {
        metadata.insert("reportId".to_string(), serde_json::json!(id));
    }
    if let Some(ref ts) = report.timestamp {
        metadata.insert("timestamp".to_string(), serde_json::json!(ts));
    }
    if let Some(ref gb) = report.generated_by {
        metadata.insert("generatedBy".to_string(), serde_json::json!(gb));
    }

    if let Some(ref ver) = report.results.tool.version {
        metadata.insert("toolVersion".to_string(), serde_json::json!(ver));
    }
    if let Some(ref extra) = report.results.tool.extra {
        metadata.insert("toolExtra".to_string(), extra.clone());
    }

    if let Some(suites) = report.results.summary.suites {
        metadata.insert("suiteCount".to_string(), serde_json::json!(suites));
    }
    if let Some(ref extra) = report.results.summary.extra {
        metadata.insert("summaryExtra".to_string(), extra.clone());
    }

    if let Some(ref env) = report.results.environment {
        if let Some(ref v) = env.app_name { metadata.insert("appName".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.app_version { metadata.insert("appVersion".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.build_name { metadata.insert("buildName".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.build_number { metadata.insert("buildNumber".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.build_url { metadata.insert("buildUrl".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.repository_name { metadata.insert("repositoryName".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.repository_url { metadata.insert("repositoryUrl".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.commit { metadata.insert("commit".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.branch_name { metadata.insert("branchName".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.os_platform { metadata.insert("osPlatform".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.os_release { metadata.insert("osRelease".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.os_version { metadata.insert("osVersion".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.test_environment { metadata.insert("testEnvironment".to_string(), serde_json::json!(v)); }
        if let Some(ref v) = env.extra { metadata.insert("environmentExtra".to_string(), v.clone()); }
    }

    serde_json::Value::Object(metadata)
}

// ============================================================================
// Input deserialization structs
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfFile {
    #[serde(default)]
    #[allow(dead_code)]
    report_format: Option<String>,
    #[serde(default)]
    spec_version: Option<String>,
    #[serde(default)]
    report_id: Option<String>,
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(default)]
    generated_by: Option<String>,
    results: CtrfResults,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfResults {
    tool: CtrfTool,
    summary: CtrfSummary,
    tests: Vec<CtrfTest>,
    #[serde(default)]
    environment: Option<CtrfEnvironment>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfTool {
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfSummary {
    #[serde(default)]
    tests: usize,
    #[serde(default)]
    passed: usize,
    #[serde(default)]
    failed: usize,
    #[serde(default)]
    skipped: usize,
    #[serde(default)]
    pending: usize,
    #[serde(default)]
    other: usize,
    #[serde(default)]
    flaky: Option<usize>,
    #[serde(default)]
    suites: Option<usize>,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    stop: f64,
    #[serde(default)]
    duration: Option<f64>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfTest {
    name: String,
    status: String,
    #[serde(default)]
    duration: f64,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    trace: Option<String>,
    #[serde(default)]
    file_path: Option<String>,
    #[serde(default)]
    line: Option<u64>,
    #[serde(default)]
    retries: Option<i32>,
    #[serde(default)]
    flaky: Option<bool>,
    #[serde(default, deserialize_with = "deserialize_suite")]
    suite: CtrfSuite,
}

/// Suite can be a single string or an array of strings in CTRF.
/// We normalize to our enum for uniform handling.
#[derive(Debug, Deserialize)]
#[serde(untagged)]
#[allow(dead_code)]
enum CtrfSuite {
    Single(String),
    Array(Vec<String>),
}

impl Default for CtrfSuite {
    fn default() -> Self {
        CtrfSuite::Array(Vec::new())
    }
}

fn deserialize_suite<'de, D>(deserializer: D) -> Result<CtrfSuite, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum RawSuite {
        Single(String),
        Array(Vec<String>),
    }

    match Option::<RawSuite>::deserialize(deserializer)? {
        Some(RawSuite::Single(s)) => {
            if s.is_empty() {
                Ok(CtrfSuite::Array(Vec::new()))
            } else {
                Ok(CtrfSuite::Array(vec![s]))
            }
        }
        Some(RawSuite::Array(a)) => Ok(CtrfSuite::Array(a)),
        None => Ok(CtrfSuite::Array(Vec::new())),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CtrfEnvironment {
    #[serde(default)]
    app_name: Option<String>,
    #[serde(default)]
    app_version: Option<String>,
    #[serde(default)]
    build_name: Option<String>,
    #[serde(default)]
    build_number: Option<String>,
    #[serde(default)]
    build_url: Option<String>,
    #[serde(default)]
    repository_name: Option<String>,
    #[serde(default)]
    repository_url: Option<String>,
    #[serde(default)]
    commit: Option<String>,
    #[serde(default)]
    branch_name: Option<String>,
    #[serde(default)]
    os_platform: Option<String>,
    #[serde(default)]
    os_release: Option<String>,
    #[serde(default)]
    os_version: Option<String>,
    #[serde(default)]
    test_environment: Option<String>,
    #[serde(default)]
    extra: Option<serde_json::Value>,
}
