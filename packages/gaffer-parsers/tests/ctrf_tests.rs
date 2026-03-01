use gaffer_parsers::{CtrfParser, Parser, ParserRegistry, ParseResult};

fn load_fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/ctrf/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e))
}

fn parse_basic() -> gaffer_parsers::types::ParsedReport {
    let content = load_fixture("ctrf-basic.json");
    let parser = CtrfParser;
    match parser.parse(&content, "ctrf-basic.json").unwrap() {
        ParseResult::TestReport(report) => report,
        _ => panic!("Expected TestReport"),
    }
}

fn parse_minimal() -> gaffer_parsers::types::ParsedReport {
    let content = load_fixture("ctrf-minimal.json");
    let parser = CtrfParser;
    match parser.parse(&content, "ctrf-minimal.json").unwrap() {
        ParseResult::TestReport(report) => report,
        _ => panic!("Expected TestReport"),
    }
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_report_format_ctrf_marker_score_95() {
    let content = load_fixture("ctrf-basic.json");
    let parser = CtrfParser;
    assert_eq!(parser.detect(&content, "report.json"), 95);
}

#[test]
fn detect_structural_match_score_70() {
    let content = load_fixture("ctrf-minimal.json");
    let parser = CtrfParser;
    assert_eq!(parser.detect(&content, "report.json"), 70);
}

#[test]
fn detect_non_json_returns_0() {
    let parser = CtrfParser;
    assert_eq!(parser.detect("{}", "report.xml"), 0);
    assert_eq!(parser.detect("{}", "report.html"), 0);
}

#[test]
fn detect_unrelated_json_returns_0() {
    let parser = CtrfParser;
    let json = r#"{"name": "package", "version": "1.0.0"}"#;
    assert_eq!(parser.detect(json, "package.json"), 0);
}

#[test]
fn detect_jest_json_not_detected_as_ctrf() {
    let parser = CtrfParser;
    let json = r#"{"numTotalTests": 5, "numPassedTests": 4, "numFailedTests": 1, "numPendingTests": 0, "testResults": [], "success": false, "startTime": 1700000000000}"#;
    assert_eq!(parser.detect(json, "jest-results.json"), 0);
}

// ============================================================================
// Basic parsing tests
// ============================================================================

#[test]
fn basic_framework_name() {
    let report = parse_basic();
    assert_eq!(report.framework, "vitest");
}

#[test]
fn basic_summary_counts() {
    let report = parse_basic();
    assert_eq!(report.summary.total, 6);
    assert_eq!(report.summary.passed, 3);
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.summary.skipped, 2); // skipped(1) + pending(0) + other(1)
    assert_eq!(report.summary.flaky, 1);
}

#[test]
fn basic_summary_duration() {
    let report = parse_basic();
    assert_eq!(report.summary.duration_ms, Some(3500));
}

#[test]
fn basic_test_case_count() {
    let report = parse_basic();
    assert_eq!(report.test_cases.len(), 6);
}

// ============================================================================
// Suite handling
// ============================================================================

#[test]
fn suite_as_array_builds_full_name() {
    let report = parse_basic();
    let tc = &report.test_cases[0]; // suite: ["Math", "Addition"]
    assert_eq!(tc.full_name, "Math > Addition > adds numbers");
}

#[test]
fn suite_as_single_string_builds_full_name() {
    let report = parse_minimal();
    let tc = &report.test_cases[0]; // suite: "auth"
    assert_eq!(tc.full_name, "auth > test_login");
}

#[test]
fn no_suite_full_name_equals_name() {
    let report = parse_minimal();
    let tc = &report.test_cases[1]; // no suite
    assert_eq!(tc.full_name, "test_logout");
}

// ============================================================================
// Flaky flag overrides status
// ============================================================================

#[test]
fn flaky_flag_overrides_to_flaky_status() {
    let report = parse_basic();
    let tc = &report.test_cases[5]; // flaky: true, status: "passed"
    assert_eq!(tc.status, gaffer_parsers::TestStatus::Flaky);
}

// ============================================================================
// Error extraction
// ============================================================================

#[test]
fn error_message_from_message_field() {
    let report = parse_basic();
    let tc = &report.test_cases[2]; // has both message and trace
    assert_eq!(tc.error_message.as_deref(), Some("Expected no error but got DivisionByZero"));
}

