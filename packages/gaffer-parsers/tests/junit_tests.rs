use gaffer_parsers::{JUnitParser, Parser, ParseResult};

fn parse_and_check(input: &str) -> serde_json::Value {
    let parser = JUnitParser;
    let result = parser.parse(input, "report.xml").expect("Expected success but parse returned Err");
    match result {
        ParseResult::TestReport(report) => serde_json::to_value(&report).expect("Failed to serialize report"),
        other => panic!("Expected Report, got {:?}", other),
    }
}

fn load_fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).expect(&format!("Failed to read fixture: {}", path))
}

fn load_fixture_bytes(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/tests/fixtures/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read(&path).expect(&format!("Failed to read fixture: {}", path))
}

// ============================================================================
// valid-report.xml tests
// ============================================================================

#[test]
fn test_wrapped_format_all_passing() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["summary"]["total"], 5);
    assert_eq!(report["summary"]["passed"], 5);
    assert_eq!(report["summary"]["failed"], 0);
    assert_eq!(report["summary"]["skipped"], 0);
    assert_eq!(report["summary"]["flaky"], 0);
}

#[test]
fn test_duration_conversion() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    // First test: time="0.15" → 150ms
    assert_eq!(report["testCases"][0]["durationMs"], 150);
}

#[test]
fn test_summary_duration_from_suites() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    // Top-level suites: time="0.5" + time="0.734" = 1.234s = 1234ms
    assert_eq!(report["summary"]["durationMs"], 1234);
}

#[test]
fn test_fullname_construction() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    // classname="tests/unit/math.spec.ts", name="adds numbers correctly"
    // classname != name → "classname > name"
    assert_eq!(
        report["testCases"][0]["fullName"],
        "tests/unit/math.spec.ts > adds numbers correctly"
    );
}

#[test]
fn test_test_case_ids() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["testCases"][0]["id"], "tc-1");
    assert_eq!(report["testCases"][1]["id"], "tc-2");
    assert_eq!(report["testCases"][4]["id"], "tc-5");
}

// ============================================================================
// with-failures.xml tests
// ============================================================================

