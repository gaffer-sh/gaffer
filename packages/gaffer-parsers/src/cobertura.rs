//! Cobertura XML coverage report parser.
//!
//! Cobertura is a widely used XML format produced by many tools:
//! - Python coverage.py (via pytest-cov)
//! - Java Cobertura plugin
//! - JavaScript Istanbul (via nyc)
//! - .NET Coverlet
//!
//! XML structure:
//! ```xml
//! <coverage line-rate="0.85" branch-rate="0.75"
//!          lines-valid="100" lines-covered="85"
//!          branches-valid="20" branches-covered="14">
//!   <packages>
//!     <package name="...">
//!       <classes>
//!         <class name="..." filename="src/file.py">
//!           <methods>
//!             <method name="..." hits="1"/>
//!           </methods>
//!           <lines>
//!             <line number="1" hits="1" branch="false"/>
//!             <line number="5" hits="0" branch="true" condition-coverage="50% (1/2)"/>
//!           </lines>
//!         </class>
//!       </classes>
//!     </package>
//!   </packages>
//! </coverage>
//! ```

use std::collections::HashMap;

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::registry::Parser;
use crate::types::{
    CoverageReport, CoverageReportSummary, FileCoverage, ParseError, ParseResult, ResultType,
};
use crate::xml_helpers::{
    calculate_summary_from_files, get_attr, get_float_attr, get_int_attr, make_metrics, strip_bom,
};

pub struct CoberturaParser;

impl Parser for CoberturaParser {
    fn id(&self) -> &str {
        "cobertura"
    }

    fn name(&self) -> &str {
        "Cobertura Coverage Report"
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

        // Tier 1: filename contains "cobertura"
        if lower.contains("cobertura") {
            return 95;
        }

        // Content-based detection
        let has_coverage = sample.contains("<coverage");
        let has_line_rate = sample.contains("line-rate=");
        let has_branch_rate = sample.contains("branch-rate=");
        let has_packages = sample.contains("<packages");
        let has_classes = sample.contains("<classes");

        if has_coverage && has_line_rate && has_packages {
            return 95;
        }

        if has_coverage && has_line_rate && (has_branch_rate || has_classes) {
            return 85;
        }

        if has_coverage && has_line_rate {
            return 70;
        }

        0
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        parse_cobertura(content).map(ParseResult::Coverage)
    }
}

/// Accumulator for a single file while parsing.
#[derive(Default)]
struct FileAccumulator {
    lines_total: i32,
    lines_covered: i32,
    branches_total: i32,
    branches_covered: i32,
    methods_total: i32,
    methods_covered: i32,
}