#[test]
fn error_message_from_trace_first_line() {
    let report = parse_minimal();
    let tc = &report.test_cases[1]; // trace only, no message
    assert_eq!(tc.error_message.as_deref(), Some("AssertionError: expected 200"));
}

#[test]
fn no_error_when_neither_message_nor_trace() {
    let report = parse_basic();
    let tc = &report.test_cases[0]; // passed, no error
    assert!(tc.error_message.is_none());
}

// ============================================================================
// Duration
// ============================================================================

#[test]
fn test_duration_rounded() {
    let report = parse_basic();
    assert_eq!(report.test_cases[0].duration_ms, Some(12)); // 12.3 rounds to 12
    assert_eq!(report.test_cases[1].duration_ms, Some(9));   // 8.7 rounds to 9
}

#[test]
fn summary_duration_from_explicit_field() {
    // ctrf-basic has explicit duration: 3500
    let report = parse_basic();
    assert_eq!(report.summary.duration_ms, Some(3500));
}

#[test]
fn summary_duration_from_stop_minus_start() {
    // ctrf-minimal has no duration, but has start=1000, stop=2000
    let report = parse_minimal();
    assert_eq!(report.summary.duration_ms, Some(1000));
}

#[test]
fn summary_duration_fallback_to_test_sum() {
    let json = r#"{
        "results": {
            "tool": { "name": "test" },
            "summary": { "tests": 2, "passed": 2, "failed": 0, "skipped": 0, "pending": 0, "other": 0, "start": 0, "stop": 0 },
            "tests": [
                { "name": "a", "status": "passed", "duration": 100 },
                { "name": "b", "status": "passed", "duration": 200 }
            ]
        }
    }"#;
    let parser = CtrfParser;
    match parser.parse(json, "report.json").unwrap() {
        ParseResult::TestReport(report) => {
            assert_eq!(report.summary.duration_ms, Some(300));
        }
        _ => panic!("Expected TestReport"),
    }
}

// ============================================================================
// Test IDs
// ============================================================================

#[test]
fn test_id_from_field() {
    let report = parse_basic();
    assert_eq!(report.test_cases[0].id, "test-001");
}

#[test]
fn test_id_auto_generated() {
    let report = parse_minimal();
    assert_eq!(report.test_cases[0].id, "tc-1");
    assert_eq!(report.test_cases[1].id, "tc-2");
}

// ============================================================================
// Metadata extraction
// ============================================================================

#[test]
fn metadata_basic_fields() {
    let report = parse_basic();
    let m = &report.metadata;
    assert_eq!(m["specVersion"], "0.0.1");
    assert_eq!(m["toolName"], "vitest");
    assert_eq!(m["toolVersion"], "2.1.0");
    assert_eq!(m["reportId"], "rpt-abc123");
    assert_eq!(m["timestamp"], "2026-02-25T10:00:00.000Z");
    assert_eq!(m["generatedBy"], "ctrf-vitest-reporter");
    assert_eq!(m["suiteCount"], 2);
}

#[test]
fn metadata_tool_extra() {
    let report = parse_basic();
    assert_eq!(report.metadata["toolExtra"]["configFile"], "vitest.config.ts");
}

#[test]
fn metadata_summary_extra() {
    let report = parse_basic();
    assert_eq!(report.metadata["summaryExtra"]["seed"], 12345);
}

#[test]
fn metadata_environment_fields() {
    let report = parse_basic();
    let m = &report.metadata;
    assert_eq!(m["appName"], "my-app");
    assert_eq!(m["appVersion"], "1.2.3");
    assert_eq!(m["buildName"], "CI Build");
    assert_eq!(m["buildNumber"], "456");
    assert_eq!(m["buildUrl"], "https://ci.example.com/builds/456");
    assert_eq!(m["repositoryName"], "my-org/my-app");
    assert_eq!(m["repositoryUrl"], "https://github.com/my-org/my-app");
    assert_eq!(m["commit"], "abc123def456");
    assert_eq!(m["branchName"], "main");
    assert_eq!(m["osPlatform"], "linux");
    assert_eq!(m["osRelease"], "22.04");
    assert_eq!(m["osVersion"], "5.15.0");
    assert_eq!(m["testEnvironment"], "ci");
    assert_eq!(m["environmentExtra"]["runner"], "github-actions");
}

