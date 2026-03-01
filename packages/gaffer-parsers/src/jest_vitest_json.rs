use serde::Deserialize;

use crate::registry::Parser;
use crate::types::{ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus};

pub struct JestVitestParser;

impl Parser for JestVitestParser {
    fn id(&self) -> &str {
        "jest-vitest-json"
    }

    fn name(&self) -> &str {
        "Jest/Vitest JSON Report"
    }

    fn priority(&self) -> u8 {
        85
    }

    fn result_type(&self) -> ResultType {
        ResultType::TestReport
    }

    fn detect(&self, content: &str, filename: &str) -> u8 {
        if !filename.ends_with(".json") {
            return 0;
        }

        let keys = crate::detect::extract_json_top_level_keys(content);
        let has_num_total = keys.iter().any(|k| k == "numTotalTests");
        let has_results = keys.iter().any(|k| k == "testResults");
        let has_success = keys.iter().any(|k| k == "success");

        if has_num_total && has_results && has_success { 90 } else { 0 }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let report: JestVitestReport = serde_json::from_str(content)
            .map_err(|e| ParseError::from(format!("Invalid Jest/Vitest JSON: {}", e)))?;

        let framework = detect_framework(&report, content);

        let mut test_cases: Vec<TestCase> = Vec::new();
        let mut id_counter: usize = 0;

        for test_result in &report.test_results {
            let file_path = normalize_file_path(&test_result.name);

            for assertion in &test_result.assertion_results {
                id_counter += 1;

                let status = match assertion.status.as_str() {
                    "passed" => TestStatus::Passed,
                    "failed" => TestStatus::Failed,
                    "pending" | "skipped" | "todo" | "disabled" => TestStatus::Skipped,
                    _ => TestStatus::Skipped,
                };

                let duration_ms = assertion.duration.and_then(|d| {
                    if d.is_finite() && d >= 0.0 { Some(d.round() as u64) } else { None }
                });

                let error_message = if assertion.failure_messages.is_empty() {
                    None
                } else {
                    Some(assertion.failure_messages.join("\n"))
                };

                test_cases.push(TestCase {
                    id: format!("tc-{}", id_counter),
                    name: assertion.title.clone(),
                    full_name: assertion.full_name.clone(),
                    status,
                    duration_ms,
                    error_message,
                    file_path: Some(file_path.clone()),
                    line: assertion.location.as_ref().map(|l| l.line),
                    retry_attempt: None,
                });
            }
        }

        // Duration: sum of (endTime - startTime) across testResults
        let suite_duration_ms: u64 = report.test_results.iter()
            .map(|r| r.end_time - r.start_time)
            .filter(|d| *d > 0.0 && d.is_finite())
            .map(|d| d.round() as u64)
            .sum();

        // Fallback to sum of test case durations
        let duration_ms = if suite_duration_ms > 0 {
            Some(suite_duration_ms)
        } else {
            let fallback: u64 = test_cases.iter().map(|tc| tc.duration_ms.unwrap_or(0)).sum();
            if fallback > 0 { Some(fallback) } else { None }
        };

        let summary = Summary {
            passed: report.num_passed_tests,
            failed: report.num_failed_tests,
            skipped: report.num_pending_tests + report.num_todo_tests,
            flaky: 0,
            total: report.num_total_tests,
            duration_ms,
        };

        let mut metadata = serde_json::json!({
            "suiteCount": report.num_total_test_suites,
            "passedSuites": report.num_passed_test_suites,
            "failedSuites": report.num_failed_test_suites,
            "pendingSuites": report.num_pending_test_suites,
            "success": report.success,
            "startTime": report.start_time,
        });

        if let Some(snapshot) = &report.snapshot {
            metadata["snapshots"] = serde_json::json!({
                "total": snapshot.total,
                "matched": snapshot.matched,
                "unmatched": snapshot.unmatched,
                "updated": snapshot.updated,
            });
        }

        if framework == "jest" {
            if let Some(n) = report.num_runtime_error_test_suites {
                metadata["runtimeErrorSuites"] = serde_json::json!(n);
            }
            if let Some(w) = report.was_interrupted {
                metadata["wasInterrupted"] = serde_json::json!(w);
            }
        }

        Ok(ParseResult::TestReport(ParsedReport {
            framework: framework.to_string(),
            summary,
            test_cases,
            metadata,
        }))
    }
}

fn detect_framework(report: &JestVitestReport, raw_text: &str) -> &'static str {
    // Check for Jest-only top-level fields
    if report.num_runtime_error_test_suites.is_some()
        || report.open_handles.is_some()
        || report.was_interrupted.is_some()
    {
        return "jest";
    }

    // Check assertion results for framework-specific fields
    for test_result in &report.test_results {
        for assertion in &test_result.assertion_results {
            if assertion.meta.is_some() {
                return "vitest";
            }
            if assertion.invocations.is_some() || assertion.num_passing_asserts.is_some() {
                return "jest";
            }
        }
    }

    // Fallback: check raw text
    if raw_text.contains("vitest") || raw_text.contains("Vitest") {
        return "vitest";
    }

    "jest"
}

fn normalize_file_path(full_path: &str) -> String {
    let normalized = full_path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    let marker_index = parts.iter().position(|&p| {
        p == "tests" || p == "test" || p == "__tests__" || p == "src"
    });
    match marker_index {
        Some(idx) => parts[idx..].join("/"),
        None => parts.last().unwrap_or(&full_path).to_string(),
    }
}

// ============================================================================
// Input deserialization structs
// ============================================================================

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct JestVitestReport {
    num_total_tests: usize,
    num_passed_tests: usize,
    num_failed_tests: usize,
    num_pending_tests: usize,
    #[serde(default)]
    num_todo_tests: usize,
    num_total_test_suites: usize,
    num_passed_test_suites: usize,
    num_failed_test_suites: usize,
    num_pending_test_suites: usize,
    start_time: f64,
    success: bool,
    test_results: Vec<TestResult>,
    // Optional Jest-only fields (for framework detection)
    num_runtime_error_test_suites: Option<usize>,
    open_handles: Option<serde_json::Value>,
    was_interrupted: Option<bool>,
    // Optional
    snapshot: Option<SnapshotSummary>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TestResult {
    assertion_results: Vec<AssertionResult>,
    end_time: f64,
    start_time: f64,
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssertionResult {
    full_name: String,
    status: String,
    title: String,
    duration: Option<f64>,
    #[serde(default)]
    failure_messages: Vec<String>,
    location: Option<Location>,
    // Vitest-only (for framework detection)
    meta: Option<serde_json::Value>,
    // Jest-only (for framework detection)
    invocations: Option<serde_json::Value>,
    num_passing_asserts: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Location {
    line: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SnapshotSummary {
    total: usize,
    matched: usize,
    unmatched: usize,
    updated: usize,
}
