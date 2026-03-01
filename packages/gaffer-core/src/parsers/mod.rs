//! Report file parsers — delegates detection and parsing to `gaffer_parsers::ParserRegistry`.

use std::path::Path;

pub use gaffer_parsers::ResultType;
use gaffer_parsers::ParserRegistry;

use crate::types::TestEvent;

/// Result of parsing a report file.
#[derive(Debug)]
pub struct ParsedReport {
    pub framework: String,
    pub tests: Vec<TestEvent>,
    pub duration_ms: Option<u64>,
}

/// Detect whether a file is a test report or coverage, using `ParserRegistry`.
/// Returns `None` if no parser matched.
pub fn detect_result_type(path: &Path, content: &str) -> Option<ResultType> {
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let registry = ParserRegistry::with_defaults();
    registry.detect(content, filename).map(|m| m.result_type)
}

/// Parse a report file, auto-detecting format via `ParserRegistry`.
/// Returns Err if no parser matched or parsing fails.
pub fn parse_report(path: &Path, content: &str) -> Result<ParsedReport, String> {
    let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    let registry = ParserRegistry::with_defaults();

    match registry.parse(content, filename) {
        Some(Ok(gaffer_parsers::ParseResult::TestReport(report))) => {
            Ok(convert_parsed_report(report))
        }
        Some(Ok(gaffer_parsers::ParseResult::Coverage(_))) => {
            Err("File is a coverage report, not a test report".to_string())
        }
        Some(Err(e)) => Err(e.message),
        None => Err(format!("Unknown report format for {}", path.display())),
    }
}

/// Convert a gaffer_parsers::ParsedReport into gaffer_core's ParsedReport.
fn convert_parsed_report(report: gaffer_parsers::types::ParsedReport) -> ParsedReport {
    ParsedReport {
        framework: report.framework,
        tests: report.test_cases.into_iter().map(convert_test_case).collect(),
        duration_ms: report.summary.duration_ms,
    }
}

