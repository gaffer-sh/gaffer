use gaffer_parsers::{Parser, ParserRegistry, JUnitParser, LcovParser, ParseError, ParseResult, ResultType};

#[test]
fn test_registry_with_defaults_has_junit() {
    let registry = ParserRegistry::with_defaults();
    let ids = registry.parser_ids();
    assert_eq!(ids, vec!["junit", "jest-vitest-json", "playwright-json", "ctrf", "lcov", "trx", "cobertura", "jacoco", "clover"]);
}

#[test]
fn test_registry_detects_and_parses_junit_xml() {
    let registry = ParserRegistry::with_defaults();
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="test1" classname="suite" time="0.1"/>
        </testsuite>"#;

    let result = registry.parse(xml, "report.xml");
    assert!(result.is_some(), "Registry should detect JUnit XML");

    let parsed = result.unwrap().expect("Parsing should succeed");
    match parsed {
        ParseResult::TestReport(report) => {
            assert_eq!(report.summary.total, 1);
            assert_eq!(report.summary.passed, 1);
        }
        _ => panic!("Expected TestReport"),
    }
}

#[test]
fn test_registry_returns_none_for_json() {
    let registry = ParserRegistry::with_defaults();
    let json = r#"{"tests": [{"name": "test1"}]}"#;

    let result = registry.parse(json, "report.json");
    assert!(result.is_none(), "Registry should not detect JSON as JUnit");
}

#[test]
fn test_registry_returns_none_for_non_junit_xml() {
    let registry = ParserRegistry::with_defaults();
    let xml = r#"<?xml version="1.0"?><config><setting name="foo"/></config>"#;

    let result = registry.parse(xml, "config.xml");
    assert!(result.is_none(), "Registry should not detect non-JUnit XML");
}

#[test]
fn test_registry_duplicate_id_returns_error() {
    let mut registry = ParserRegistry::new();
    registry.register(Box::new(JUnitParser)).unwrap();
    let err = registry.register(Box::new(JUnitParser)).unwrap_err();
    assert_eq!(err.message, "Duplicate parser id: junit");
}

#[test]
fn test_junit_parser_detect_scores() {
    let parser = JUnitParser;

    // XML with <testsuite> → 100
    assert_eq!(parser.detect("<testsuite name=\"s\">", "report.xml"), 100);

    // XML with <testsuites> → 100
    assert_eq!(parser.detect("<testsuites>", "report.xml"), 100);

    // Non-XML filename → 0
    assert_eq!(parser.detect("<testsuite>", "report.json"), 0);

    // XML filename but no JUnit root → 0
    assert_eq!(parser.detect("<config/>", "config.xml"), 0);
}

// --- Tests for review gaps ---

#[test]
fn test_empty_registry_returns_none() {
    let registry = ParserRegistry::new();
    let result = registry.parse("<testsuite/>", "report.xml");
    assert!(result.is_none());
}

#[test]
fn test_full_content_detection_with_trailing_padding() {
    let registry = ParserRegistry::with_defaults();
    let xml = format!(
        "<?xml version=\"1.0\"?>\n<testsuite name=\"s\"><testcase name=\"t\" time=\"0.1\"/></testsuite>{}",
        " ".repeat(3000)
    );
    assert!(registry.parse(&xml, "report.xml").is_some());
}

#[test]
fn test_full_content_detects_markers_past_2kb() {
    // Previously this failed because detection only sampled the first 2KB.
    // Now the full content is passed to detect(), so markers at any offset work.
    let registry = ParserRegistry::with_defaults();
    let padding = " ".repeat(2100);
    let xml = format!(
        "<?xml version=\"1.0\"?>\n{}<testsuite name=\"s\"><testcase name=\"t\" time=\"0.1\"/></testsuite>",
        padding
    );
    assert!(registry.parse(&xml, "report.xml").is_some());
}

#[test]
fn test_full_content_with_multibyte_utf8() {
    let registry = ParserRegistry::with_defaults();
    let prefix = "<?xml version=\"1.0\"?>\n<testsuite name=\"s\"><testcase name=\"t\" time=\"0.1\"/></testsuite>";
    let filler = "\u{00E9}".repeat(1500); // e-acute is 2 bytes each = 3000 bytes
    let xml = format!("{}{}", prefix, filler);
    let result = registry.parse(&xml, "report.xml");
    assert!(result.is_some());
}

