use gaffer_parsers::registry::ParserRegistry;

fn load_fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).expect(&format!("Failed to read fixture: {}", path))
}

fn parse_via_registry(content: &str, filename: &str) -> serde_json::Value {
    let registry = ParserRegistry::with_defaults();
    let result = registry.parse(content, filename)
        .expect("Expected a parser to match")
        .expect("Expected parse to succeed");
    let json_str = serde_json::to_string(&result).expect("Failed to serialize");
    serde_json::from_str(&json_str).expect("Failed to parse JSON output")
}

// ============================================================================
// Multi-project report tests
// ============================================================================

#[test]
fn test_playwright_framework_detection() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    assert_eq!(report["data"]["framework"], "playwright");
}

#[test]
fn test_playwright_summary() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let summary = &report["data"]["summary"];
    assert_eq!(summary["passed"], 5);
    assert_eq!(summary["failed"], 4);
    assert_eq!(summary["skipped"], 2);
    assert_eq!(summary["flaky"], 1);
    assert_eq!(summary["total"], 12);
    assert_eq!(summary["durationMs"], 42000);
}

#[test]
fn test_playwright_multi_project_test_cases() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // 6 specs × 2 projects = 12 test cases
    assert_eq!(test_cases.len(), 12);
}

#[test]
fn test_playwright_multi_project_ids() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // First spec runs on chromium and firefox
    assert_eq!(test_cases[0]["id"], "spec-1-chromium");
    assert_eq!(test_cases[1]["id"], "spec-1-firefox");
}

#[test]
fn test_playwright_multi_project_full_name() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // Top-level spec (no describe block) — file suite title is a path, gets skipped
    assert_eq!(test_cases[0]["fullName"], "should display login form [chromium]");
    assert_eq!(test_cases[1]["fullName"], "should display login form [firefox]");

    // Nested in "Login Suite" describe block
    assert_eq!(test_cases[2]["fullName"], "Login Suite > should login successfully [chromium]");
    assert_eq!(test_cases[3]["fullName"], "Login Suite > should login successfully [firefox]");
}

#[test]
fn test_playwright_status_mapping_passed() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // spec-1 chromium: status "expected" → passed
    assert_eq!(test_cases[0]["status"], "passed");
}

#[test]
fn test_playwright_status_mapping_failed() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // spec-3 chromium: status "unexpected" → failed
    let failed = test_cases.iter().find(|tc| tc["id"] == "spec-3-chromium").unwrap();
    assert_eq!(failed["status"], "failed");
}

#[test]
fn test_playwright_status_mapping_skipped() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let skipped = test_cases.iter().find(|tc| tc["id"] == "spec-4-chromium").unwrap();
    assert_eq!(skipped["status"], "skipped");
}

#[test]
fn test_playwright_status_mapping_flaky() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let flaky = test_cases.iter().find(|tc| tc["id"] == "spec-5-chromium").unwrap();
    assert_eq!(flaky["status"], "flaky");
}

#[test]
fn test_playwright_status_mapping_timed_out() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let timed_out = test_cases.iter().find(|tc| tc["id"] == "spec-6-chromium").unwrap();
    assert_eq!(timed_out["status"], "timedOut");
}

#[test]
fn test_playwright_error_extraction() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let failed = test_cases.iter().find(|tc| tc["id"] == "spec-3-chromium").unwrap();
    let error = failed["errorMessage"].as_str().unwrap();
    assert!(error.contains("expect(received).toBeVisible()"));
}

#[test]
fn test_playwright_timed_out_error_message() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let timed_out = test_cases.iter().find(|tc| tc["id"] == "spec-6-chromium").unwrap();
    let error = timed_out["errorMessage"].as_str().unwrap();
    assert!(error.contains("Test timeout of 5000ms exceeded"));
}

#[test]
fn test_playwright_retry_attempt() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // Passed test: retry 0
    assert_eq!(test_cases[0]["retryAttempt"], 0);
    // Failed test with retries: last result retry = 2
    let failed = test_cases.iter().find(|tc| tc["id"] == "spec-3-chromium").unwrap();
    assert_eq!(failed["retryAttempt"], 2);
    // Flaky test: passed on retry 1
    let flaky = test_cases.iter().find(|tc| tc["id"] == "spec-5-chromium").unwrap();
    assert_eq!(flaky["retryAttempt"], 1);
}

#[test]
fn test_playwright_file_path_and_line() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases[0]["filePath"], "tests/login.spec.ts");
    assert_eq!(test_cases[0]["line"], 3);
}

