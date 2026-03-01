use gaffer_parsers::{TrxParser, Parser, ParserRegistry, ParseResult, TestStatus};

fn load_fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/trx/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e))
}

fn parse_fixture(name: &str) -> gaffer_parsers::types::ParsedReport {
    let content = load_fixture(name);
    let parser = TrxParser;
    match parser.parse(&content, name).unwrap() {
        ParseResult::TestReport(report) => report,
        _ => panic!("Expected TestReport"),
    }
}

// ============================================================================
// xUnit fixture tests
// ============================================================================

#[test]
fn xunit_summary_counts() {
    let report = parse_fixture("xunit-results.trx");
    assert_eq!(report.summary.total, 15);
    assert_eq!(report.summary.passed, 13);
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.summary.skipped, 1);
}

#[test]
fn xunit_framework_is_trx() {
    let report = parse_fixture("xunit-results.trx");
    assert_eq!(report.framework, "trx");
}

#[test]
fn xunit_all_test_cases_extracted() {
    let report = parse_fixture("xunit-results.trx");
    assert_eq!(report.test_cases.len(), 15);
}

#[test]
fn xunit_passing_test() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "Add_TwoPositiveNumbers_ReturnsSum").unwrap();
    assert_eq!(tc.status, TestStatus::Passed);
}

#[test]
fn xunit_full_name_from_dotted_test_name() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "Add_TwoPositiveNumbers_ReturnsSum").unwrap();
    assert_eq!(tc.full_name, "Calculator.XUnit.Tests.CalculatorTests > Add_TwoPositiveNumbers_ReturnsSum");
}

#[test]
fn xunit_failed_test_with_error() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "FailingTest_DemonstratesFailure").unwrap();
    assert_eq!(tc.status, TestStatus::Failed);
    assert!(tc.error_message.as_ref().unwrap().contains("Assert.Equal() Failure"));
}

#[test]
fn xunit_skipped_test() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "SkippedTest_DemonstratesSkip").unwrap();
    assert_eq!(tc.status, TestStatus::Skipped);
}

#[test]
fn xunit_parameterized_test() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name.contains("Add_MultipleInputs_ReturnsExpectedSum(a: 1")).unwrap();
    assert_eq!(tc.status, TestStatus::Passed);
}

#[test]
fn xunit_metadata_run_id() {
    let report = parse_fixture("xunit-results.trx");
    assert_eq!(report.metadata["runId"], "f416979a-31af-42ee-ac6f-26f2b5a62ef5");
}

#[test]
fn xunit_metadata_run_name() {
    let report = parse_fixture("xunit-results.trx");
    assert!(report.metadata["runName"].as_str().unwrap().contains("2025-12-16"));
}

#[test]
fn xunit_duration_positive() {
    let report = parse_fixture("xunit-results.trx");
    assert!(report.summary.duration_ms.unwrap() > 0);
}

#[test]
fn xunit_test_duration_reasonable() {
    let report = parse_fixture("xunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "Subtract_TwoNumbers_ReturnsDifference").unwrap();
    // Duration is 00:00:00.0036232 = ~4ms
    let ms = tc.duration_ms.unwrap();
    assert!(ms > 0 && ms < 10, "Expected ~4ms, got {}ms", ms);
}

// ============================================================================
// NUnit fixture tests
// ============================================================================

#[test]
fn nunit_summary_counts() {
    let report = parse_fixture("nunit-results.trx");
    assert_eq!(report.summary.total, 16);
    assert_eq!(report.summary.passed, 13);
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.summary.skipped, 2); // SkippedTest + InconclusiveTest
}

#[test]
fn nunit_inconclusive_mapped_to_skipped() {
    let report = parse_fixture("nunit-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "InconclusiveTest_DemonstratesInconclusive").unwrap();
    assert_eq!(tc.status, TestStatus::Skipped);
}

// ============================================================================
// MSTest fixture tests
// ============================================================================

#[test]
fn mstest_summary_counts() {
    let report = parse_fixture("mstest-results.trx");
    assert_eq!(report.summary.total, 16);
    assert_eq!(report.summary.passed, 13);
    assert_eq!(report.summary.failed, 1);
    assert_eq!(report.summary.skipped, 2); // SkippedTest + InconclusiveTest
}

#[test]
fn mstest_failure_message() {
    let report = parse_fixture("mstest-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name == "FailingTest_DemonstratesFailure").unwrap();
    assert_eq!(tc.status, TestStatus::Failed);
    assert!(tc.error_message.as_ref().unwrap().contains("Assert.AreEqual failed"));
}

#[test]
fn mstest_parameterized_with_space() {
    let report = parse_fixture("mstest-results.trx");
    let tc = report.test_cases.iter().find(|t| t.name.contains("Add_MultipleInputs_ReturnsExpectedSum (1,2,3)")).unwrap();
    assert_eq!(tc.status, TestStatus::Passed);
}

// ============================================================================
// Registry detection
// ============================================================================

#[test]
fn registry_detects_trx_file() {
    let content = load_fixture("xunit-results.trx");
    let registry = ParserRegistry::with_defaults();
    let result = registry.parse(&content, "report.trx");
    assert!(result.is_some(), "Registry should detect TRX report");
    match result.unwrap().unwrap() {
        ParseResult::TestReport(report) => {
            assert_eq!(report.framework, "trx");
            assert_eq!(report.summary.total, 15);
        }
        _ => panic!("Expected TestReport"),
    }
}

#[test]
fn registry_includes_trx_parser() {
    let registry = ParserRegistry::with_defaults();
    let ids = registry.parser_ids();
    assert!(ids.contains(&"trx"), "Registry should include TRX parser");
}
