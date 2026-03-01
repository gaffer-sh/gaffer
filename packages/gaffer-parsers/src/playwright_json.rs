use serde::Deserialize;

use crate::registry::Parser;
use crate::types::{ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus};

pub struct PlaywrightJsonParser;

impl Parser for PlaywrightJsonParser {
    fn id(&self) -> &str {
        "playwright-json"
    }

    fn name(&self) -> &str {
        "Playwright JSON Report"
    }

    fn priority(&self) -> u8 {
        86
    }

    fn result_type(&self) -> ResultType {
        ResultType::TestReport
    }

    fn detect(&self, content: &str, filename: &str) -> u8 {
        if !filename.ends_with(".json") {
            return 0;
        }

        let keys = crate::detect::extract_json_top_level_keys(content);
        let has_config = keys.iter().any(|k| k == "config");
        let has_suites = keys.iter().any(|k| k == "suites");
        let has_stats = keys.iter().any(|k| k == "stats");

        if has_config && has_suites && has_stats { 90 } else { 0 }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let report: PlaywrightJsonReport = serde_json::from_str(content)
            .map_err(|e| ParseError::from(format!("Invalid Playwright JSON: {}", e)))?;

        let multi_project = report.config.projects.len() > 1;

        let mut test_cases: Vec<TestCase> = Vec::new();

        for suite in &report.suites {
            collect_test_cases(suite, &[], multi_project, &mut test_cases);
        }

        let project_names: Vec<String> = report.config.projects
            .iter()
            .filter(|p| !p.name.is_empty())
            .map(|p| p.name.clone())
            .collect();

        let summary = Summary {
            passed: report.stats.expected,
            failed: report.stats.unexpected,
            skipped: report.stats.skipped,
            flaky: report.stats.flaky,
            total: report.stats.expected + report.stats.unexpected + report.stats.flaky + report.stats.skipped,
            duration_ms: Some(report.stats.duration.max(0.0).round() as u64),
        };

        let mut metadata = serde_json::json!({
            "ok": report.stats.unexpected == 0,
            "startTime": report.stats.start_time,
        });

        if !report.errors.is_empty() {
            let error_messages: Vec<&str> = report.errors.iter()
                .map(|e| e.message.as_str())
                .collect();
            metadata["globalErrors"] = serde_json::json!({
                "count": report.errors.len(),
                "messages": error_messages,
            });
        }

        if !project_names.is_empty() {
            metadata["projects"] = serde_json::json!(project_names);
        }

        Ok(ParseResult::TestReport(ParsedReport {
            framework: "playwright".to_string(),
            summary,
            test_cases,
            metadata,
        }))
    }
}

fn collect_test_cases(
    suite: &PlaywrightSuite,
    ancestors: &[&str],
    multi_project: bool,
    test_cases: &mut Vec<TestCase>,
) {
    // Build path: skip empty titles (root file-level suites have the filename as title,
    // but specs already carry their own file path)
    let mut path: Vec<&str> = ancestors.to_vec();
    if !suite.title.is_empty() && !suite.title.contains('/') && !suite.title.contains('\\') {
        path.push(&suite.title);
    }

    for spec in &suite.specs {
        for test in &spec.tests {
            let id = if multi_project && !test.project_id.is_empty() {
                format!("{}-{}", spec.id, test.project_id)
            } else {
                spec.id.clone()
            };

            let mut full_name_parts: Vec<&str> = path.iter()
                .filter(|s| !s.is_empty())
                .copied()
                .collect();
            full_name_parts.push(&spec.title);

            let mut full_name = full_name_parts.join(" > ");
            if multi_project && !test.project_name.is_empty() {
                full_name = format!("{} [{}]", full_name, test.project_name);
            }

            let last_result = test.results.last();

            let status = map_status(&test.status, last_result);

            let duration_ms = last_result.map(|r| r.duration.max(0.0).round() as u64);

            let error_message = last_result.and_then(|r| extract_error(r));

            let retry_attempt = last_result.map(|r| r.retry);

            test_cases.push(TestCase {
                id,
                name: spec.title.clone(),
                full_name,
                status,
                duration_ms,
                error_message,
                file_path: Some(spec.file.clone()),
                line: Some(spec.line),
                retry_attempt,
            });
        }
    }

    if let Some(ref nested) = suite.suites {
        for child in nested {
            collect_test_cases(child, &path, multi_project, test_cases);
        }
    }
}

fn map_status(status: &str, last_result: Option<&PlaywrightTestResult>) -> TestStatus {
    match status {
        "expected" => TestStatus::Passed,
        "unexpected" => {
            if let Some(result) = last_result {
                if result.status.as_deref() == Some("timedOut") {
                    return TestStatus::TimedOut;
                }
            }
            TestStatus::Failed
        }
        "skipped" => TestStatus::Skipped,
        "flaky" => TestStatus::Flaky,
        // Unknown statuses default to Skipped for forward-compatibility.
        // Known Playwright statuses: expected, unexpected, skipped, flaky.
        other => {
            eprintln!("Warning: Unknown Playwright test status '{}', treating as skipped", other);
            TestStatus::Skipped
        }
    }
}

fn extract_error(result: &PlaywrightTestResult) -> Option<String> {
    // Prefer errors[] array (may have multiple)
    if !result.errors.is_empty() {
        let messages: Vec<&str> = result.errors.iter()
            .map(|e| e.message.as_str())
            .collect();
        let joined = messages.join("\n");
        if !joined.is_empty() {
            return Some(joined);
        }
    }

    // Fallback to single error field
    result.error.as_ref().map(|e| e.message.clone())
}

// ============================================================================
// Input deserialization structs
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightJsonReport {
    config: PlaywrightConfig,
    suites: Vec<PlaywrightSuite>,
    #[serde(default)]
    errors: Vec<PlaywrightErrorMessage>,
    stats: PlaywrightStats,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightConfig {
    projects: Vec<ProjectConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ProjectConfig {
    #[allow(dead_code)]
    id: String,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightSuite {
    title: String,
    #[allow(dead_code)]
    file: String,
    #[allow(dead_code)]
    line: u64,
    #[allow(dead_code)]
    column: u64,
    #[serde(default)]
    specs: Vec<PlaywrightSpec>,
    suites: Option<Vec<PlaywrightSuite>>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightSpec {
    title: String,
    #[allow(dead_code)]
    ok: bool,
    id: String,
    file: String,
    line: u64,
    #[allow(dead_code)]
    column: u64,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
    tests: Vec<PlaywrightTest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightTest {
    project_name: String,
    project_id: String,
    #[serde(default)]
    results: Vec<PlaywrightTestResult>,
    status: String,
    #[serde(default)]
    #[allow(dead_code)]
    annotations: Vec<PlaywrightAnnotation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightTestResult {
    status: Option<String>,
    duration: f64,
    error: Option<PlaywrightErrorMessage>,
    #[serde(default)]
    errors: Vec<PlaywrightErrorMessage>,
    retry: u32,
    #[allow(dead_code)]
    attachments: Vec<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightErrorMessage {
    message: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightAnnotation {
    #[serde(rename = "type")]
    #[allow(dead_code)]
    type_: String,
    #[allow(dead_code)]
    description: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PlaywrightStats {
    start_time: String,
    duration: f64,
    expected: usize,
    unexpected: usize,
    flaky: usize,
    skipped: usize,
}
