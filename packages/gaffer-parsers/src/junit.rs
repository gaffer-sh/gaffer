use quick_xml::events::Event;
use quick_xml::Reader;

use crate::registry::Parser;
use crate::types::{ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus};
use crate::xml_helpers::{get_attr, strip_bom};

pub struct JUnitParser;

impl Parser for JUnitParser {
    fn id(&self) -> &str {
        "junit"
    }

    fn name(&self) -> &str {
        "JUnit XML Report"
    }

    fn priority(&self) -> u8 {
        90
    }

    fn result_type(&self) -> ResultType {
        ResultType::TestReport
    }

    fn detect(&self, sample: &str, filename: &str) -> u8 {
        if !filename.ends_with(".xml") {
            return 0;
        }
        let has_testsuites = sample.contains("<testsuites");
        let has_testsuite = sample.contains("<testsuite");
        if has_testsuites || has_testsuite {
            100
        } else {
            0
        }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        parse_junit(content)
            .map(ParseResult::TestReport)
            .map_err(ParseError::from)
    }
}

/// Context tracked while inside a `<testsuite>` element.
struct SuiteContext {
    name: String,
    file: Option<String>,
}

/// Context accumulated while inside a `<testcase>` element.
struct TestcaseContext {
    name: String,
    classname: Option<String>,
    time: Option<f64>,
    file: Option<String>,
    line: Option<u64>,
    has_skipped: bool,
    has_error: bool,
    error_message: Option<String>,
    has_failure: bool,
    failure_message: Option<String>,
    text_buf: String,
}

#[derive(PartialEq)]
enum InsideElement {
    None,
    Failure,
    Error,
}

fn finalize_testcase(ctx: TestcaseContext, id: usize, suite_file: Option<&str>) -> TestCase {
    let name = ctx.name;
    let classname = ctx.classname;

    let full_name = match &classname {
        Some(cn) if cn != &name => format!("{} > {}", cn, name),
        _ => name.clone(),
    };

    // Status priority (matching JS): skipped > error > failure > passed
    let (status, error_message) = if ctx.has_skipped {
        (TestStatus::Skipped, None)
    } else if ctx.has_error {
        let msg = ctx
            .error_message
            .unwrap_or_else(|| "Test error".to_string());
        (TestStatus::Failed, Some(msg))
    } else if ctx.has_failure {
        let msg = ctx
            .failure_message
            .unwrap_or_else(|| "Test failed".to_string());
        (TestStatus::Failed, Some(msg))
    } else {
        (TestStatus::Passed, None)
    };

    let duration_ms = {
        let time = ctx.time.unwrap_or(0.0);
        let ms = time * 1000.0;
        if ms.is_finite() {
            Some(ms.round() as u64)
        } else {
            None
        }
    };

    let file_path = ctx.file.or_else(|| suite_file.map(|s| s.to_string()));

    TestCase {
        id: format!("tc-{}", id),
        name,
        full_name,
        status,
        duration_ms,
        error_message,
        file_path,
        line: ctx.line,
        retry_attempt: None,
    }
}