#[test]
fn test_failures_and_errors() {
    let xml = load_fixture("with-failures.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["summary"]["total"], 6);
    assert_eq!(report["summary"]["passed"], 2);
    assert_eq!(report["summary"]["failed"], 3); // 2 failures + 1 error
    assert_eq!(report["summary"]["skipped"], 1);
}

#[test]
fn test_failure_error_message() {
    let xml = load_fixture("with-failures.xml");
    let report = parse_and_check(&xml);

    let test_cases = report["testCases"].as_array().unwrap();
    let failed_test = test_cases
        .iter()
        .find(|tc| tc["name"] == "rejects invalid credentials")
        .expect("Should find test");

    assert_eq!(failed_test["status"], "failed");
    assert_eq!(failed_test["errorMessage"], "Expected 401 but got 200");
}

#[test]
fn test_error_treated_as_failure() {
    let xml = load_fixture("with-failures.xml");
    let report = parse_and_check(&xml);

    let test_cases = report["testCases"].as_array().unwrap();
    let error_test = test_cases
        .iter()
        .find(|tc| tc["name"] == "handles timeout")
        .expect("Should find test");

    assert_eq!(error_test["status"], "failed");
    assert_eq!(error_test["errorMessage"], "Connection timeout");
}

#[test]
fn test_skipped_status() {
    let xml = load_fixture("with-failures.xml");
    let report = parse_and_check(&xml);

    let test_cases = report["testCases"].as_array().unwrap();
    let skipped_test = test_cases
        .iter()
        .find(|tc| tc["name"] == "pending feature")
        .expect("Should find test");

    assert_eq!(skipped_test["status"], "skipped");
}

// ============================================================================
// single-suite.xml tests
// ============================================================================

#[test]
fn test_bare_testsuite_format() {
    let xml = load_fixture("single-suite.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["summary"]["total"], 3);
    assert_eq!(report["summary"]["passed"], 3);
}

#[test]
fn test_file_and_line_extraction() {
    let xml = load_fixture("single-suite.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["testCases"][0]["filePath"], "tests/single.spec.ts");
    assert_eq!(report["testCases"][0]["line"], 5);
    assert_eq!(report["testCases"][1]["line"], 10);
}

// ============================================================================
// BOM handling
// ============================================================================

#[test]
fn test_bom_handling() {
    let bytes = load_fixture_bytes("bom.xml");
    // Verify it actually has a BOM
    assert_eq!(&bytes[0..3], &[0xEF, 0xBB, 0xBF], "Fixture should have BOM");

    let input = String::from_utf8(bytes).expect("Valid UTF-8");
    let report = parse_and_check(&input);

    assert_eq!(report["testCases"].as_array().unwrap().len(), 1);
    assert_eq!(report["testCases"][0]["name"], "handles BOM");
    assert_eq!(report["testCases"][0]["status"], "passed");
}

// ============================================================================
// Nested testsuites
// ============================================================================

#[test]
fn test_nested_testsuites() {
    let xml = load_fixture("nested-suites.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["summary"]["total"], 4);
    assert_eq!(report["summary"]["passed"], 3);
    assert_eq!(report["summary"]["failed"], 1);

    let test_cases = report["testCases"].as_array().unwrap();
    let api_test = test_cases
        .iter()
        .find(|tc| tc["name"] == "testApiEndpoint")
        .expect("Should find test");
    assert_eq!(api_test["status"], "failed");
    assert_eq!(api_test["errorMessage"], "Status code mismatch");
}

// ============================================================================
// Mixed nested/direct testcases (document order)
// ============================================================================

#[test]
fn test_mixed_nested_and_direct_testcases() {
    // When a <testsuite> has both direct <testcase> and nested <testsuite>
    // interspersed, the Rust streaming parser emits testcases in document order.
    // Note: the JS DOM parser (fast-xml-parser) groups by tag name and would
    // produce a different ordering. This pattern is rare in real-world JUnit XML.
    let xml = load_fixture("mixed-content.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["summary"]["total"], 3);
    assert_eq!(report["summary"]["passed"], 3);

    let test_cases = report["testCases"].as_array().unwrap();
    assert_eq!(test_cases[0]["name"], "direct-first");
    assert_eq!(test_cases[1]["name"], "nested-test");
    assert_eq!(test_cases[2]["name"], "direct-second");
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn test_missing_attributes() {
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="minimal">
            <testcase name="test"/>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"][0]["name"], "test");
    assert_eq!(report["testCases"][0]["durationMs"], 0);
}

#[test]
fn test_empty_testsuites_returns_error() {
    let xml = r#"<?xml version="1.0"?><testsuites></testsuites>"#;
    let parser = JUnitParser;
    let result = parser.parse(xml, "test.xml");
    assert!(result.is_err(), "Expected error for empty testsuites");
}

#[test]
fn test_invalid_xml_returns_error() {
    let xml = "not valid xml <testsuite";
    let parser = JUnitParser;
    let result = parser.parse(xml, "test.xml");
    assert!(result.is_err(), "Expected error for invalid XML");
}

#[test]
fn test_metadata() {
    let xml = load_fixture("valid-report.xml");
    let report = parse_and_check(&xml);

    assert_eq!(report["framework"], "junit");
    assert_eq!(report["metadata"]["suiteName"], "Vitest Tests");
    assert_eq!(report["metadata"]["suiteCount"], 2);
}

#[test]
fn test_self_closing_testcase() {
    // Self-closing <testcase .../> should work
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="self-closing" classname="suite" time="0.1"/>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"].as_array().unwrap().len(), 1);
    assert_eq!(report["testCases"][0]["name"], "self-closing");
    assert_eq!(report["testCases"][0]["status"], "passed");
    assert_eq!(report["testCases"][0]["durationMs"], 100);
}

#[test]
fn test_self_closing_skipped() {
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="skipped-test" classname="suite" time="0">
                <skipped/>
            </testcase>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"][0]["status"], "skipped");
}

#[test]
fn test_status_priority_skipped_over_error() {
    // If a testcase has both <skipped/> and <error>, skipped wins (matching JS priority)
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="both" classname="suite" time="0">
                <skipped/>
                <error message="some error"/>
            </testcase>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"][0]["status"], "skipped");
}

#[test]
fn test_error_text_content_fallback() {
    // When <error> has no @message, fall back to text content
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="error-text" classname="suite" time="0.1">
                <error>NullPointerException at line 42</error>
            </testcase>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"][0]["status"], "failed");
    assert_eq!(
        report["testCases"][0]["errorMessage"],
        "NullPointerException at line 42"
    );
}

#[test]
fn test_failure_text_content_fallback() {
    // When <failure> has no @message, fall back to text content
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="fail-text" classname="suite" time="0.1">
                <failure>assertion failed: expected true</failure>
            </testcase>
        </testsuite>"#;
    let report = parse_and_check(xml);

    assert_eq!(report["testCases"][0]["status"], "failed");
    assert_eq!(
        report["testCases"][0]["errorMessage"],
        "assertion failed: expected true"
    );
}

#[test]
fn test_summary_duration_fallback_to_testcase_times() {
    // When suite has no @time, summary duration should sum testcase times
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="no-time-suite">
            <testcase name="a" classname="suite" time="0.5"/>
            <testcase name="b" classname="suite" time="0.3"/>
        </testsuite>"#;
    let report = parse_and_check(xml);

    // 0.5 + 0.3 = 0.8s = 800ms
    assert_eq!(report["summary"]["durationMs"], 800);
}

#[test]
fn test_fullname_same_as_name_when_classname_equals_name() {
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="mytest" classname="mytest" time="0.1"/>
        </testsuite>"#;
    let report = parse_and_check(xml);

    // When classname == name, fullName should just be name
    assert_eq!(report["testCases"][0]["fullName"], "mytest");
}

#[test]
fn test_bare_testsuite_metadata() {
    let xml = load_fixture("single-suite.xml");
    let report = parse_and_check(&xml);

    // No <testsuites> root, so suiteName should be null
    assert!(report["metadata"]["suiteName"].is_null());
    // One top-level suite
    assert_eq!(report["metadata"]["suiteCount"], 1);
}

#[test]
fn test_suite_file_inherited_by_testcase() {
    // Suite has @file, testcases inherit it
    let xml = load_fixture("single-suite.xml");
    let report = parse_and_check(&xml);

    // All testcases in single-suite.xml have explicit @file, but the suite also has @file
    // The testcase's own @file takes priority
    assert_eq!(report["testCases"][0]["filePath"], "tests/single.spec.ts");
}
