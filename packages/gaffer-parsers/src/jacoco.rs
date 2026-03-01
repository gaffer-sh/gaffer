//! JaCoCo XML coverage report parser.
//!
//! JaCoCo is the dominant Java/JVM coverage tool, built into Maven, Gradle, etc.
//!
//! XML structure:
//! ```xml
//! <report name="...">
//!   <sessioninfo id="..." start="..." dump="..."/>
//!   <package name="com/example">
//!     <class name="com/example/Calculator" sourcefilename="Calculator.java">
//!       <counter type="LINE" missed="1" covered="15"/>
//!     </class>
//!     <sourcefile name="Calculator.java">
//!       <counter type="LINE" missed="1" covered="15"/>
//!       <counter type="BRANCH" missed="1" covered="3"/>
//!       <counter type="METHOD" missed="0" covered="5"/>
//!     </sourcefile>
//!   </package>
//!   <counter type="LINE" .../> <!-- report-level counters -->
//! </report>
//! ```

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::registry::Parser;
use crate::types::{
    CoverageReport, CoverageReportSummary, FileCoverage, ParseError, ParseResult, ResultType,
};
use crate::xml_helpers::{calculate_summary_from_files, get_attr, get_int_attr, make_metrics, strip_bom};

pub struct JacocoParser;

impl Parser for JacocoParser {
    fn id(&self) -> &str {
        "jacoco"
    }

    fn name(&self) -> &str {
        "JaCoCo Coverage Report"
    }

    fn priority(&self) -> u8 {
        95
    }

    fn result_type(&self) -> ResultType {
        ResultType::Coverage
    }

    fn detect(&self, sample: &str, filename: &str) -> u8 {
        let lower = filename.to_lowercase();

        if !lower.ends_with(".xml") {
            return 0;
        }

        // Tier 1: filename contains "jacoco"
        if lower.contains("jacoco") {
            return 95;
        }

        // Content-based detection
        let has_report = sample.contains("<report");
        let has_counter_type = sample.contains("<counter type=");
        let has_line_counter = sample.contains("type=\"LINE\"");
        let has_branch_counter = sample.contains("type=\"BRANCH\"");
        let has_sessioninfo = sample.contains("<sessioninfo");
        let has_package = sample.contains("<package");

        if has_report && has_counter_type && (has_line_counter || has_branch_counter) {
            return 95;
        }

        if has_report && has_sessioninfo && has_package {
            return 90;
        }

        if has_report && has_counter_type {
            return 75;
        }

        0
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        parse_jacoco(content).map(ParseResult::Coverage)
    }
}

/// Accumulates counter data for a sourcefile or class.
#[derive(Default)]
struct CounterAccumulator {
    lines_covered: i32,
    lines_total: i32,
    branches_covered: i32,
    branches_total: i32,
    methods_covered: i32,
    methods_total: i32,
}

impl CounterAccumulator {
    fn add_counter(&mut self, counter_type: &str, missed: i32, covered: i32) {
        match counter_type {
            "LINE" => {
                self.lines_covered = covered;
                self.lines_total = missed + covered;
            }
            "BRANCH" => {
                self.branches_covered = covered;
                self.branches_total = missed + covered;
            }
            "METHOD" => {
                self.methods_covered = covered;
                self.methods_total = missed + covered;
            }
            _ => {}
        }
    }

    fn merge_into(&self, other: &mut CounterAccumulator) {
        other.lines_covered += self.lines_covered;
        other.lines_total += self.lines_total;
        other.branches_covered += self.branches_covered;
        other.branches_total += self.branches_total;
        other.methods_covered += self.methods_covered;
        other.methods_total += self.methods_total;
    }
}

#[derive(PartialEq)]
enum JacocoState {
    Root,
    InPackage,
    InSourcefile,
    InClass,
}

