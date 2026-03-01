use quick_xml::events::Event;
use quick_xml::Reader;

use crate::registry::Parser;
use crate::types::{ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus};
use crate::xml_helpers::{get_attr, local_name, strip_bom};

pub struct TrxParser;

impl Parser for TrxParser {
    fn id(&self) -> &str {
        "trx"
    }

    fn name(&self) -> &str {
        "Visual Studio TRX Report"
    }

    fn priority(&self) -> u8 {
        88
    }

    fn result_type(&self) -> ResultType {
        ResultType::TestReport
    }

    fn detect(&self, sample: &str, filename: &str) -> u8 {
        let is_trx = filename.ends_with(".trx");
        let is_xml = filename.ends_with(".xml");

        if !is_trx && !is_xml {
            return 0;
        }

        let has_test_run = sample.contains("<TestRun");
        if !has_test_run {
            return 0;
        }

        if is_trx {
            return 100;
        }

        let has_trx_namespace = sample.contains("microsoft.com/schemas/VisualStudio/TeamTest");
        if has_trx_namespace {
            95
        } else {
            70
        }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        parse_trx(content)
            .map(ParseResult::TestReport)
            .map_err(ParseError::from)
    }
}

/// Parse .NET TimeSpan duration format to milliseconds.
/// Formats: "HH:mm:ss.fffffff" or "d.HH:mm:ss.fffffff"
fn parse_dotnet_timespan(duration: &str) -> u64 {
    let parts: Vec<&str> = duration.split(':').collect();
    if parts.len() < 3 {
        return 0;
    }

    // First segment is either "HH" or "d.HH"
    let hours = if let Some(dot_pos) = parts[0].find('.') {
        let days: u64 = parts[0][..dot_pos].parse().unwrap_or(0);
        let h: u64 = parts[0][dot_pos + 1..].parse().unwrap_or(0);
        days * 24 + h
    } else {
        parts[0].parse::<u64>().unwrap_or(0)
    };

    let minutes: u64 = parts[1].parse().unwrap_or(0);
    let seconds: f64 = parts[2].parse().unwrap_or(0.0);

    let total_ms = (hours * 3600 + minutes * 60) as f64 * 1000.0 + seconds * 1000.0;
    total_ms.round() as u64
}

/// Extract class name and method name from TRX testName.
/// Format: "Namespace.Class.Method" or "Namespace.Class.Method(params)"
fn extract_class_and_method(test_name: &str) -> (String, String) {
    // Handle parameterized tests: split at first '(' but handle " (" (MSTest style)
    let (base_name, params) = if let Some(paren_pos) = test_name.find('(') {
        // Check for MSTest space before paren: "Method (params)"
        let effective_pos = if paren_pos > 0 && test_name.as_bytes()[paren_pos - 1] == b' ' {
            paren_pos - 1
        } else {
            paren_pos
        };
        (&test_name[..effective_pos], &test_name[effective_pos..])
    } else {
        (test_name, "")
    };

    match base_name.rsplit_once('.') {
        Some((class, method)) => (class.to_string(), format!("{}{}", method, params)),
        None => (String::new(), test_name.to_string()),
    }
}

/// Build a TestCase from TRX test result attributes.
fn build_test_case(
    id: usize,
    test_name: &str,
    outcome: &str,
    duration: &str,
    error_message: Option<String>,
) -> TestCase {
    let (class_name, method_name) = extract_class_and_method(test_name);
    let full_name = if class_name.is_empty() {
        method_name.clone()
    } else {
        format!("{} > {}", class_name, method_name)
    };

    TestCase {
        id: format!("tc-{}", id),
        name: method_name,
        full_name,
        status: map_outcome(outcome),
        duration_ms: Some(parse_dotnet_timespan(duration)),
        error_message,
        file_path: None,
        line: None,
        retry_attempt: None,
    }
}

/// Map TRX outcome value to Gaffer test status.
fn map_outcome(outcome: &str) -> TestStatus {
    match outcome.to_lowercase().as_str() {
        "passed" => TestStatus::Passed,
        "failed" | "error" | "aborted" => TestStatus::Failed,
        "notexecuted" | "notrunnable" | "inconclusive" => TestStatus::Skipped,
        "timeout" => TestStatus::TimedOut,
        unknown => {
            #[cfg(feature = "logging")]
            eprintln!("TRX: unknown outcome '{}', mapping to Skipped", unknown);
            let _ = unknown; // suppress unused warning when logging feature is off
            TestStatus::Skipped
        }
    }
}