#[test]
fn test_detect_score_below_threshold_excluded() {
    // JUnit returns 0 for non-JUnit XML (below threshold of 50)
    let registry = ParserRegistry::with_defaults();
    let xml = r#"<?xml version="1.0"?><not-junit/>"#;
    assert!(registry.parse(xml, "report.xml").is_none());
}

// Tie-breaking and priority tests using fake parsers

struct FakeParser {
    id: &'static str,
    prio: u8,
    score: u8,
}

impl Parser for FakeParser {
    fn id(&self) -> &str { self.id }
    fn name(&self) -> &str { "Fake" }
    fn priority(&self) -> u8 { self.prio }
    fn result_type(&self) -> ResultType { ResultType::TestReport }
    fn detect(&self, _sample: &str, _filename: &str) -> u8 { self.score }
    fn parse(&self, _content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        Err(ParseError { message: self.id.to_string() })
    }
}

#[test]
fn test_tiebreak_by_priority() {
    let mut registry = ParserRegistry::new();
    registry.register(Box::new(FakeParser { id: "low-pri", prio: 10, score: 80 })).unwrap();
    registry.register(Box::new(FakeParser { id: "high-pri", prio: 90, score: 80 })).unwrap();

    let result = registry.parse("anything", "file.xml");
    let err = result.unwrap().unwrap_err();
    assert_eq!(err.message, "high-pri");
}

#[test]
fn test_higher_detect_score_wins_over_priority() {
    let mut registry = ParserRegistry::new();
    registry.register(Box::new(FakeParser { id: "low-score", prio: 100, score: 70 })).unwrap();
    registry.register(Box::new(FakeParser { id: "high-score", prio: 10, score: 90 })).unwrap();

    let result = registry.parse("anything", "file.xml");
    let err = result.unwrap().unwrap_err();
    assert_eq!(err.message, "high-score");
}

#[test]
fn test_score_at_threshold_is_included() {
    let mut registry = ParserRegistry::new();
    registry.register(Box::new(FakeParser { id: "edge", prio: 50, score: 50 })).unwrap();

    assert!(registry.parse("content", "file.xml").is_some());
}

#[test]
fn test_score_below_threshold_is_excluded() {
    let mut registry = ParserRegistry::new();
    registry.register(Box::new(FakeParser { id: "low", prio: 90, score: 49 })).unwrap();

    assert!(registry.parse("content", "file.xml").is_none());
}

// --- Cross-type guard tests ---
// These verify that parse_report/parse_coverage WASM exports correctly reject
// cross-type results. We test at the registry level since WASM exports aren't
// callable in native tests.

#[test]
fn test_registry_coverage_result_from_lcov() {
    let registry = ParserRegistry::with_defaults();
    let lcov = "TN:\nSF:/src/file.ts\nDA:1,1\nLF:1\nLH:1\nend_of_record\n";

    let result = registry.parse(lcov, "coverage.lcov");
    assert!(result.is_some(), "Registry should detect LCOV");

    match result.unwrap().unwrap() {
        ParseResult::Coverage(report) => {
            assert_eq!(report.format, "lcov");
        }
        other => panic!("Expected Coverage, got {:?}", other),
    }
}

#[test]
fn test_registry_test_report_not_coverage() {
    let registry = ParserRegistry::with_defaults();
    let xml = r#"<?xml version="1.0"?>
        <testsuite name="suite">
            <testcase name="test1" classname="suite" time="0.1"/>
        </testsuite>"#;

    let result = registry.parse(xml, "report.xml");
    match result.unwrap().unwrap() {
        ParseResult::TestReport(_) => {} // correct
        ParseResult::Coverage(_) => panic!("JUnit should not produce Coverage result"),
    }
}

#[test]
fn test_lcov_parser_result_type_is_coverage() {
    let parser = LcovParser;
    assert_eq!(parser.result_type(), ResultType::Coverage);
}

#[test]
fn test_junit_parser_result_type_is_test_report() {
    let parser = JUnitParser;
    assert_eq!(parser.result_type(), ResultType::TestReport);
}