fn parse_jacoco(input: &str) -> Result<CoverageReport, ParseError> {
    let input = strip_bom(input);

    if !input.contains("<report") {
        return Err(ParseError::from(
            "No <report> element found".to_string(),
        ));
    }

    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);

    // Per-file accumulators for sourcefiles
    let mut sf_files: Vec<(String, CounterAccumulator)> = Vec::new();
    // Per-file accumulators for class fallback
    let mut class_file_map: HashMap<String, CounterAccumulator> = HashMap::new();
    let mut class_file_order: Vec<String> = Vec::new();

    // Report-level counters
    let mut report_counters = CounterAccumulator::default();
    let mut has_report_counters = false;

    // State
    let mut state = JacocoState::Root;
    let mut current_package_name = String::new();
    let mut current_sf_name = String::new();
    let mut current_sf_counters = CounterAccumulator::default();
    let mut current_class_path = String::new();
    let mut current_class_counters = CounterAccumulator::default();

    // Track depth to distinguish report-level counters from nested ones
    let mut depth: u32 = 0;
    let mut report_depth: u32 = 0;

    loop {
        match reader.read_event() {
            Err(e) => {
                return Err(ParseError::from(format!(
                    "XML parse error at position {}: {}",
                    reader.error_position(),
                    e
                )));
            }
            Ok(Event::Eof) => break,
            Ok(Event::Start(ref e)) => {
                depth += 1;
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"report" => {
                        report_depth = depth;
                    }
                    b"package" => {
                        current_package_name =
                            get_attr(e, b"name").unwrap_or_default();
                        state = JacocoState::InPackage;
                    }
                    b"sourcefile" if state == JacocoState::InPackage => {
                        current_sf_name =
                            get_attr(e, b"name").unwrap_or_default();
                        current_sf_counters = CounterAccumulator::default();
                        state = JacocoState::InSourcefile;
                    }
                    b"class" if state == JacocoState::InPackage => {
                        let class_name = get_attr(e, b"name").unwrap_or_default();
                        let source_filename =
                            get_attr(e, b"sourcefilename").unwrap_or_default();

                        // Build file path from class package + sourcefilename
                        if !source_filename.is_empty() {
                            let last_slash = class_name.rfind('/');
                            let package_path = match last_slash {
                                Some(pos) if pos > 0 => &class_name[..pos],
                                _ => "",
                            };
                            current_class_path = if package_path.is_empty() {
                                source_filename
                            } else {
                                format!("{}/{}", package_path, source_filename)
                            };
                        } else {
                            current_class_path = class_name;
                        }
                        current_class_counters = CounterAccumulator::default();
                        state = JacocoState::InClass;
                    }
                    b"counter" => {
                        handle_counter_start(
                            e,
                            &state,
                            depth,
                            report_depth,
                            &mut current_sf_counters,
                            &mut current_class_counters,
                            &mut report_counters,
                            &mut has_report_counters,
                        );
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = e.name().as_ref().to_vec();
                if tag.as_slice() == b"counter" {
                    handle_counter_start(
                        e,
                        &state,
                        depth + 1, // Treat empty as if it were inside the parent
                        report_depth,
                        &mut current_sf_counters,
                        &mut current_class_counters,
                        &mut report_counters,
                        &mut has_report_counters,
                    );
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"sourcefile" if state == JacocoState::InSourcefile => {
                        let path = if current_package_name.is_empty() {
                            current_sf_name.clone()
                        } else {
                            format!("{}/{}", current_package_name, current_sf_name)
                        };
                        sf_files.push((path, std::mem::take(&mut current_sf_counters)));
                        state = JacocoState::InPackage;
                    }
                    b"class" if state == JacocoState::InClass => {
                        if !current_class_path.is_empty() {
                            let acc = class_file_map
                                .entry(current_class_path.clone())
                                .or_default();
                            current_class_counters.merge_into(acc);
                            if !class_file_order.contains(&current_class_path) {
                                class_file_order.push(current_class_path.clone());
                            }
                        }
                        state = JacocoState::InPackage;
                    }
                    b"package" => {
                        state = JacocoState::Root;
                    }
                    _ => {}
                }
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
    }

    // Prefer sourcefiles over classes
    let files: Vec<FileCoverage> = if !sf_files.is_empty() {
        sf_files
            .iter()
            .map(|(path, acc)| FileCoverage {
                path: path.clone(),
                lines: make_metrics(acc.lines_covered, acc.lines_total),
                branches: make_metrics(acc.branches_covered, acc.branches_total),
                functions: make_metrics(acc.methods_covered, acc.methods_total),
            })
            .collect()
    } else {
        class_file_order
            .iter()
            .filter_map(|path| {
                class_file_map.get(path).map(|acc| FileCoverage {
                    path: path.clone(),
                    lines: make_metrics(acc.lines_covered, acc.lines_total),
                    branches: make_metrics(acc.branches_covered, acc.branches_total),
                    functions: make_metrics(acc.methods_covered, acc.methods_total),
                })
            })
            .collect()
    };

    // Prefer report-level counters for summary
    let summary: CoverageReportSummary = if has_report_counters
        && (report_counters.lines_total > 0 || report_counters.branches_total > 0)
    {
        CoverageReportSummary {
            lines: make_metrics(report_counters.lines_covered, report_counters.lines_total),
            branches: make_metrics(
                report_counters.branches_covered,
                report_counters.branches_total,
            ),
            functions: make_metrics(
                report_counters.methods_covered,
                report_counters.methods_total,
            ),
        }
    } else {
        calculate_summary_from_files(&files)
    };

    Ok(CoverageReport {
        format: "jacoco".to_string(),
        summary,
        files,
    })
}

#[allow(clippy::too_many_arguments)]
fn handle_counter_start(
    e: &quick_xml::events::BytesStart,
    state: &JacocoState,
    depth: u32,
    report_depth: u32,
    sf_counters: &mut CounterAccumulator,
    class_counters: &mut CounterAccumulator,
    report_counters: &mut CounterAccumulator,
    has_report_counters: &mut bool,
) {
    let counter_type = match get_attr(e, b"type") {
        Some(t) => t,
        None => return,
    };
    let missed = get_int_attr(e, b"missed").unwrap_or(0);
    let covered = get_int_attr(e, b"covered").unwrap_or(0);

    match state {
        JacocoState::InSourcefile => {
            sf_counters.add_counter(&counter_type, missed, covered);
        }
        JacocoState::InClass => {
            class_counters.add_counter(&counter_type, missed, covered);
        }
        JacocoState::Root => {
            // Only capture counters that are direct children of <report>
            if depth == report_depth + 1 {
                report_counters.add_counter(&counter_type, missed, covered);
                *has_report_counters = true;
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_jacoco_filename() {
        let parser = JacocoParser;
        assert_eq!(parser.detect("<report><counter type=\"LINE\"/></report>", "jacoco.xml"), 95);
    }

    #[test]
    fn detect_jacoco_in_path() {
        let parser = JacocoParser;
        assert_eq!(
            parser.detect("<report>", "target/site/jacoco/jacoco.xml"),
            95
        );
    }

    #[test]
    fn detect_xml_structure_with_line_counter() {
        let parser = JacocoParser;
        let sample = r#"<report name="test"><counter type="LINE" missed="5" covered="95"/></report>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 95);
    }

    #[test]
    fn detect_session_structure() {
        let parser = JacocoParser;
        let sample =
            r#"<report name="test"><sessioninfo id="s1"/><package name="com"></package></report>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 90);
    }

    #[test]
    fn detect_partial_markers() {
        let parser = JacocoParser;
        let sample = r#"<report name="test"><counter type="INSTRUCTION" missed="10" covered="90"/></report>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 75);
    }

    #[test]
    fn detect_non_jacoco() {
        let parser = JacocoParser;
        assert_eq!(
            parser.detect("<coverage line-rate=\"0.8\"></coverage>", "coverage.xml"),
            0
        );
    }

    #[test]
    fn detect_non_xml() {
        let parser = JacocoParser;
        assert_eq!(parser.detect("{}", "report.json"), 0);
    }

    // =========================================================================
    // Parse tests
    // =========================================================================

    #[test]
    fn parse_minimal_report_level_counters() {
        let input = r#"<?xml version="1.0" encoding="UTF-8"?>
<report name="test-project">
  <sessioninfo id="s1" start="123" dump="456"/>
  <counter type="LINE" missed="15" covered="85"/>
  <counter type="BRANCH" missed="6" covered="14"/>
  <counter type="METHOD" missed="2" covered="8"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.format, "jacoco");
        assert_eq!(result.summary.lines.covered, 85);
        assert_eq!(result.summary.lines.total, 100);
        assert_eq!(result.summary.lines.percentage, 85.0);
        assert_eq!(result.summary.branches.covered, 14);
        assert_eq!(result.summary.branches.total, 20);
        assert_eq!(result.summary.functions.covered, 8);
        assert_eq!(result.summary.functions.total, 10);
    }

    #[test]
    fn parse_sourcefiles() {
        let input = r#"<?xml version="1.0"?>
<report name="test">
  <sessioninfo id="s1" start="123" dump="456"/>
  <package name="com/example">
    <sourcefile name="Calculator.java">
      <counter type="LINE" missed="5" covered="45"/>
      <counter type="BRANCH" missed="2" covered="8"/>
      <counter type="METHOD" missed="1" covered="9"/>
    </sourcefile>
  </package>
  <counter type="LINE" missed="5" covered="45"/>
  <counter type="BRANCH" missed="2" covered="8"/>
  <counter type="METHOD" missed="1" covered="9"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "com/example/Calculator.java");
        assert_eq!(result.files[0].lines.covered, 45);
        assert_eq!(result.files[0].lines.total, 50);
    }

    #[test]
    fn parse_multiple_packages() {
        let input = r#"<?xml version="1.0"?>
<report name="test">
  <sessioninfo id="s1" start="123" dump="456"/>
  <package name="com/example/util">
    <sourcefile name="StringUtils.java">
      <counter type="LINE" missed="2" covered="48"/>
      <counter type="BRANCH" missed="1" covered="9"/>
      <counter type="METHOD" missed="0" covered="10"/>
    </sourcefile>
  </package>
  <package name="com/example/math">
    <sourcefile name="Calculator.java">
      <counter type="LINE" missed="8" covered="42"/>
      <counter type="BRANCH" missed="3" covered="7"/>
      <counter type="METHOD" missed="2" covered="8"/>
    </sourcefile>
  </package>
  <counter type="LINE" missed="10" covered="90"/>
  <counter type="BRANCH" missed="4" covered="16"/>
  <counter type="METHOD" missed="2" covered="18"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "com/example/util/StringUtils.java");
        assert_eq!(result.files[1].path, "com/example/math/Calculator.java");
    }

    #[test]
    fn parse_real_jacoco_output() {
        let input = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<!DOCTYPE report PUBLIC "-//JACOCO//DTD Report 1.1//EN" "report.dtd">
<report name="gaffer-java-example">
    <sessioninfo id="session1" start="1706000000000" dump="1706000001000"/>
    <package name="com/example">
        <class name="com/example/Calculator" sourcefilename="Calculator.java">
            <method name="add" desc="(DD)D" line="10">
                <counter type="INSTRUCTION" missed="0" covered="5"/>
                <counter type="LINE" missed="0" covered="1"/>
                <counter type="METHOD" missed="0" covered="1"/>
            </method>
            <counter type="LINE" missed="1" covered="4"/>
            <counter type="BRANCH" missed="1" covered="1"/>
            <counter type="METHOD" missed="0" covered="2"/>
        </class>
        <sourcefile name="Calculator.java">
            <counter type="LINE" missed="1" covered="4"/>
            <counter type="BRANCH" missed="1" covered="1"/>
            <counter type="METHOD" missed="0" covered="2"/>
        </sourcefile>
    </package>
    <counter type="LINE" missed="1" covered="4"/>
    <counter type="BRANCH" missed="1" covered="1"/>
    <counter type="METHOD" missed="0" covered="2"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.format, "jacoco");
        assert_eq!(result.summary.lines.covered, 4);
        assert_eq!(result.summary.lines.total, 5);
        assert_eq!(result.summary.branches.covered, 1);
        assert_eq!(result.summary.branches.total, 2);
        assert_eq!(result.summary.functions.covered, 2);
        assert_eq!(result.summary.functions.total, 2);
    }

    #[test]
    fn parse_class_fallback() {
        let input = r#"<?xml version="1.0"?>
<report name="test-project">
  <sessioninfo id="s1" start="123" dump="456"/>
  <package name="com/example">
    <class name="com/example/Calculator" sourcefilename="Calculator.java">
      <counter type="LINE" missed="5" covered="15"/>
      <counter type="BRANCH" missed="2" covered="6"/>
      <counter type="METHOD" missed="1" covered="4"/>
    </class>
    <class name="com/example/Utils" sourcefilename="Utils.java">
      <counter type="LINE" missed="3" covered="17"/>
      <counter type="BRANCH" missed="1" covered="3"/>
      <counter type="METHOD" missed="0" covered="5"/>
    </class>
  </package>
  <counter type="LINE" missed="8" covered="32"/>
  <counter type="BRANCH" missed="3" covered="9"/>
  <counter type="METHOD" missed="1" covered="9"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "com/example/Calculator.java");
        assert_eq!(result.files[0].lines.covered, 15);
        assert_eq!(result.files[0].lines.total, 20);
        assert_eq!(result.files[1].path, "com/example/Utils.java");
        assert_eq!(result.files[1].lines.covered, 17);
        assert_eq!(result.files[1].lines.total, 20);
    }

    #[test]
    fn parse_counter_attr_order() {
        let input = r#"<?xml version="1.0"?>
<report name="test">
  <counter covered="85" missed="15" type="LINE"/>
  <counter type="BRANCH" covered="14" missed="6"/>
  <counter missed="2" type="METHOD" covered="8"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.summary.lines.covered, 85);
        assert_eq!(result.summary.lines.total, 100);
        assert_eq!(result.summary.branches.covered, 14);
        assert_eq!(result.summary.branches.total, 20);
        assert_eq!(result.summary.functions.covered, 8);
        assert_eq!(result.summary.functions.total, 10);
    }

    #[test]
    fn parse_zero_coverage() {
        let input = r#"<report name="test">
  <counter type="LINE" missed="100" covered="0"/>
  <counter type="BRANCH" missed="20" covered="0"/>
  <counter type="METHOD" missed="10" covered="0"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 0.0);
        assert_eq!(result.summary.branches.percentage, 0.0);
        assert_eq!(result.summary.functions.percentage, 0.0);
    }

    #[test]
    fn parse_full_coverage() {
        let input = r#"<report name="test">
  <counter type="LINE" missed="0" covered="100"/>
  <counter type="BRANCH" missed="0" covered="20"/>
  <counter type="METHOD" missed="0" covered="10"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 100.0);
        assert_eq!(result.summary.branches.percentage, 100.0);
        assert_eq!(result.summary.functions.percentage, 100.0);
    }

    #[test]
    fn parse_empty_content_returns_error() {
        assert!(parse_jacoco("").is_err());
    }

    #[test]
    fn parse_non_jacoco_xml_returns_error() {
        assert!(parse_jacoco("<coverage><test/></coverage>").is_err());
    }

    #[test]
    fn parse_malformed_xml_graceful() {
        let input = r#"<report name="test">
  <sessioninfo id="s1" start="123" dump="456"/>
  <package name="com/example">
    <sourcefile name="Test.java""#;

        let result = parse_jacoco(input);
        // Should not panic — truncated XML returns partial result (EOF) or error
        if let Ok(report) = &result {
            assert_eq!(report.format, "jacoco");
        }
    }

    #[test]
    fn parse_malformed_attributes_graceful() {
        let input = r#"<report name="test">
  <counter type="LINE" missed="not-a-number" covered="invalid"/>
</report>"#;

        let result = parse_jacoco(input).unwrap();
        assert_eq!(result.format, "jacoco");
        assert_eq!(result.summary.lines.total, 0);
    }
}
