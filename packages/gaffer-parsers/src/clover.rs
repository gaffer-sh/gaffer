//! Clover XML coverage report parser.
//!
//! The Clover XML format lives on through PHPUnit's `--coverage-clover` output
//! and OpenClover (Java).
//!
//! XML structure:
//! ```xml
//! <coverage generated="..." clover="...">
//!   <project timestamp="...">
//!     <metrics statements="..." coveredstatements="..." .../>
//!     <package name="...">
//!       <file name="..." path="/full/path/to/file.php">
//!         <line num="1" type="stmt" count="1"/>
//!         <line num="5" type="cond" truecount="1" falsecount="0"/>
//!         <line num="10" type="method" name="myMethod" count="3"/>
//!         <metrics statements="..." coveredstatements="..." .../>
//!       </file>
//!     </package>
//!   </project>
//! </coverage>
//! ```

use quick_xml::events::Event;
use quick_xml::Reader;

use crate::registry::Parser;
use crate::types::{
    CoverageReport, CoverageReportSummary, FileCoverage, ParseError, ParseResult, ResultType,
};
use crate::xml_helpers::{
    calculate_summary_from_files, get_attr, get_int_attr, make_metrics, strip_bom,
};

pub struct CloverParser;

impl Parser for CloverParser {
    fn id(&self) -> &str {
        "clover"
    }

    fn name(&self) -> &str {
        "Clover Coverage Report"
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

        // Tier 1: filename contains "clover"
        if lower.contains("clover") {
            return 95;
        }

        // Content-based detection
        let has_coverage = sample.contains("<coverage");
        let has_project = sample.contains("<project");
        let has_metrics = sample.contains("<metrics");
        let has_statements = sample.contains("statements=");
        let has_covered_statements = sample.contains("coveredstatements=");
        let has_methods_pair =
            sample.contains("methods=") && sample.contains("coveredmethods=");
        let has_clover_attr = sample.contains("clover=");

        if has_coverage && has_project && has_metrics && (has_covered_statements || has_methods_pair)
        {
            return 95;
        }

        if has_coverage && has_clover_attr && has_project {
            return 90;
        }

        if has_coverage && has_project && has_metrics && has_statements {
            return 80;
        }

        0
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        parse_clover(content).map(ParseResult::Coverage)
    }
}

/// Metrics extracted from a `<metrics>` element.
#[derive(Default)]
struct MetricsData {
    lines_total: i32,
    lines_covered: i32,
    branches_total: i32,
    branches_covered: i32,
    methods_total: i32,
    methods_covered: i32,
}

/// Accumulator for line-element fallback parsing within a file.
#[derive(Default)]
struct LineAccumulator {
    lines_total: i32,
    lines_covered: i32,
    branches_total: i32,
    branches_covered: i32,
    methods_total: i32,
    methods_covered: i32,
}

#[derive(PartialEq)]
enum CloverState {
    Root,
    InProject,
    InPackageOrFile,
    InFile,
}

