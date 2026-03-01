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
// Vitest report tests
// ============================================================================

#[test]
fn test_vitest_framework_detection() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    assert_eq!(report["data"]["framework"], "vitest");
}

#[test]
fn test_vitest_summary() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    let summary = &report["data"]["summary"];
    assert_eq!(summary["total"], 5);
    assert_eq!(summary["passed"], 4);
    assert_eq!(summary["failed"], 0);
    assert_eq!(summary["skipped"], 1);
    assert_eq!(summary["flaky"], 0);
}

#[test]
fn test_vitest_test_cases() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases.len(), 5);

    // First test case
    assert_eq!(test_cases[0]["id"], "tc-1");
    assert_eq!(test_cases[0]["name"], "should add two positive numbers");
    assert_eq!(test_cases[0]["fullName"], "Calculator add should add two positive numbers");
    assert_eq!(test_cases[0]["status"], "passed");
    // 1.2699... rounds to 1
    assert_eq!(test_cases[0]["durationMs"], 1);

    // Pending test
    assert_eq!(test_cases[2]["status"], "skipped");
    assert_eq!(test_cases[2]["name"], "pending feature");
}

#[test]
fn test_vitest_file_path_normalization() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    // /home/runner/work/project/tests/calculator.test.ts -> tests/calculator.test.ts
    assert_eq!(test_cases[0]["filePath"], "tests/calculator.test.ts");
    // /home/runner/work/project/src/utils/string.test.ts -> src/utils/string.test.ts
    assert_eq!(test_cases[3]["filePath"], "src/utils/string.test.ts");
}

#[test]
fn test_vitest_duration_from_test_results() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    // Suite 1: 140630 - 140126 = 504ms, Suite 2: 141200 - 140700 = 500ms → 1004ms
    assert_eq!(report["data"]["summary"]["durationMs"], 1004);
}

#[test]
fn test_vitest_metadata() {
    let json = load_fixture("vitest-report.json");
    let report = parse_via_registry(&json, "vitest-results.json");

    let metadata = &report["data"]["metadata"];
    assert_eq!(metadata["suiteCount"], 2);
    assert_eq!(metadata["passedSuites"], 2);
    assert_eq!(metadata["failedSuites"], 0);
    assert_eq!(metadata["pendingSuites"], 0);
    assert_eq!(metadata["success"], true);
    assert_eq!(metadata["startTime"], 1764894139853.0);
}

// ============================================================================
// Jest report tests
// ============================================================================

#[test]
fn test_jest_framework_detection() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    assert_eq!(report["data"]["framework"], "jest");
}

#[test]
fn test_jest_summary() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let summary = &report["data"]["summary"];
    assert_eq!(summary["total"], 5);
    assert_eq!(summary["passed"], 3);
    assert_eq!(summary["failed"], 1);
    assert_eq!(summary["skipped"], 1); // 1 todo
    assert_eq!(summary["flaky"], 0);
}

#[test]
fn test_jest_line_extraction() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases[0]["line"], 5);
    assert_eq!(test_cases[1]["line"], 10);
    // todo test has location: null
    assert!(test_cases[2]["line"].is_null());
}

#[test]
fn test_jest_error_messages() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    let failed = test_cases.iter().find(|tc| tc["status"] == "failed").unwrap();
    assert_eq!(failed["name"], "should authenticate user");

    let error = failed["errorMessage"].as_str().unwrap();
    assert!(error.contains("Expected 200 but got 401"));
    assert!(error.contains("at Object.<anonymous>"));
}

#[test]
fn test_jest_file_path_normalization() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases[0]["filePath"], "tests/calculator.test.ts");
    assert_eq!(test_cases[3]["filePath"], "__tests__/auth.test.ts");
}

#[test]
fn test_jest_metadata() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let metadata = &report["data"]["metadata"];
    assert_eq!(metadata["suiteCount"], 2);
    assert_eq!(metadata["passedSuites"], 1);
    assert_eq!(metadata["failedSuites"], 1);
    assert_eq!(metadata["success"], false);
    assert_eq!(metadata["runtimeErrorSuites"], 0);
    assert_eq!(metadata["wasInterrupted"], false);

    // Snapshot metadata
    let snapshots = &metadata["snapshots"];
    assert_eq!(snapshots["total"], 3);
    assert_eq!(snapshots["matched"], 3);
    assert_eq!(snapshots["unmatched"], 0);
    assert_eq!(snapshots["updated"], 0);
}

#[test]
fn test_jest_duration_across_suites() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    // Suite 1: 145233 - 143429 = 1804ms, Suite 2: 145500 - 145000 = 500ms → 2304ms
    assert_eq!(report["data"]["summary"]["durationMs"], 2304);
}