#[test]
fn test_playwright_duration_from_last_result() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // First test: single result with duration 1250
    assert_eq!(test_cases[0]["durationMs"], 1250);
}

#[test]
fn test_playwright_skipped_no_results_duration_null() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let skipped = test_cases.iter().find(|tc| tc["id"] == "spec-4-chromium").unwrap();
    assert!(skipped["durationMs"].is_null());
    assert!(skipped["retryAttempt"].is_null());
}

#[test]
fn test_playwright_metadata() {
    let json = load_fixture("playwright-json-report.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let metadata = &report["data"]["metadata"];
    assert_eq!(metadata["ok"], false);
    assert_eq!(metadata["startTime"], "2026-02-25T10:00:00.000Z");
    assert!(metadata.get("globalErrors").is_none() || metadata["globalErrors"].is_null(),
        "No globalErrors when errors array is empty");
    let projects = metadata["projects"].as_array().unwrap();
    assert_eq!(projects.len(), 2);
    assert_eq!(projects[0], "chromium");
    assert_eq!(projects[1], "firefox");
}

// ============================================================================
// Single-project report tests
// ============================================================================

#[test]
fn test_single_project_framework() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    assert_eq!(report["data"]["framework"], "playwright");
}

#[test]
fn test_single_project_summary() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let summary = &report["data"]["summary"];
    assert_eq!(summary["passed"], 2);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["skipped"], 0);
    assert_eq!(summary["flaky"], 0);
    assert_eq!(summary["total"], 3);
    assert_eq!(summary["durationMs"], 5000);
}

#[test]
fn test_single_project_no_project_suffix() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases.len(), 3);
    // No [projectName] suffix for single project
    assert_eq!(test_cases[0]["fullName"], "should render homepage");
    assert_eq!(test_cases[1]["fullName"], "should navigate to about");
}

#[test]
fn test_single_project_ids_no_project_suffix() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // Single project with empty ID → just spec id
    assert_eq!(test_cases[0]["id"], "spec-1");
    assert_eq!(test_cases[1]["id"], "spec-2");
}

#[test]
fn test_single_project_nested_suite_full_name() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // Nested in "Footer" describe block
    assert_eq!(test_cases[2]["fullName"], "Footer > should show copyright");
}

#[test]
fn test_single_project_failed_with_error() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let failed = test_cases.iter().find(|tc| tc["status"] == "failed").unwrap();
    assert_eq!(failed["name"], "should show copyright");
    let error = failed["errorMessage"].as_str().unwrap();
    assert!(error.contains("Expected: \"2026\""));
    assert!(error.contains("Received: \"2025\""));
}

#[test]
fn test_single_project_retry_on_failure() {
    let json = load_fixture("playwright-json-single-project.json");
    let report = parse_via_registry(&json, "playwright-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let failed = test_cases.iter().find(|tc| tc["status"] == "failed").unwrap();
    assert_eq!(failed["retryAttempt"], 1);
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn test_detection_non_json_returns_zero() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    assert_eq!(parser.detect("{}", "report.xml"), 0);
    assert_eq!(parser.detect("{}", "report.html"), 0);
}

#[test]
fn test_detection_jest_json_returns_zero() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    let jest_sample = r#"{"numTotalTests": 5, "numPassedTests": 5, "testResults": [], "success": true}"#;
    assert_eq!(parser.detect(jest_sample, "results.json"), 0);
}

#[test]
fn test_detection_ctrf_json_returns_zero() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    let ctrf_sample = r#"{"results": {"tool": {"name": "ctrf"}}, "tests": []}"#;
    assert_eq!(parser.detect(ctrf_sample, "ctrf-report.json"), 0);
}

#[test]
fn test_detection_valid_playwright_json() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    let json = load_fixture("playwright-json-report.json");
    assert_eq!(parser.detect(&json, "results.json"), 90);
}

// ============================================================================
// Empty suites
// ============================================================================

#[test]
fn test_empty_suites() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let json = r#"{
        "config": { "projects": [{ "id": "", "name": "" }] },
        "suites": [],
        "errors": [],
        "stats": { "startTime": "2026-01-01T00:00:00Z", "duration": 0, "expected": 0, "unexpected": 0, "flaky": 0, "skipped": 0 }
    }"#;

    let parser = PlaywrightJsonParser;
    let result = parser.parse(json, "results.json").unwrap();
    let json_str = serde_json::to_string(&result).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases.len(), 0);
    assert_eq!(report["data"]["summary"]["total"], 0);
}

// ============================================================================
// parse_report WASM export
// ============================================================================