pub fn parse_junit(input: &str) -> Result<ParsedReport, String> {
    let input = strip_bom(input);

    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);

    let mut test_cases: Vec<TestCase> = Vec::new();
    let mut id_counter: usize = 0;

    let mut suite_stack: Vec<SuiteContext> = Vec::new();
    let mut suite_depth: usize = 0;

    let mut testsuites_name: Option<String> = None;
    let mut has_testsuites_root = false;
    let mut top_level_suite_count: usize = 0;

    let mut current_testcase: Option<TestcaseContext> = None;
    let mut inside_element = InsideElement::None;

    let mut top_level_suite_times: Vec<f64> = Vec::new();

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
                let local = e.name().as_ref().to_vec();
                match local.as_slice() {
                    b"testsuites" => {
                        has_testsuites_root = true;
                        testsuites_name = get_attr(e, b"name");
                    }
                    b"testsuite" => {
                        suite_depth += 1;

                        if suite_depth == 1 {
                            top_level_suite_count += 1;

                            if let Some(t) = get_attr(e, b"time")
                                .and_then(|s| s.parse::<f64>().ok())
                            {
                                top_level_suite_times.push(t);
                            }
                        }

                        suite_stack.push(SuiteContext {
                            name: get_attr(e, b"name")
                                .unwrap_or_else(|| "Unknown Suite".to_string()),
                            file: get_attr(e, b"file"),
                        });
                    }
                    b"testcase" => {
                        let suite_name = suite_stack
                            .last()
                            .map(|s| s.name.clone())
                            .unwrap_or_else(|| "Unknown Suite".to_string());

                        current_testcase = Some(TestcaseContext {
                            name: get_attr(e, b"name")
                                .unwrap_or_else(|| "Unknown Test".to_string()),
                            classname: get_attr(e, b"classname").or(Some(suite_name)),
                            time: get_attr(e, b"time")
                                .and_then(|s| s.parse::<f64>().ok()),
                            file: get_attr(e, b"file"),
                            line: get_attr(e, b"line")
                                .and_then(|s| s.parse::<u64>().ok()),
                            has_skipped: false,
                            has_error: false,
                            error_message: None,
                            has_failure: false,
                            failure_message: None,
                            text_buf: String::new(),
                        });
                    }
                    b"failure" => {
                        if let Some(ref mut tc) = current_testcase {
                            if !tc.has_failure {
                                tc.has_failure = true;
                                tc.failure_message = get_attr(e, b"message");
                                tc.text_buf.clear();
                                inside_element = InsideElement::Failure;
                            }
                        }
                    }
                    b"error" => {
                        if let Some(ref mut tc) = current_testcase {
                            if !tc.has_error {
                                tc.has_error = true;
                                tc.error_message = get_attr(e, b"message");
                                tc.text_buf.clear();
                                inside_element = InsideElement::Error;
                            }
                        }
                    }
                    b"skipped" => {
                        if let Some(ref mut tc) = current_testcase {
                            tc.has_skipped = true;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let local = e.name().as_ref().to_vec();
                match local.as_slice() {
                    b"testsuite" => {
                        // Self-closing <testsuite/> — count it but nothing to push/pop
                        if !has_testsuites_root || suite_depth == 0 {
                            top_level_suite_count += 1;
                        }
                    }
                    b"testcase" => {
                        let suite_name = suite_stack
                            .last()
                            .map(|s| s.name.clone())
                            .unwrap_or_else(|| "Unknown Suite".to_string());
                        let suite_file =
                            suite_stack.last().and_then(|s| s.file.as_deref());

                        let ctx = TestcaseContext {
                            name: get_attr(e, b"name")
                                .unwrap_or_else(|| "Unknown Test".to_string()),
                            classname: get_attr(e, b"classname").or(Some(suite_name)),
                            time: get_attr(e, b"time")
                                .and_then(|s| s.parse::<f64>().ok()),
                            file: get_attr(e, b"file"),
                            line: get_attr(e, b"line")
                                .and_then(|s| s.parse::<u64>().ok()),
                            has_skipped: false,
                            has_error: false,
                            error_message: None,
                            has_failure: false,
                            failure_message: None,
                            text_buf: String::new(),
                        };

                        id_counter += 1;
                        test_cases.push(finalize_testcase(ctx, id_counter, suite_file));
                    }
                    b"skipped" => {
                        if let Some(ref mut tc) = current_testcase {
                            tc.has_skipped = true;
                        }
                    }
                    b"failure" => {
                        if let Some(ref mut tc) = current_testcase {
                            if !tc.has_failure {
                                tc.has_failure = true;
                                tc.failure_message = get_attr(e, b"message");
                            }
                        }
                    }
                    b"error" => {
                        if let Some(ref mut tc) = current_testcase {
                            if !tc.has_error {
                                tc.has_error = true;
                                tc.error_message = get_attr(e, b"message");
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let local = e.name().as_ref().to_vec();
                match local.as_slice() {
                    b"testsuites" => {}
                    b"testsuite" => {
                        suite_stack.pop();
                        suite_depth = suite_depth.saturating_sub(1);
                    }
                    b"testcase" => {
                        if let Some(ctx) = current_testcase.take() {
                            let suite_file =
                                suite_stack.last().and_then(|s| s.file.as_deref());
                            id_counter += 1;
                            test_cases.push(finalize_testcase(ctx, id_counter, suite_file));
                        }
                        inside_element = InsideElement::None;
                    }
                    b"failure" => {
                        // If no @message was found, fall back to text content
                        if inside_element == InsideElement::Failure {
                            if let Some(ref mut tc) = current_testcase {
                                if tc.failure_message.is_none() {
                                    let trimmed = tc.text_buf.trim().to_string();
                                    if !trimmed.is_empty() {
                                        tc.failure_message = Some(trimmed);
                                    }
                                }
                            }
                            inside_element = InsideElement::None;
                        }
                    }
                    b"error" => {
                        if inside_element == InsideElement::Error {
                            if let Some(ref mut tc) = current_testcase {
                                if tc.error_message.is_none() {
                                    let trimmed = tc.text_buf.trim().to_string();
                                    if !trimmed.is_empty() {
                                        tc.error_message = Some(trimmed);
                                    }
                                }
                            }
                            inside_element = InsideElement::None;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Text(ref e)) => {
                if inside_element != InsideElement::None {
                    if let Some(ref mut tc) = current_testcase {
                        if let Ok(text) = e.unescape() {
                            tc.text_buf.push_str(&text);
                        }
                    }
                }
            }
            Ok(Event::CData(ref e)) => {
                if inside_element != InsideElement::None {
                    if let Some(ref mut tc) = current_testcase {
                        if let Ok(text) = std::str::from_utf8(e.as_ref()) {
                            tc.text_buf.push_str(text);
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if test_cases.is_empty() {
        return Err("No test cases found in XML".to_string());
    }

    // Calculate summary
    let passed = test_cases.iter().filter(|tc| tc.status == TestStatus::Passed).count();
    let failed = test_cases.iter().filter(|tc| tc.status == TestStatus::Failed).count();
    let skipped = test_cases.iter().filter(|tc| tc.status == TestStatus::Skipped).count();
    let total = test_cases.len();

    // Duration: prefer sum of top-level suite times, fallback to sum of testcase times
    let suite_duration_ms: u64 = top_level_suite_times
        .iter()
        .map(|t| (t * 1000.0).round() as u64)
        .sum();

    let duration_ms = if suite_duration_ms > 0 {
        suite_duration_ms
    } else {
        test_cases.iter().map(|tc| tc.duration_ms.unwrap_or(0)).sum()
    };

    Ok(ParsedReport {
        framework: "junit".to_string(),
        summary: Summary {
            passed,
            failed,
            skipped,
            flaky: 0,
            total,
            duration_ms: Some(duration_ms),
        },
        test_cases,
        metadata: serde_json::json!({
            "suiteName": testsuites_name,
            "suiteCount": top_level_suite_count,
        }),
    })
}