fn parse_clover(input: &str) -> Result<CoverageReport, ParseError> {
    let input = strip_bom(input);

    if !input.contains("<coverage") {
        return Err(ParseError::from(
            "No <coverage> element found".to_string(),
        ));
    }

    let mut reader = Reader::from_str(input);
    reader.config_mut().trim_text(false);

    let mut files: Vec<FileCoverage> = Vec::new();

    // Project-level metrics
    let mut project_metrics: Option<MetricsData> = None;

    // Current file state
    let mut current_file_path = String::new();
    let mut current_file_has_metrics = false;
    let mut current_line_acc = LineAccumulator::default();

    let mut state = CloverState::Root;
    let mut seen_nested_element = false; // Have we seen <package> or <file> inside <project>?

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
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"project" => {
                        state = CloverState::InProject;
                        seen_nested_element = false;
                    }
                    b"package" if state == CloverState::InProject || state == CloverState::InPackageOrFile => {
                        state = CloverState::InPackageOrFile;
                        seen_nested_element = true;
                    }
                    b"file" => {
                        seen_nested_element = true;
                        // Prefer path attribute, fall back to name
                        current_file_path = get_attr(e, b"path")
                            .or_else(|| get_attr(e, b"name"))
                            .unwrap_or_default();
                        current_file_has_metrics = false;
                        current_line_acc = LineAccumulator::default();
                        state = CloverState::InFile;
                    }
                    b"metrics" if state == CloverState::InFile && !current_file_has_metrics => {
                        // File-level metrics element
                        let m = parse_metrics_element(e);
                        current_file_has_metrics = true;
                        files.push(FileCoverage {
                            path: current_file_path.clone(),
                            lines: make_metrics(m.lines_covered, m.lines_total),
                            branches: make_metrics(m.branches_covered, m.branches_total),
                            functions: make_metrics(m.methods_covered, m.methods_total),
                        });
                    }
                    b"metrics"
                        if (state == CloverState::InProject) && !seen_nested_element =>
                    {
                        // Project-level metrics (before any package/file)
                        let m = parse_metrics_element(e);
                        if m.lines_total > 0 || m.methods_total > 0 {
                            project_metrics = Some(m);
                        }
                    }
                    b"line" if state == CloverState::InFile && !current_file_has_metrics => {
                        handle_line_element(e, &mut current_line_acc);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(ref e)) => {
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"metrics" if state == CloverState::InFile && !current_file_has_metrics => {
                        let m = parse_metrics_element(e);
                        current_file_has_metrics = true;
                        files.push(FileCoverage {
                            path: current_file_path.clone(),
                            lines: make_metrics(m.lines_covered, m.lines_total),
                            branches: make_metrics(m.branches_covered, m.branches_total),
                            functions: make_metrics(m.methods_covered, m.methods_total),
                        });
                    }
                    b"metrics"
                        if (state == CloverState::InProject) && !seen_nested_element =>
                    {
                        let m = parse_metrics_element(e);
                        if m.lines_total > 0 || m.methods_total > 0 {
                            project_metrics = Some(m);
                        }
                    }
                    b"line" if state == CloverState::InFile && !current_file_has_metrics => {
                        handle_line_element(e, &mut current_line_acc);
                    }
                    _ => {}
                }
            }
            Ok(Event::End(ref e)) => {
                let tag = e.name().as_ref().to_vec();
                match tag.as_slice() {
                    b"file" if state == CloverState::InFile => {
                        if !current_file_has_metrics && current_line_acc.lines_total > 0 {
                            // Use line-element fallback
                            files.push(FileCoverage {
                                path: current_file_path.clone(),
                                lines: make_metrics(
                                    current_line_acc.lines_covered,
                                    current_line_acc.lines_total,
                                ),
                                branches: make_metrics(
                                    current_line_acc.branches_covered,
                                    current_line_acc.branches_total,
                                ),
                                functions: make_metrics(
                                    current_line_acc.methods_covered,
                                    current_line_acc.methods_total,
                                ),
                            });
                        }
                        state = CloverState::InPackageOrFile;
                    }
                    b"package" => {
                        state = CloverState::InProject;
                    }
                    b"project" => {
                        state = CloverState::Root;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    // Summary: prefer project-level metrics, fall back to file aggregation
    let summary: CoverageReportSummary = if let Some(ref m) = project_metrics {
        CoverageReportSummary {
            lines: make_metrics(m.lines_covered, m.lines_total),
            branches: make_metrics(m.branches_covered, m.branches_total),
            functions: make_metrics(m.methods_covered, m.methods_total),
        }
    } else {
        calculate_summary_from_files(&files)
    };

    Ok(CoverageReport {
        format: "clover".to_string(),
        summary,
        files,
    })
}

fn parse_metrics_element(e: &quick_xml::events::BytesStart) -> MetricsData {
    let statements = get_int_attr(e, b"statements");
    let covered_statements = get_int_attr(e, b"coveredstatements");
    let conditionals = get_int_attr(e, b"conditionals");
    let covered_conditionals = get_int_attr(e, b"coveredconditionals");
    let methods = get_int_attr(e, b"methods");
    let covered_methods = get_int_attr(e, b"coveredmethods");

    // Alternative attribute names
    let ncloc = get_int_attr(e, b"ncloc");
    let loc = get_int_attr(e, b"loc");

    // Lines: use statements, fall back to ncloc/loc
    let lines_total = statements.or(ncloc).or(loc).unwrap_or(0);

    MetricsData {
        lines_total,
        lines_covered: covered_statements.unwrap_or(0),
        branches_total: conditionals.unwrap_or(0),
        branches_covered: covered_conditionals.unwrap_or(0),
        methods_total: methods.unwrap_or(0),
        methods_covered: covered_methods.unwrap_or(0),
    }
}

fn handle_line_element(e: &quick_xml::events::BytesStart, acc: &mut LineAccumulator) {
    let line_type = get_attr(e, b"type").unwrap_or_default();
    let count = get_int_attr(e, b"count").unwrap_or(0);

    match line_type.as_str() {
        "stmt" => {
            acc.lines_total += 1;
            if count > 0 {
                acc.lines_covered += 1;
            }
        }
        "cond" => {
            // Branches: 2 per conditional (true + false)
            acc.branches_total += 2;
            let true_count = get_int_attr(e, b"truecount").unwrap_or(0);
            let false_count = get_int_attr(e, b"falsecount").unwrap_or(0);
            if true_count > 0 {
                acc.branches_covered += 1;
            }
            if false_count > 0 {
                acc.branches_covered += 1;
            }
            // Also count as a line
            acc.lines_total += 1;
            if count > 0 {
                acc.lines_covered += 1;
            }
        }
        "method" => {
            acc.methods_total += 1;
            if count > 0 {
                acc.methods_covered += 1;
            }
            // Methods also count as lines
            acc.lines_total += 1;
            if count > 0 {
                acc.lines_covered += 1;
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
    fn detect_clover_filename() {
        let parser = CloverParser;
        assert_eq!(
            parser.detect("<coverage><project><metrics/></project></coverage>", "clover.xml"),
            95
        );
    }

    #[test]
    fn detect_clover_in_path() {
        let parser = CloverParser;
        assert_eq!(
            parser.detect("<coverage>", "reports/clover/clover.xml"),
            95
        );
    }

    #[test]
    fn detect_xml_structure_with_covered_statements() {
        let parser = CloverParser;
        let sample = r#"<coverage><project><metrics statements="100" coveredstatements="85"/></project></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 95);
    }

    #[test]
    fn detect_clover_version_marker() {
        let parser = CloverParser;
        let sample = r#"<coverage clover="4.0"><project><metrics/></project></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 90);
    }

    #[test]
    fn detect_partial_structure() {
        let parser = CloverParser;
        let sample = r#"<coverage><project><metrics statements="100"/></project></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 80);
    }

    #[test]
    fn detect_non_clover() {
        let parser = CloverParser;
        assert_eq!(
            parser.detect("<report><counter type=\"LINE\"/></report>", "report.xml"),
            0
        );
    }

    #[test]
    fn detect_cobertura_not_clover() {
        let parser = CloverParser;
        let sample = r#"<coverage line-rate="0.8"><packages></packages></coverage>"#;
        assert_eq!(parser.detect(sample, "coverage.xml"), 0);
    }

    #[test]
    fn detect_non_xml() {
        let parser = CloverParser;
        assert_eq!(parser.detect("<coverage>", "report.json"), 0);
    }

    // =========================================================================
    // Parse tests
    // =========================================================================

    #[test]
    fn parse_minimal_project_metrics() {
        let input = r#"<?xml version="1.0"?>
<coverage generated="123" clover="4.0">
  <project timestamp="123">
    <metrics statements="100" coveredstatements="85" conditionals="20" coveredconditionals="15" methods="10" coveredmethods="8"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.format, "clover");
        assert_eq!(result.summary.lines.covered, 85);
        assert_eq!(result.summary.lines.total, 100);
        assert_eq!(result.summary.lines.percentage, 85.0);
        assert_eq!(result.summary.branches.covered, 15);
        assert_eq!(result.summary.branches.total, 20);
        assert_eq!(result.summary.functions.covered, 8);
        assert_eq!(result.summary.functions.total, 10);
    }

    #[test]
    fn parse_file_level() {
        let input = r#"<?xml version="1.0"?>
<coverage generated="123" clover="4.0">
  <project timestamp="123">
    <package name="src">
      <file name="Calculator.php" path="/src/Calculator.php">
        <metrics statements="50" coveredstatements="45" conditionals="10" coveredconditionals="8" methods="5" coveredmethods="5"/>
      </file>
    </package>
    <metrics statements="50" coveredstatements="45" conditionals="10" coveredconditionals="8" methods="5" coveredmethods="5"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "/src/Calculator.php");
        assert_eq!(result.files[0].lines.covered, 45);
        assert_eq!(result.files[0].lines.total, 50);
    }

    #[test]
    fn parse_multiple_files() {
        let input = r#"<?xml version="1.0"?>
<coverage generated="123" clover="4.0">
  <project timestamp="123">
    <package name="src">
      <file name="Calculator.php" path="/src/Calculator.php">
        <metrics statements="60" coveredstatements="55" conditionals="12" coveredconditionals="10" methods="6" coveredmethods="6"/>
      </file>
      <file name="Utils.php" path="/src/Utils.php">
        <metrics statements="40" coveredstatements="30" conditionals="8" coveredconditionals="6" methods="4" coveredmethods="3"/>
      </file>
    </package>
    <metrics statements="100" coveredstatements="85" conditionals="20" coveredconditionals="16" methods="10" coveredmethods="9"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.files.len(), 2);
        assert_eq!(result.files[0].path, "/src/Calculator.php");
        assert_eq!(result.files[1].path, "/src/Utils.php");
    }

    #[test]
    fn parse_real_phpunit_output() {
        let input = r#"<?xml version="1.0" encoding="UTF-8"?>
<coverage generated="1706000000">
  <project timestamp="1706000000">
    <package name="GafferExample">
      <file name="Calculator.php" path="/home/runner/work/examples/php-example/src/Calculator.php">
        <class name="GafferExample\Calculator" namespace="GafferExample">
          <metrics complexity="6" methods="6" coveredmethods="5" conditionals="4" coveredconditionals="3" statements="15" coveredstatements="13" elements="25" coveredelements="21"/>
        </class>
        <line num="13" type="method" name="add" count="5"/>
        <line num="15" type="stmt" count="5"/>
        <line num="20" type="method" name="subtract" count="3"/>
        <line num="22" type="stmt" count="3"/>
        <line num="27" type="method" name="multiply" count="4"/>
        <line num="29" type="stmt" count="4"/>
        <line num="34" type="method" name="divide" count="2"/>
        <line num="36" type="cond" truecount="2" falsecount="0"/>
        <line num="38" type="stmt" count="0"/>
        <line num="40" type="stmt" count="2"/>
        <line num="45" type="method" name="power" count="3"/>
        <line num="47" type="stmt" count="3"/>
        <line num="52" type="method" name="factorial" count="0"/>
        <line num="54" type="cond" truecount="0" falsecount="0"/>
        <line num="56" type="stmt" count="0"/>
        <metrics loc="70" ncloc="50" classes="1" methods="6" coveredmethods="5" conditionals="4" coveredconditionals="3" statements="15" coveredstatements="13" elements="25" coveredelements="21"/>
      </file>
    </package>
    <metrics files="1" loc="70" ncloc="50" classes="1" methods="6" coveredmethods="5" conditionals="4" coveredconditionals="3" statements="15" coveredstatements="13" elements="25" coveredelements="21"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.format, "clover");
        assert_eq!(result.summary.lines.covered, 13);
        assert_eq!(result.summary.lines.total, 15);
        assert_eq!(result.summary.branches.covered, 3);
        assert_eq!(result.summary.branches.total, 4);
        assert_eq!(result.summary.functions.covered, 5);
        assert_eq!(result.summary.functions.total, 6);
    }

    #[test]
    fn parse_line_element_fallback() {
        let input = r#"<?xml version="1.0"?>
<coverage generated="123" clover="4.0">
  <project timestamp="123">
    <package name="src">
      <file name="Calculator.php" path="/src/Calculator.php">
        <line num="10" type="method" name="add" count="5"/>
        <line num="11" type="stmt" count="5"/>
        <line num="12" type="stmt" count="5"/>
        <line num="20" type="method" name="divide" count="2"/>
        <line num="21" type="cond" truecount="2" falsecount="0" count="2"/>
        <line num="22" type="stmt" count="0"/>
      </file>
    </package>
    <metrics files="1" statements="4" coveredstatements="3" conditionals="2" coveredconditionals="1" methods="2" coveredmethods="2"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.files.len(), 1);
        assert_eq!(result.files[0].path, "/src/Calculator.php");
        // 2 methods + 3 stmts + 1 cond = 6 total lines
        assert_eq!(result.files[0].lines.total, 6);
        // 2 methods + 2 stmts + 1 cond = 5 covered
        assert_eq!(result.files[0].lines.covered, 5);
        // 2 total, 2 covered
        assert_eq!(result.files[0].functions.total, 2);
        assert_eq!(result.files[0].functions.covered, 2);
        // 1 cond = 2 branches total, truecount=2 so 1 covered
        assert_eq!(result.files[0].branches.total, 2);
        assert_eq!(result.files[0].branches.covered, 1);
    }

    #[test]
    fn parse_project_metrics_precedence() {
        let input = r#"<?xml version="1.0"?>
<coverage generated="123" clover="4.0">
  <project timestamp="123">
    <metrics files="2" statements="100" coveredstatements="80" conditionals="20" coveredconditionals="15" methods="10" coveredmethods="8"/>
    <package name="src">
      <file name="File1.php" path="/src/File1.php">
        <metrics statements="50" coveredstatements="40" conditionals="10" coveredconditionals="8" methods="5" coveredmethods="4"/>
      </file>
      <file name="File2.php" path="/src/File2.php">
        <metrics statements="50" coveredstatements="40" conditionals="10" coveredconditionals="7" methods="5" coveredmethods="4"/>
      </file>
    </package>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        // Summary should come from project-level metrics
        assert_eq!(result.summary.lines.total, 100);
        assert_eq!(result.summary.lines.covered, 80);
        assert_eq!(result.summary.branches.total, 20);
        assert_eq!(result.summary.branches.covered, 15);
    }

    #[test]
    fn parse_zero_coverage() {
        let input = r#"<coverage clover="4.0">
  <project>
    <metrics statements="100" coveredstatements="0" conditionals="20" coveredconditionals="0" methods="10" coveredmethods="0"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 0.0);
        assert_eq!(result.summary.branches.percentage, 0.0);
        assert_eq!(result.summary.functions.percentage, 0.0);
    }

    #[test]
    fn parse_full_coverage() {
        let input = r#"<coverage clover="4.0">
  <project>
    <metrics statements="100" coveredstatements="100" conditionals="20" coveredconditionals="20" methods="10" coveredmethods="10"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.summary.lines.percentage, 100.0);
        assert_eq!(result.summary.branches.percentage, 100.0);
        assert_eq!(result.summary.functions.percentage, 100.0);
    }

    #[test]
    fn parse_empty_content_returns_error() {
        assert!(parse_clover("").is_err());
    }

    #[test]
    fn parse_non_clover_xml_returns_error() {
        assert!(parse_clover("<report><test/></report>").is_err());
    }

    #[test]
    fn parse_malformed_xml_graceful() {
        let input = r#"<coverage clover="4.0">
  <project>
    <package name="src">
      <file name="Test.php" path="/src/Test.php""#;

        let result = parse_clover(input);
        // Should not panic — truncated XML returns partial result (EOF) or error
        if let Ok(report) = &result {
            assert_eq!(report.format, "clover");
        }
    }

    #[test]
    fn parse_malformed_attributes_graceful() {
        let input = r#"<coverage clover="4.0">
  <project>
    <metrics statements="not-a-number" coveredstatements="invalid" methods="abc" coveredmethods="xyz"/>
  </project>
</coverage>"#;

        let result = parse_clover(input).unwrap();
        assert_eq!(result.format, "clover");
    }
}