#[derive(PartialEq)]
enum InsideElement {
    None,
    Message,
}

pub fn parse_trx(input: &str) -> Result<ParsedReport, String> {
    let input = strip_bom(input);

    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);

    let mut test_cases: Vec<TestCase> = Vec::new();
    let mut id_counter: usize = 0;

    let mut run_id: Option<String> = None;
    let mut run_name: Option<String> = None;

    // State tracking
    let mut in_results = false;
    let mut in_unit_test_result = false;
    let mut in_error_info = false;
    let mut inside_element = InsideElement::None;

    // Current test result state
    let mut current_test_name = String::new();
    let mut current_outcome = String::new();
    let mut current_duration = String::new();
    let mut current_error_message: Option<String> = None;
    let mut text_buf = String::new();

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(format!(
                    "XML parse error at position {}: {}",
                    reader.error_position(),
                    e
                ))
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    b"TestRun" => {
                        run_id = get_attr(e, b"id");
                        run_name = get_attr(e, b"name");
                    }
                    b"Results" => {
                        in_results = true;
                    }
                    b"UnitTestResult" if in_results => {
                        in_unit_test_result = true;
                        current_test_name = get_attr(e, b"testName")
                            .unwrap_or_else(|| "Unknown Test".to_string());
                        current_outcome = get_attr(e, b"outcome")
                            .unwrap_or_else(|| "Unknown".to_string());
                        current_duration = get_attr(e, b"duration")
                            .unwrap_or_default();
                        current_error_message = None;
                    }
                    b"ErrorInfo" if in_unit_test_result => {
                        in_error_info = true;
                    }
                    b"Message" if in_error_info => {
                        text_buf.clear();
                        inside_element = InsideElement::Message;
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                if local == b"UnitTestResult" && in_results {
                    let test_name = get_attr(e, b"testName")
                        .unwrap_or_else(|| "Unknown Test".to_string());
                    let outcome = get_attr(e, b"outcome")
                        .unwrap_or_else(|| "Unknown".to_string());
                    let duration = get_attr(e, b"duration")
                        .unwrap_or_default();

                    id_counter += 1;
                    test_cases.push(build_test_case(
                        id_counter, &test_name, &outcome, &duration, None,
                    ));
                }
            }
            Ok(Event::End(ref e)) => {
                let name = e.name();
                let local = local_name(name.as_ref());
                match local {
                    b"Results" => {
                        in_results = false;
                    }
                    b"UnitTestResult" if in_unit_test_result => {
                        id_counter += 1;
                        test_cases.push(build_test_case(
                            id_counter,
                            &current_test_name,
                            &current_outcome,
                            &current_duration,
                            current_error_message.take(),
                        ));

                        in_unit_test_result = false;
                        in_error_info = false;
                        inside_element = InsideElement::None;
                    }
                    b"ErrorInfo" => {
                        in_error_info = false;
                    }
                    b"Message" if inside_element == InsideElement::Message => {
                        let trimmed = text_buf.trim().to_string();
                        if !trimmed.is_empty() {
                            current_error_message = Some(trimmed);
                        }
                        inside_element = InsideElement::None;
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if inside_element != InsideElement::None {
                    if let Ok(text) = e.unescape() {
                        text_buf.push_str(&text);
                    }
                }
            }
            Ok(Event::CData(ref e)) => {
                if inside_element != InsideElement::None {
                    if let Ok(text) = std::str::from_utf8(e.as_ref()) {
                        text_buf.push_str(text);
                    }
                }
            }
            _ => {}
        }
    }

    if test_cases.is_empty() {
        return Err("No test cases found in TRX".to_string());
    }

    // Calculate summary
    let passed = test_cases.iter().filter(|tc| tc.status == TestStatus::Passed).count();
    let failed = test_cases.iter().filter(|tc| tc.status == TestStatus::Failed).count();
    let timed_out = test_cases.iter().filter(|tc| tc.status == TestStatus::TimedOut).count();
    let skipped = test_cases.iter().filter(|tc| tc.status == TestStatus::Skipped).count();
    let total = test_cases.len();

    let duration_ms: u64 = test_cases.iter().map(|tc| tc.duration_ms.unwrap_or(0)).sum();

    Ok(ParsedReport {
        framework: "trx".to_string(),
        summary: Summary {
            passed,
            failed: failed + timed_out,
            skipped,
            flaky: 0,
            total,
            duration_ms: Some(duration_ms),
        },
        test_cases,
        metadata: serde_json::json!({
            "runId": run_id,
            "runName": run_name,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_trx_extension_with_testrun() {
        let parser = TrxParser;
        let sample = r#"<?xml version="1.0"?><TestRun id="abc">"#;
        assert_eq!(parser.detect(sample, "report.trx"), 100);
    }

    #[test]
    fn detect_xml_with_trx_namespace() {
        let parser = TrxParser;
        let sample = r#"<TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">"#;
        assert_eq!(parser.detect(sample, "report.xml"), 95);
    }

    #[test]
    fn detect_xml_with_testrun_no_namespace() {
        let parser = TrxParser;
        let sample = r#"<?xml version="1.0"?><TestRun><Results></Results></TestRun>"#;
        assert_eq!(parser.detect(sample, "report.xml"), 70);
    }

    #[test]
    fn detect_zero_for_non_xml() {
        let parser = TrxParser;
        assert_eq!(parser.detect(r#"{"test": "json"}"#, "report.json"), 0);
    }

    #[test]
    fn detect_zero_for_xml_without_testrun() {
        let parser = TrxParser;
        let sample = r#"<?xml version="1.0"?><root><item>test</item></root>"#;
        assert_eq!(parser.detect(sample, "data.xml"), 0);
    }

    // =========================================================================
    // Duration parsing
    // =========================================================================

    #[test]
    fn parse_timespan_standard() {
        assert_eq!(parse_dotnet_timespan("00:00:01.0000000"), 1000);
    }

    #[test]
    fn parse_timespan_fractional() {
        assert_eq!(parse_dotnet_timespan("00:00:00.0036232"), 4);
    }

    #[test]
    fn parse_timespan_with_days() {
        // 1.02:03:04.5 = 1 day + 2h + 3m + 4.5s
        assert_eq!(parse_dotnet_timespan("1.02:03:04.5000000"), 93784500);
    }

    #[test]
    fn parse_timespan_empty() {
        assert_eq!(parse_dotnet_timespan(""), 0);
    }

    #[test]
    fn parse_timespan_zero() {
        assert_eq!(parse_dotnet_timespan("00:00:00.0000000"), 0);
    }

    #[test]
    fn parse_timespan_sub_millisecond() {
        // 0.0000529 seconds = 0.0529 ms, rounds to 0
        assert_eq!(parse_dotnet_timespan("00:00:00.0000529"), 0);
    }

    // =========================================================================
    // Name extraction
    // =========================================================================

    #[test]
    fn extract_simple_dotted_name() {
        let (class, method) = extract_class_and_method("Namespace.Class.Method");
        assert_eq!(class, "Namespace.Class");
        assert_eq!(method, "Method");
    }

    #[test]
    fn extract_parameterized_name() {
        let (class, method) = extract_class_and_method("Ns.Class.Method(a: 1, b: 2)");
        assert_eq!(class, "Ns.Class");
        assert_eq!(method, "Method(a: 1, b: 2)");
    }

    #[test]
    fn extract_mstest_parameterized_with_space() {
        let (class, method) = extract_class_and_method("Ns.Class.Method (1,2,3)");
        assert_eq!(class, "Ns.Class");
        assert_eq!(method, "Method (1,2,3)");
    }

    #[test]
    fn extract_no_dots() {
        let (class, method) = extract_class_and_method("SimpleTest");
        assert_eq!(class, "");
        assert_eq!(method, "SimpleTest");
    }

    // =========================================================================
    // Outcome mapping
    // =========================================================================

    #[test]
    fn outcome_passed() {
        assert_eq!(map_outcome("Passed"), TestStatus::Passed);
    }

    #[test]
    fn outcome_failed() {
        assert_eq!(map_outcome("Failed"), TestStatus::Failed);
    }

    #[test]
    fn outcome_error() {
        assert_eq!(map_outcome("Error"), TestStatus::Failed);
    }

    #[test]
    fn outcome_aborted() {
        assert_eq!(map_outcome("Aborted"), TestStatus::Failed);
    }

    #[test]
    fn outcome_not_executed() {
        assert_eq!(map_outcome("NotExecuted"), TestStatus::Skipped);
    }

    #[test]
    fn outcome_inconclusive() {
        assert_eq!(map_outcome("Inconclusive"), TestStatus::Skipped);
    }

    #[test]
    fn outcome_timeout() {
        assert_eq!(map_outcome("Timeout"), TestStatus::TimedOut);
    }

    #[test]
    fn outcome_not_runnable() {
        assert_eq!(map_outcome("NotRunnable"), TestStatus::Skipped);
    }

    #[test]
    fn outcome_unknown_defaults_to_skipped() {
        assert_eq!(map_outcome("SomethingElse"), TestStatus::Skipped);
    }

    // =========================================================================
    // Minimal parse tests
    // =========================================================================

    #[test]
    fn parse_minimal_trx() {
        let input = r#"<?xml version="1.0"?>
            <TestRun id="run-1" name="Test Run" xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult testName="SimpleTest" outcome="Passed" duration="00:00:00.0010000"/>
                </Results>
            </TestRun>"#;

        let report = parse_trx(input).unwrap();
        assert_eq!(report.framework, "trx");
        assert_eq!(report.test_cases.len(), 1);
        assert_eq!(report.test_cases[0].name, "SimpleTest");
        assert_eq!(report.test_cases[0].status, TestStatus::Passed);
        assert_eq!(report.test_cases[0].duration_ms, Some(1));
    }

    #[test]
    fn parse_error_message_extracted() {
        let input = r#"<?xml version="1.0"?>
            <TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult testName="Ns.Class.FailTest" outcome="Failed" duration="00:00:00.0010000">
                        <Output>
                            <ErrorInfo>
                                <Message>Assert.Equal() Failure</Message>
                            </ErrorInfo>
                        </Output>
                    </UnitTestResult>
                </Results>
            </TestRun>"#;

        let report = parse_trx(input).unwrap();
        assert_eq!(report.test_cases[0].status, TestStatus::Failed);
        assert_eq!(report.test_cases[0].error_message.as_deref(), Some("Assert.Equal() Failure"));
        assert_eq!(report.test_cases[0].full_name, "Ns.Class > FailTest");
    }

    #[test]
    fn parse_metadata_extracted() {
        let input = r#"<?xml version="1.0"?>
            <TestRun id="abc-123" name="My Test Run" xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult testName="Test1" outcome="Passed" duration="00:00:00.001"/>
                </Results>
            </TestRun>"#;

        let report = parse_trx(input).unwrap();
        assert_eq!(report.metadata["runId"], "abc-123");
        assert_eq!(report.metadata["runName"], "My Test Run");
    }

    #[test]
    fn parse_empty_results_returns_error() {
        let input = r#"<?xml version="1.0"?><TestRun><Results></Results></TestRun>"#;
        assert!(parse_trx(input).is_err());
    }

    #[test]
    fn parse_invalid_xml_returns_error() {
        let input = "this is not valid xml at all";
        assert!(parse_trx(input).is_err());
    }

    #[test]
    fn parse_missing_test_name_uses_default() {
        let input = r#"<?xml version="1.0"?>
            <TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult outcome="Passed" duration="00:00:00.001"/>
                </Results>
            </TestRun>"#;

        let report = parse_trx(input).unwrap();
        assert_eq!(report.test_cases[0].name, "Unknown Test");
    }

    #[test]
    fn parse_bom_handled() {
        let input = "\u{FEFF}<?xml version=\"1.0\"?>
            <TestRun xmlns=\"http://microsoft.com/schemas/VisualStudio/TeamTest/2010\">
                <Results>
                    <UnitTestResult testName=\"BomTest\" outcome=\"Passed\" duration=\"00:00:00.001\"/>
                </Results>
            </TestRun>";

        let report = parse_trx(input).unwrap();
        assert_eq!(report.test_cases[0].name, "BomTest");
    }

    #[test]
    fn parse_summary_counts_timed_out_as_failed() {
        let input = r#"<?xml version="1.0"?>
            <TestRun xmlns="http://microsoft.com/schemas/VisualStudio/TeamTest/2010">
                <Results>
                    <UnitTestResult testName="Test1" outcome="Passed" duration="00:00:00.001"/>
                    <UnitTestResult testName="Test2" outcome="Timeout" duration="00:00:30.000"/>
                </Results>
            </TestRun>"#;

        let report = parse_trx(input).unwrap();
        assert_eq!(report.summary.passed, 1);
        assert_eq!(report.summary.failed, 1); // Timeout counted as failed in summary
        assert_eq!(report.summary.total, 2);
    }
}