// ============================================================================
// Status mapping tests
// ============================================================================

#[test]
fn test_all_status_mappings() {
    let json = r#"{
        "numTotalTests": 6,
        "numPassedTests": 1,
        "numFailedTests": 1,
        "numPendingTests": 1,
        "numTodoTests": 1,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 0,
        "numFailedTestSuites": 1,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": false,
        "testResults": [{
            "assertionResults": [
                { "fullName": "a", "status": "passed", "title": "passed-test", "duration": 1, "failureMessages": [] },
                { "fullName": "b", "status": "failed", "title": "failed-test", "duration": 1, "failureMessages": ["err"] },
                { "fullName": "c", "status": "pending", "title": "pending-test", "duration": 0, "failureMessages": [] },
                { "fullName": "d", "status": "skipped", "title": "skipped-test", "duration": 0, "failureMessages": [] },
                { "fullName": "e", "status": "todo", "title": "todo-test", "duration": 0, "failureMessages": [] },
                { "fullName": "f", "status": "disabled", "title": "disabled-test", "duration": 0, "failureMessages": [] }
            ],
            "startTime": 1000,
            "endTime": 2000,
            "name": "/path/to/test.ts",
            "message": "",
            "status": "failed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    let test_cases = report["data"]["testCases"].as_array().unwrap();

    assert_eq!(test_cases[0]["status"], "passed");
    assert_eq!(test_cases[1]["status"], "failed");
    assert_eq!(test_cases[2]["status"], "skipped");
    assert_eq!(test_cases[3]["status"], "skipped");
    assert_eq!(test_cases[4]["status"], "skipped");
    assert_eq!(test_cases[5]["status"], "skipped");
}

// ============================================================================
// Empty test results
// ============================================================================

#[test]
fn test_empty_test_results() {
    let json = r#"{
        "numTotalTests": 0,
        "numPassedTests": 0,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 0,
        "numPassedTestSuites": 0,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": []
    }"#;

    let report = parse_via_registry(json, "results.json");
    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases.len(), 0);
    assert_eq!(report["data"]["summary"]["total"], 0);
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn test_detection_non_json_filename() {
    let registry = ParserRegistry::with_defaults();
    let json = load_fixture("vitest-report.json");

    // Non-.json filename should not match
    let result = registry.parse(&json, "report.xml");
    // Should match JUnit or nothing, but NOT jest-vitest
    if let Some(Ok(result)) = result {
        let json_str = serde_json::to_string(&result).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();
        assert_ne!(parsed["data"]["framework"], "vitest");
    }
}

#[test]
fn test_detection_non_jest_json() {
    let registry = ParserRegistry::with_defaults();
    let json = r#"{"results": {"tool": {"name": "ctrf"}}, "tests": []}"#;

    let result = registry.parse(json, "ctrf-report.json");
    assert!(result.is_none(), "CTRF JSON should not match jest-vitest parser");
}

#[test]
fn test_detection_valid_jest_json() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::JestVitestParser;

    let parser = JestVitestParser;
    let json = load_fixture("jest-report.json");
    let sample = &json[..json.len().min(2048)];

    let score = parser.detect(sample, "results.json");
    assert_eq!(score, 90);
}

#[test]
fn test_detection_returns_zero_for_non_json_extension() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::JestVitestParser;

    let parser = JestVitestParser;
    let score = parser.detect("{}", "report.xml");
    assert_eq!(score, 0);
}

// ============================================================================
// Error message joining
// ============================================================================

#[test]
fn test_failure_messages_joined_with_newline() {
    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 0,
        "numFailedTests": 1,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 0,
        "numFailedTestSuites": 1,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": false,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "failed",
                "title": "test",
                "duration": 1,
                "failureMessages": ["Error: first failure", "Error: second failure"]
            }],
            "startTime": 1000,
            "endTime": 2000,
            "name": "/path/to/test.ts",
            "message": "",
            "status": "failed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    let error = report["data"]["testCases"][0]["errorMessage"].as_str().unwrap();
    assert_eq!(error, "Error: first failure\nError: second failure");
}

// ============================================================================
// Multiple test files
// ============================================================================

#[test]
fn test_multiple_test_files() {
    let json = load_fixture("jest-report.json");
    let report = parse_via_registry(&json, "jest-results.json");

    let test_cases = report["data"]["testCases"].as_array().unwrap();
    assert_eq!(test_cases.len(), 5);

    // IDs should be sequential across files
    assert_eq!(test_cases[0]["id"], "tc-1");
    assert_eq!(test_cases[3]["id"], "tc-4");
    assert_eq!(test_cases[4]["id"], "tc-5");
}

// ============================================================================
// parse_report WASM export
// ============================================================================