#[test]
fn test_parse_report_wasm_export_playwright() {
    let json = load_fixture("playwright-json-report.json");
    let result = gaffer_parsers::parse_report(&json, "playwright-results.json")
        .expect("parse_report should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["data"]["framework"], "playwright");
}

// ============================================================================
// Invalid input
// ============================================================================

#[test]
fn test_parse_invalid_json() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    let result = parser.parse("{ not valid json }", "results.json");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.message.contains("Invalid Playwright JSON"));
}

#[test]
fn test_parse_valid_json_wrong_shape() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let parser = PlaywrightJsonParser;
    let result = parser.parse(r#"{"results": [], "tool": "ctrf"}"#, "results.json");
    assert!(result.is_err());
}

// ============================================================================
// Error extraction edge cases
// ============================================================================

#[test]
fn test_extract_error_fallback_to_singular_error_field() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let json = r#"{
        "config": { "projects": [{ "id": "default", "name": "default" }] },
        "suites": [{
            "title": "tests/example.spec.ts",
            "file": "tests/example.spec.ts",
            "line": 0,
            "column": 0,
            "specs": [{
                "title": "should work",
                "ok": false,
                "id": "spec-fallback",
                "file": "tests/example.spec.ts",
                "line": 5,
                "column": 1,
                "tags": [],
                "tests": [{
                    "projectName": "",
                    "projectId": "",
                    "status": "unexpected",
                    "results": [{
                        "status": "failed",
                        "duration": 100,
                        "error": { "message": "Fallback error message" },
                        "errors": [],
                        "retry": 0,
                        "attachments": []
                    }],
                    "annotations": []
                }]
            }],
            "suites": []
        }],
        "errors": [],
        "stats": { "startTime": "2026-01-01T00:00:00Z", "duration": 100, "expected": 0, "unexpected": 1, "flaky": 0, "skipped": 0 }
    }"#;

    let parser = PlaywrightJsonParser;
    let result = parser.parse(json, "results.json").unwrap();
    let json_str = serde_json::to_string(&result).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let tc = &test_cases[0];
    assert_eq!(tc["errorMessage"], "Fallback error message");
}

#[test]
fn test_extract_error_multiple_errors_joined() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let json = r#"{
        "config": { "projects": [{ "id": "default", "name": "default" }] },
        "suites": [{
            "title": "tests/example.spec.ts",
            "file": "tests/example.spec.ts",
            "line": 0,
            "column": 0,
            "specs": [{
                "title": "has multiple failures",
                "ok": false,
                "id": "spec-multi-err",
                "file": "tests/example.spec.ts",
                "line": 10,
                "column": 1,
                "tags": [],
                "tests": [{
                    "projectName": "",
                    "projectId": "",
                    "status": "unexpected",
                    "results": [{
                        "status": "failed",
                        "duration": 200,
                        "errors": [
                            { "message": "First assertion failed" },
                            { "message": "Second assertion failed" }
                        ],
                        "retry": 0,
                        "attachments": []
                    }],
                    "annotations": []
                }]
            }],
            "suites": []
        }],
        "errors": [],
        "stats": { "startTime": "2026-01-01T00:00:00Z", "duration": 200, "expected": 0, "unexpected": 1, "flaky": 0, "skipped": 0 }
    }"#;

    let parser = PlaywrightJsonParser;
    let result = parser.parse(json, "results.json").unwrap();
    let json_str = serde_json::to_string(&result).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let error_msg = test_cases[0]["errorMessage"].as_str().unwrap();
    assert!(error_msg.contains("First assertion failed"));
    assert!(error_msg.contains("Second assertion failed"));
    assert!(error_msg.contains('\n'), "Multiple errors should be joined with newline");
}

#[test]
fn test_global_errors_count() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::PlaywrightJsonParser;

    let json = r#"{
        "config": { "projects": [{ "id": "default", "name": "default" }] },
        "suites": [],
        "errors": [
            { "message": "Global setup failed" },
            { "message": "Config validation error" }
        ],
        "stats": { "startTime": "2026-01-01T00:00:00Z", "duration": 0, "expected": 0, "unexpected": 0, "flaky": 0, "skipped": 0 }
    }"#;

    let parser = PlaywrightJsonParser;
    let result = parser.parse(json, "results.json").unwrap();
    let json_str = serde_json::to_string(&result).unwrap();
    let report: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    assert_eq!(report["data"]["metadata"]["globalErrors"]["count"], 2);
    let messages = report["data"]["metadata"]["globalErrors"]["messages"].as_array().unwrap();
    assert_eq!(messages[0], "Global setup failed");
    assert_eq!(messages[1], "Config validation error");
}