/// Convert a gaffer_parsers::TestCase into gaffer_core's TestEvent.
fn convert_test_case(tc: gaffer_parsers::types::TestCase) -> TestEvent {
    use crate::types::status;
    use gaffer_parsers::types::TestStatus;

    let (test_status, flaky) = match tc.status {
        TestStatus::Passed => (status::PASSED, None),
        TestStatus::Failed | TestStatus::TimedOut => (status::FAILED, None),
        TestStatus::Skipped => (status::SKIPPED, None),
        TestStatus::Flaky => (status::PASSED, Some(true)),
    };

    TestEvent {
        name: tc.full_name,
        status: test_status.to_string(),
        duration: tc.duration_ms.unwrap_or(0) as f64,
        file_path: tc.file_path,
        error: tc.error_message,
        retry_count: tc.retry_attempt.map(|r| r as i32),
        flaky,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_junit_xml() {
        let content = r#"<?xml version="1.0"?><testsuites><testsuite name="t"><testcase name="a" time="0.1"/></testsuite></testsuites>"#;
        let result = detect_result_type(Path::new("report.xml"), content);
        assert_eq!(result, Some(ResultType::TestReport));
    }

    #[test]
    fn detect_lcov() {
        let content = "TN:\nSF:src/file.ts\nDA:1,1\nLF:1\nLH:1\nend_of_record\n";
        let result = detect_result_type(Path::new("lcov.info"), content);
        assert_eq!(result, Some(ResultType::Coverage));
    }

    #[test]
    fn detect_unknown() {
        let result = detect_result_type(Path::new("readme.txt"), "hello world");
        assert_eq!(result, None);
    }

    #[test]
    fn detect_trx_file() {
        let content = r#"<?xml version="1.0"?><TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010"><Results><UnitTestResult testName="t" outcome="Passed" duration="00:00:01"/></Results></TestRun>"#;
        let result = detect_result_type(Path::new("results.trx"), content);
        assert_eq!(result, Some(ResultType::TestReport));
    }

    #[test]
    fn detect_cobertura_xml() {
        let content = r#"<?xml version="1.0"?><coverage line-rate="0.5" branch-rate="0.3"><packages><package name="p"><classes></classes></package></packages></coverage>"#;
        let result = detect_result_type(Path::new("cobertura.xml"), content);
        assert_eq!(result, Some(ResultType::Coverage));
    }

    #[test]
    fn parse_junit_report() {
        let content = r#"<?xml version="1.0"?>
<testsuites>
  <testsuite name="tests" tests="2" time="0.5">
    <testcase name="passes" classname="tests" time="0.1"/>
    <testcase name="fails" classname="tests" time="0.2">
      <failure message="Expected 200, got 500"/>
    </testcase>
  </testsuite>
</testsuites>"#;
        let result = parse_report(Path::new("junit.xml"), content).unwrap();
        assert_eq!(result.tests.len(), 2);
        assert_eq!(result.tests[0].status, "passed");
        assert_eq!(result.tests[1].status, "failed");
    }

    #[test]
    fn parse_trx_report() {
        let content = r#"<?xml version="1.0"?>
            <TestRun id="abc" name="Test Run" xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult testName="Ns.Class.PassingTest" outcome="Passed" duration="00:00:01.000"/>
                    <UnitTestResult testName="Ns.Class.FailingTest" outcome="Failed" duration="00:00:00.500">
                        <Output><ErrorInfo><Message>Expected true</Message></ErrorInfo></Output>
                    </UnitTestResult>
                </Results>
            </TestRun>"#;
        let result = parse_report(Path::new("results.trx"), content).unwrap();
        assert_eq!(result.framework, "trx");
        assert_eq!(result.tests.len(), 2);
        assert_eq!(result.tests[0].status, "passed");
        assert_eq!(result.tests[1].status, "failed");
        assert!(result.tests[1].error.is_some());
    }

    #[test]
    fn parse_jest_vitest_json_fixture() {
        let content = r#"{
            "numTotalTests": 2,
            "numPassedTests": 1,
            "numFailedTests": 1,
            "numPendingTests": 0,
            "numTodoTests": 0,
            "numTotalTestSuites": 1,
            "numPassedTestSuites": 0,
            "numFailedTestSuites": 1,
            "numPendingTestSuites": 0,
            "startTime": 1700000000000,
            "success": false,
            "testResults": [
                {
                    "name": "/project/src/math.test.ts",
                    "startTime": 1700000000000,
                    "endTime": 1700000000100,
                    "assertionResults": [
                        {
                            "fullName": "math adds numbers",
                            "title": "adds numbers",
                            "status": "passed",
                            "duration": 5,
                            "failureMessages": []
                        },
                        {
                            "fullName": "math subtracts numbers",
                            "title": "subtracts numbers",
                            "status": "failed",
                            "duration": 3,
                            "failureMessages": ["Expected 1 to be 2"]
                        }
                    ]
                }
            ]
        }"#;

        let result = parse_report(Path::new("vitest-results.json"), content).unwrap();
        assert_eq!(result.tests.len(), 2);
        assert_eq!(result.tests[0].name, "math adds numbers");
        assert_eq!(result.tests[0].status, "passed");
        assert_eq!(result.tests[0].duration, 5.0);
        assert_eq!(result.tests[1].name, "math subtracts numbers");
        assert_eq!(result.tests[1].status, "failed");
        assert!(result.tests[1].error.is_some());
    }

    #[test]
    fn parse_playwright_json_fixture() {
        let content = r#"{
            "config": {
                "projects": [
                    { "id": "default", "name": "chromium" }
                ]
            },
            "suites": [
                {
                    "title": "login.spec.ts",
                    "file": "tests/login.spec.ts",
                    "line": 0,
                    "column": 0,
                    "specs": [
                        {
                            "title": "should log in",
                            "ok": true,
                            "id": "spec-1",
                            "file": "tests/login.spec.ts",
                            "line": 3,
                            "column": 1,
                            "tests": [
                                {
                                    "projectName": "chromium",
                                    "projectId": "default",
                                    "status": "expected",
                                    "results": [
                                        {
                                            "status": "passed",
                                            "duration": 1200,
                                            "retry": 0,
                                            "attachments": []
                                        }
                                    ]
                                }
                            ]
                        },
                        {
                            "title": "should show error",
                            "ok": false,
                            "id": "spec-2",
                            "file": "tests/login.spec.ts",
                            "line": 10,
                            "column": 1,
                            "tests": [
                                {
                                    "projectName": "chromium",
                                    "projectId": "default",
                                    "status": "unexpected",
                                    "results": [
                                        {
                                            "status": "failed",
                                            "duration": 800,
                                            "retry": 0,
                                            "errors": [
                                                { "message": "Locator not found" }
                                            ],
                                            "attachments": []
                                        }
                                    ]
                                }
                            ]
                        }
                    ]
                }
            ],
            "errors": [],
            "stats": {
                "startTime": "2024-01-01T00:00:00.000Z",
                "duration": 2000,
                "expected": 1,
                "unexpected": 1,
                "flaky": 0,
                "skipped": 0
            }
        }"#;

        let result = parse_report(Path::new("playwright-results.json"), content).unwrap();
        assert_eq!(result.framework, "playwright");
        assert_eq!(result.tests.len(), 2);
        assert_eq!(result.tests[0].status, "passed");
        assert_eq!(result.tests[1].status, "failed");
        assert!(result.tests[1].error.is_some());
    }

    #[test]
    fn convert_test_case_timed_out_maps_to_failed() {
        use gaffer_parsers::types::{TestCase, TestStatus};
        let tc = TestCase {
            id: "t1".to_string(),
            name: "slow test".to_string(),
            full_name: "slow test".to_string(),
            status: TestStatus::TimedOut,
            duration_ms: Some(30000),
            error_message: Some("Timeout exceeded".to_string()),
            file_path: None,
            line: None,
            retry_attempt: None,
        };
        let result = convert_test_case(tc);
        assert_eq!(result.status, "failed");
        assert_eq!(result.flaky, None);
    }

    #[test]
    fn convert_test_case_flaky_maps_to_passed_with_flaky_flag() {
        use gaffer_parsers::types::{TestCase, TestStatus};
        let tc = TestCase {
            id: "t2".to_string(),
            name: "flaky test".to_string(),
            full_name: "flaky test".to_string(),
            status: TestStatus::Flaky,
            duration_ms: Some(500),
            error_message: None,
            file_path: None,
            line: None,
            retry_attempt: Some(1),
        };
        let result = convert_test_case(tc);
        assert_eq!(result.status, "passed");
        assert_eq!(result.flaky, Some(true));
    }
}