#[test]
fn test_parse_report_wasm_export_jest() {
    let json = load_fixture("jest-report.json");
    let result = gaffer_parsers::parse_report(&json, "jest-results.json")
        .expect("parse_report should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["data"]["framework"], "jest");
}

#[test]
fn test_parse_report_wasm_export_vitest() {
    let json = load_fixture("vitest-report.json");
    let result = gaffer_parsers::parse_report(&json, "vitest-results.json")
        .expect("parse_report should succeed");
    let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
    assert_eq!(parsed["data"]["framework"], "vitest");
}

#[test]
fn test_parse_report_no_match() {
    let result = gaffer_parsers::parse_report("not a report", "readme.txt");
    assert!(result.is_err());
}

// ============================================================================
// Invalid input tests
// ============================================================================

#[test]
fn test_parse_invalid_json() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::JestVitestParser;

    let parser = JestVitestParser;
    let result = parser.parse("{ not valid json }", "results.json");
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.message.contains("Invalid Jest/Vitest JSON"));
}

#[test]
fn test_parse_valid_json_wrong_shape() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::JestVitestParser;

    let parser = JestVitestParser;
    let result = parser.parse(r#"{"results": [], "tool": "ctrf"}"#, "results.json");
    assert!(result.is_err());
}

// ============================================================================
// Duration fallback tests
// ============================================================================

#[test]
fn test_duration_fallback_when_suite_times_zero() {
    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 1,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 1,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "passed",
                "title": "test",
                "duration": 42,
                "failureMessages": []
            }],
            "startTime": 1000,
            "endTime": 1000,
            "name": "/path/to/tests/test.ts",
            "message": "",
            "status": "passed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    // Suite endTime == startTime → 0ms, so falls back to sum of test case durations
    assert_eq!(report["data"]["summary"]["durationMs"], 42);
}

#[test]
fn test_duration_none_when_all_zero() {
    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 1,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 1,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "passed",
                "title": "test",
                "duration": 0,
                "failureMessages": []
            }],
            "startTime": 1000,
            "endTime": 1000,
            "name": "/path/to/tests/test.ts",
            "message": "",
            "status": "passed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    // Both suite and fallback are 0 → durationMs should be null
    assert!(report["data"]["summary"]["durationMs"].is_null());
}

#[test]
fn test_negative_duration_ignored() {
    use gaffer_parsers::registry::Parser;
    use gaffer_parsers::JestVitestParser;

    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 1,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 1,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "passed",
                "title": "test",
                "duration": -1.0,
                "failureMessages": []
            }],
            "startTime": 2000,
            "endTime": 1000,
            "name": "/path/to/tests/test.ts",
            "message": "",
            "status": "passed"
        }]
    }"#;

    let parser = JestVitestParser;
    let result = parser.parse(json, "results.json").unwrap();
    let json_str = serde_json::to_string(&result).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&json_str).unwrap();

    // Negative duration on test case → None
    assert!(parsed["data"]["testCases"][0]["durationMs"].is_null());
    // Negative suite duration (endTime < startTime) filtered out, fallback also 0 → None
    assert!(parsed["data"]["summary"]["durationMs"].is_null());
}

// ============================================================================
// Path normalization edge cases
// ============================================================================

#[test]
fn test_path_normalization_windows_backslashes() {
    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 1,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 1,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "passed",
                "title": "test",
                "duration": 1,
                "failureMessages": []
            }],
            "startTime": 1000,
            "endTime": 2000,
            "name": "C:\\Users\\ci\\project\\tests\\calculator.test.ts",
            "message": "",
            "status": "passed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    assert_eq!(report["data"]["testCases"][0]["filePath"], "tests/calculator.test.ts");
}

#[test]
fn test_path_normalization_no_marker() {
    let json = r#"{
        "numTotalTests": 1,
        "numPassedTests": 1,
        "numFailedTests": 0,
        "numPendingTests": 0,
        "numTodoTests": 0,
        "numTotalTestSuites": 1,
        "numPassedTestSuites": 1,
        "numFailedTestSuites": 0,
        "numPendingTestSuites": 0,
        "startTime": 1000,
        "success": true,
        "testResults": [{
            "assertionResults": [{
                "fullName": "test",
                "status": "passed",
                "title": "test",
                "duration": 1,
                "failureMessages": []
            }],
            "startTime": 1000,
            "endTime": 2000,
            "name": "/usr/local/lib/my-file.spec.ts",
            "message": "",
            "status": "passed"
        }]
    }"#;

    let report = parse_via_registry(json, "results.json");
    // No marker found → fallback to filename
    assert_eq!(report["data"]["testCases"][0]["filePath"], "my-file.spec.ts");
}