fn parse_cobertura(input: &str) -> Result<CoverageReport, ParseError> {
    let input = strip_bom(input);

    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);

    // Root-level summary attributes
    let mut root_lines_valid: Option<i32> = None;
    let mut root_lines_covered: Option<i32> = None;
    let mut root_branches_valid: Option<i32> = None;
    let mut root_branches_covered: Option<i32> = None;

    // File accumulation
    let mut file_map: HashMap<String, FileAccumulator> = HashMap::new();
    let mut file_order: Vec<String> = Vec::new();

    // State machine
    let mut current_filename: Option<String> = None;
    let mut in_methods = false;
    let mut found_coverage_root = false;

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
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                let tag = e.name().as_ref().to_vec();

                match tag.as_slice() {
                    b"coverage" => {
                        found_coverage_root = true;
                        root_lines_valid = get_int_attr(e, b"lines-valid");
                        root_lines_covered = get_int_attr(e, b"lines-covered");
                        root_branches_valid = get_int_attr(e, b"branches-valid");
                        root_branches_covered = get_int_attr(e, b"branches-covered");
                    }
                    b"class" => {
                        current_filename = get_attr(e, b"filename");
                    }
                    b"methods" => {
                        in_methods = true;
                    }
                    b"method" if in_methods => {
                        if let Some(ref fname) = current_filename {
                            let acc = file_map.entry(fname.clone()).or_default();
                            acc.methods_total += 1;

                            // Covered if hits > 0 or line-rate > 0
                            let hits = get_int_attr(e, b"hits");
                            let line_rate = get_float_attr(e, b"line-rate");
                            if hits.unwrap_or(0) > 0
                                || line_rate.unwrap_or(0.0) > 0.0
                            {
                                acc.methods_covered += 1;
                            }
                        }
                    }
                    b"line" if !in_methods => {
                        if let Some(ref fname) = current_filename {
                            if !file_order.contains(fname) {
                                file_order.push(fname.clone());
                            }
                            let acc = file_map.entry(fname.clone()).or_default();
                            acc.lines_total += 1;

                            let hits = get_int_attr(e, b"hits").unwrap_or(0);
                            if hits > 0 {
                                acc.lines_covered += 1;
                            }

                            // Branch coverage from condition-coverage attribute
                            let is_branch = get_attr(e, b"branch")
                                .map(|v| v == "true")
                                .unwrap_or(false);
                            if is_branch {
                                if let Some(cond) = get_attr(e, b"condition-coverage") {
                                    // Format: "X% (N/M)"
                                    if let Some(paren_start) = cond.find('(') {
                                        if let Some(paren_end) = cond.find(')') {
                                            let inner = &cond[paren_start + 1..paren_end];
                                            if let Some((n_str, m_str)) = inner.split_once('/') {
                                                let n: i32 = n_str.parse().unwrap_or(0);
                                                let m: i32 = m_str.parse().unwrap_or(0);
                                                acc.branches_covered += n;
                                                acc.branches_total += m;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                match e.name().as_ref() {
                    b"class" => {
                        current_filename = None;
                    }
                    b"methods" => {
                        in_methods = false;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    if !found_coverage_root {
        return Err(ParseError::from(
            "No <coverage> root element found".to_string(),
        ));
    }

    // Build file entries in insertion order
    let files: Vec<FileCoverage> = file_order
        .iter()
        .filter_map(|path| {
            file_map.get(path).map(|acc| FileCoverage {
                path: path.clone(),
                lines: make_metrics(acc.lines_covered, acc.lines_total),
                branches: make_metrics(acc.branches_covered, acc.branches_total),
                functions: make_metrics(acc.methods_covered, acc.methods_total),
            })
        })
        .collect();

    // Prefer root attributes for summary, fall back to file aggregation
    let summary: CoverageReportSummary =
        if let (Some(lv), Some(lc)) = (root_lines_valid, root_lines_covered) {
            CoverageReportSummary {
                lines: make_metrics(lc, lv),
                branches: make_metrics(
                    root_branches_covered.unwrap_or(0),
                    root_branches_valid.unwrap_or(0),
                ),
                functions: make_metrics(0, 0),
            }
        } else {
            calculate_summary_from_files(&files)
        };

    Ok(CoverageReport {
        format: "cobertura".to_string(),
        summary,
        files,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // =========================================================================
    // Detection tests
    // =========================================================================

    #[test]
    fn detect_cobertura_filename() {
        let parser = CoberturaParser;
        assert_eq!(parser.detect("<coverage line-rate=\"0.8\">", "cobertura.xml"), 95);
    }

    #[test]
    fn detect_cobertura_in_path() {
        let parser = CoberturaParser;
        assert_eq!(
            parser.detect("<coverage line-rate=\"0.8\">", "reports/cobertura-coverage.xml"),
            95
        );
    }

    #[test]
    fn detect_xml_structure_with_packages() {
        let parser = CoberturaParser;
        let sample = r#"<coverage line-rate="0.85"><packages><package></package></packages></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 95);
    }

    #[test]
    fn detect_partial_structure() {
        let parser = CoberturaParser;
        let sample = r#"<coverage line-rate="0.85" branch-rate="0.7"><classes></classes></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 85);
    }

    #[test]
    fn detect_minimal_markers() {
        let parser = CoberturaParser;
        let sample = r#"<coverage line-rate="0.8"></coverage>"#;
        assert_eq!(parser.detect(sample, "report.xml"), 70);
    }

    #[test]
    fn detect_non_cobertura() {
        let parser = CoberturaParser;
        let sample = r#"<report><counter type="LINE"/></report>"#;
        assert_eq!(parser.detect(sample, "report.xml"), 0);
    }

    #[test]
    fn detect_non_xml_extension() {
        let parser = CoberturaParser;
        assert_eq!(parser.detect("<coverage>", "report.json"), 0);
    }

    // =========================================================================
    // Parse tests
    // =========================================================================

    #[test]
    fn parse_minimal_with_summary_attrs() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="0.85" branch-rate="0.7"
          lines-valid="100" lines-covered="85"
          branches-valid="20" branches-covered="14">
  <packages><package name="src"><classes></classes></package></packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.format, "cobertura");
        assert_eq!(result.summary.lines.covered, 85);
        assert_eq!(result.summary.lines.total, 100);
        assert_eq!(result.summary.lines.percentage, 85.0);
        assert_eq!(result.summary.branches.covered, 14);
        assert_eq!(result.summary.branches.total, 20);
    }

    #[test]
    fn parse_file_level_coverage() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="0.9" branch-rate="0.8">
  <packages>
    <package name="src">
      <classes>
        <class name="calculator.py" filename="src/calculator.py" line-rate="0.9">
          <methods>
            <method name="add" hits="5" line-rate="1.0"/>
          </methods>
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="1" branch="false"/>
            <line number="3" hits="1" branch="true" condition-coverage="100% (2/2)"/>
            <line number="4" hits="0" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "src/calculator.py");
        assert_eq!(result.files[0].lines.total, 4);
        assert_eq!(result.files[0].lines.covered, 3);
        assert_eq!(result.files[0].branches.total, 2);
        assert_eq!(result.files[0].branches.covered, 2);
        assert_eq!(result.files[0].functions.total, 1);
        assert_eq!(result.files[0].functions.covered, 1);
    }

    #[test]
    fn parse_multiple_files() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="0.85" branch-rate="0.7"
          lines-valid="30" lines-covered="25">
  <packages>
    <package name="src">
      <classes>
        <class name="file1" filename="src/file1.py" line-rate="0.9">
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="1" branch="false"/>
          </lines>
        </class>
        <class name="file2" filename="src/file2.py" line-rate="0.8">
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="0" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "src/file1.py");
        assert_eq!(result.files[1].path, "src/file2.py");
    }

    #[test]
    fn parse_real_python_coverage_output() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="7.4.1" timestamp="1706000000000" lines-valid="150" lines-covered="135" line-rate="0.9" branches-covered="28" branches-valid="35" branch-rate="0.8" complexity="0">
    <packages>
        <package name="src" line-rate="0.9" branch-rate="0.8" complexity="0">
            <classes>
                <class name="calculator.py" filename="src/calculator.py" line-rate="0.95" branch-rate="0.9" complexity="0">
                    <methods>
                        <method name="add" signature="(a, b)" line-rate="1.0" branch-rate="1.0" complexity="0">
                            <lines>
                                <line number="5" hits="10"/>
                                <line number="6" hits="10"/>
                            </lines>
                        </method>
                    </methods>
                    <lines>
                        <line number="1" hits="1" branch="false"/>
                        <line number="5" hits="10" branch="false"/>
                        <line number="6" hits="10" branch="false"/>
                        <line number="10" hits="5" branch="true" condition-coverage="100% (2/2)"/>
                        <line number="15" hits="0" branch="false"/>
                    </lines>
                </class>
            </classes>
        </package>
    </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.format, "cobertura");
        assert_eq!(result.summary.lines.covered, 135);
        assert_eq!(result.summary.lines.total, 150);
        assert_eq!(result.summary.branches.covered, 28);
        assert_eq!(result.summary.branches.total, 35);
    }

    #[test]
    fn parse_zero_coverage() {
        let input = r#"<coverage line-rate="0" branch-rate="0"
                        lines-valid="100" lines-covered="0"
                        branches-valid="20" branches-covered="0">
  <packages><package name="src"><classes></classes></package></packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 0.0);
        assert_eq!(result.summary.branches.percentage, 0.0);
    }

    #[test]
    fn parse_full_coverage() {
        let input = r#"<coverage line-rate="1.0" branch-rate="1.0"
                        lines-valid="100" lines-covered="100"
                        branches-valid="20" branches-covered="20">
  <packages><package name="src"><classes></classes></package></packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 100.0);
        assert_eq!(result.summary.branches.percentage, 100.0);
    }

    #[test]
    fn parse_empty_content_returns_error() {
        assert!(parse_cobertura("").is_err());
    }

    #[test]
    fn parse_non_cobertura_xml_returns_error() {
        assert!(parse_cobertura("<report><test/></report>").is_err());
    }

    #[test]
    fn parse_multiple_classes_same_file() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="0.8" branch-rate="0.5">
  <packages>
    <package name="src">
      <classes>
        <class name="Calculator" filename="src/calculator.py">
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="1" branch="false"/>
            <line number="3" hits="0" branch="false"/>
          </lines>
        </class>
        <class name="Calculator.Inner" filename="src/calculator.py">
          <lines>
            <line number="10" hits="1" branch="false"/>
            <line number="11" hits="0" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "src/calculator.py");
        assert_eq!(result.files[0].lines.total, 5);
        assert_eq!(result.files[0].lines.covered, 3);
    }

    #[test]
    fn parse_malformed_xml_graceful() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="0.8" branch-rate="0.5">
  <packages>
    <package name="src">
      <classes>
        <class name="Test" filename="test.py">
          <lines>
            <line number="1" hits="1""#;

        let result = parse_cobertura(input);
        // Should not panic — truncated XML returns partial result (EOF) or error
        if let Ok(report) = &result {
            assert_eq!(report.format, "cobertura");
        }
    }

    #[test]
    fn parse_malformed_attributes_graceful() {
        let input = r#"<?xml version="1.0" ?>
<coverage version="1.0" line-rate="not-a-number" branch-rate="invalid">
  <packages>
    <package name="src">
      <classes>
        <class name="Test" filename="test.py">
          <lines>
            <line number="1" hits="nan" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.format, "cobertura");
    }

    #[test]
    fn parse_consecutive_empty_line_elements() {
        // Regression test: ensures SAX loop doesn't skip events
        let input = r#"<coverage line-rate="0.5" branch-rate="0">
  <packages>
    <package name="src">
      <classes>
        <class name="Test" filename="test.py">
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="1" branch="false"/>
            <line number="3" hits="0" branch="false"/>
            <line number="4" hits="0" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        assert_eq!(result.files[0].lines.total, 4);
        assert_eq!(result.files[0].lines.covered, 2);
    }

    #[test]
    fn parse_summary_falls_back_to_files_when_no_root_attrs() {
        let input = r#"<coverage line-rate="0.8" branch-rate="0.5">
  <packages>
    <package name="src">
      <classes>
        <class name="Test" filename="test.py">
          <lines>
            <line number="1" hits="1" branch="false"/>
            <line number="2" hits="0" branch="false"/>
          </lines>
        </class>
      </classes>
    </package>
  </packages>
</coverage>"#;

        let result = parse_cobertura(input).unwrap();
        // No lines-valid/lines-covered attrs → should use file aggregation
        assert_eq!(result.summary.lines.total, 2);
        assert_eq!(result.summary.lines.covered, 1);
        assert_eq!(result.summary.lines.percentage, 50.0);
    }
}