#[test]
fn metadata_minimal_has_no_environment() {
    let report = parse_minimal();
    let m = &report.metadata;
    assert!(m.get("appName").is_none());
    assert!(m.get("buildName").is_none());
}

// ============================================================================
// File path and line
// ============================================================================

#[test]
fn file_path_and_line_extracted() {
    let report = parse_basic();
    assert_eq!(report.test_cases[0].file_path.as_deref(), Some("tests/math.test.ts"));
    assert_eq!(report.test_cases[0].line, Some(5));
}

#[test]
fn retry_attempt_extracted() {
    let report = parse_basic();
    assert_eq!(report.test_cases[5].retry_attempt, Some(2));
}

// ============================================================================
// Empty tests array
// ============================================================================

#[test]
fn empty_tests_array_parses_ok() {
    let json = r#"{
        "reportFormat": "CTRF",
        "results": {
            "tool": { "name": "test" },
            "summary": { "tests": 0, "passed": 0, "failed": 0, "skipped": 0, "pending": 0, "other": 0, "start": 0, "stop": 0 },
            "tests": []
        }
    }"#;
    let parser = CtrfParser;
    let result = parser.parse(json, "report.json");
    assert!(result.is_ok());
    match result.unwrap() {
        ParseResult::TestReport(report) => {
            assert_eq!(report.test_cases.len(), 0);
            assert_eq!(report.summary.total, 0);
        }
        _ => panic!("Expected TestReport"),
    }
}

// ============================================================================
// Registry auto-detection
// ============================================================================

#[test]
fn registry_detects_ctrf_basic() {
    let content = load_fixture("ctrf-basic.json");
    let registry = ParserRegistry::with_defaults();
    let result = registry.parse(&content, "report.json");
    assert!(result.is_some(), "Registry should detect CTRF report");
    match result.unwrap().unwrap() {
        ParseResult::TestReport(report) => {
            assert_eq!(report.framework, "vitest");
            assert_eq!(report.summary.total, 6);
        }
        _ => panic!("Expected TestReport"),
    }
}

#[test]
fn registry_detects_ctrf_minimal() {
    let content = load_fixture("ctrf-minimal.json");
    let registry = ParserRegistry::with_defaults();
    let result = registry.parse(&content, "report.json");
    assert!(result.is_some(), "Registry should detect minimal CTRF report");
    match result.unwrap().unwrap() {
        ParseResult::TestReport(report) => {
            assert_eq!(report.framework, "pytest");
        }
        _ => panic!("Expected TestReport"),
    }
}

#[test]
fn registry_includes_ctrf_parser() {
    let registry = ParserRegistry::with_defaults();
    let ids = registry.parser_ids();
    assert!(ids.contains(&"ctrf"), "Registry should include CTRF parser");
}

// ============================================================================
// Status mapping
// ============================================================================

#[test]
fn status_mapping_all_variants() {
    let json = r#"{
        "results": {
            "tool": { "name": "test" },
            "summary": { "tests": 5, "passed": 1, "failed": 1, "skipped": 1, "pending": 1, "other": 1, "start": 0, "stop": 0 },
            "tests": [
                { "name": "pass", "status": "passed", "duration": 1 },
                { "name": "fail", "status": "failed", "duration": 1 },
                { "name": "skip", "status": "skipped", "duration": 0 },
                { "name": "pend", "status": "pending", "duration": 0 },
                { "name": "other", "status": "other", "duration": 0 }
            ]
        }
    }"#;
    let parser = CtrfParser;
    match parser.parse(json, "report.json").unwrap() {
        ParseResult::TestReport(report) => {
            use gaffer_parsers::TestStatus;
            assert_eq!(report.test_cases[0].status, TestStatus::Passed);
            assert_eq!(report.test_cases[1].status, TestStatus::Failed);
            assert_eq!(report.test_cases[2].status, TestStatus::Skipped);
            assert_eq!(report.test_cases[3].status, TestStatus::Skipped);
            assert_eq!(report.test_cases[4].status, TestStatus::Skipped);
        }
        _ => panic!("Expected TestReport"),
    }
}
