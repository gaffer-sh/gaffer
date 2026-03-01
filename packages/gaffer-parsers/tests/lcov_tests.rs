use gaffer_parsers::{LcovParser, Parser, ParserRegistry, ParseResult, ResultType};

const SAMPLE_LCOV: &str = "\
TN:
SF:/src/auth.ts
FN:1,login
FN:10,logout
FNDA:5,login
FNDA:0,logout
FNF:2
FNH:1
DA:1,5
DA:2,5
DA:3,5
DA:10,0
DA:11,0
LF:5
LH:3
BRF:4
BRH:2
end_of_record
SF:/src/utils.ts
FNF:3
FNH:3
LF:10
LH:10
BRF:2
BRH:2
end_of_record
";

fn parse_lcov(content: &str) -> gaffer_parsers::CoverageReport {
    let parser = LcovParser;
    match parser.parse(content, "coverage.lcov").unwrap() {
        ParseResult::Coverage(report) => report,
        _ => panic!("Expected Coverage result"),
    }
}

// ============================================================================
// Detection tests
// ============================================================================

#[test]
fn detect_lcov_extension_score_100() {
    let parser = LcovParser;
    assert_eq!(parser.detect("", "coverage.lcov"), 100);
}

#[test]
fn detect_lcov_info_filename_score_100() {
    let parser = LcovParser;
    assert_eq!(parser.detect("", "lcov.info"), 100);
}

#[test]
fn detect_lcov_info_with_path_score_100() {
    let parser = LcovParser;
    assert_eq!(parser.detect("", "coverage/lcov.info"), 100);
}

#[test]
fn detect_content_with_all_markers_score_95() {
    let parser = LcovParser;
    let content = "SF:/src/file.ts\nDA:1,5\nLF:1\nLH:1\nend_of_record\n";
    assert_eq!(parser.detect(content, "report.txt"), 95);
}

#[test]
fn detect_content_sf_and_end_of_record_only_score_80() {
    let parser = LcovParser;
    let content = "SF:/src/file.ts\nend_of_record\n";
    assert_eq!(parser.detect(content, "report.txt"), 80);
}

#[test]
fn detect_unrelated_content_score_0() {
    let parser = LcovParser;
    assert_eq!(parser.detect("some random text", "report.txt"), 0);
    assert_eq!(parser.detect("{}", "report.json"), 0);
}

#[test]
fn detect_result_type_is_coverage() {
    let parser = LcovParser;
    assert_eq!(parser.result_type(), ResultType::Coverage);
}

#[test]
fn detect_id_is_lcov() {
    let parser = LcovParser;
    assert_eq!(parser.id(), "lcov");
}

// ============================================================================
// Parse correctness
// ============================================================================

#[test]
fn parse_summary_aggregation() {
    let report = parse_lcov(SAMPLE_LCOV);
    assert_eq!(report.summary.lines.covered, 13); // 3 + 10
    assert_eq!(report.summary.lines.total, 15); // 5 + 10
    assert_eq!(report.summary.branches.covered, 4); // 2 + 2
    assert_eq!(report.summary.branches.total, 6); // 4 + 2
    assert_eq!(report.summary.functions.covered, 4); // 1 + 3
    assert_eq!(report.summary.functions.total, 5); // 2 + 3
    assert_eq!(report.format, "lcov");
}

#[test]
fn parse_summary_percentages() {
    let report = parse_lcov(SAMPLE_LCOV);
    assert_eq!(report.summary.lines.percentage, 87.0); // round(13/15*100)
    assert_eq!(report.summary.branches.percentage, 67.0); // round(4/6*100)
    assert_eq!(report.summary.functions.percentage, 80.0); // round(4/5*100)
}

#[test]
fn parse_per_file_data() {
    let report = parse_lcov(SAMPLE_LCOV);
    assert_eq!(report.files.len(), 2);

    assert_eq!(report.files[0].path, "/src/auth.ts");
    assert_eq!(report.files[0].lines.covered, 3);
    assert_eq!(report.files[0].lines.total, 5);
    assert_eq!(report.files[0].lines.percentage, 60.0);
    assert_eq!(report.files[0].branches.covered, 2);
    assert_eq!(report.files[0].branches.total, 4);
    assert_eq!(report.files[0].functions.covered, 1);
    assert_eq!(report.files[0].functions.total, 2);

    assert_eq!(report.files[1].path, "/src/utils.ts");
    assert_eq!(report.files[1].lines.covered, 10);
    assert_eq!(report.files[1].lines.total, 10);
    assert_eq!(report.files[1].lines.percentage, 100.0);
}

#[test]
fn parse_single_file() {
    let lcov = "\
SF:src/index.ts
FNF:1
FNH:1
LF:20
LH:18
BRF:6
BRH:5
end_of_record
";
    let report = parse_lcov(lcov);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.summary.lines.covered, 18);
    assert_eq!(report.summary.lines.total, 20);
    assert_eq!(report.summary.lines.percentage, 90.0);
    assert_eq!(report.summary.branches.covered, 5);
    assert_eq!(report.summary.branches.total, 6);
    assert_eq!(report.summary.functions.covered, 1);
    assert_eq!(report.summary.functions.total, 1);
}

// ============================================================================
// Edge cases
// ============================================================================

#[test]
fn parse_unterminated_section() {
    let lcov = "\
SF:/src/file.ts
LF:5
LH:3
";
    let report = parse_lcov(lcov);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.summary.lines.covered, 3);
    assert_eq!(report.summary.lines.total, 5);
}

#[test]
fn parse_missing_branches() {
    let lcov = "\
SF:/src/simple.ts
LF:10
LH:8
end_of_record
";
    let report = parse_lcov(lcov);
    assert_eq!(report.summary.lines.covered, 8);
    assert_eq!(report.summary.lines.total, 10);
    assert_eq!(report.summary.branches.covered, 0);
    assert_eq!(report.summary.branches.total, 0);
    assert_eq!(report.summary.branches.percentage, 0.0);
    assert_eq!(report.summary.functions.covered, 0);
    assert_eq!(report.summary.functions.total, 0);
}

#[test]
fn parse_empty_input_returns_error() {
    let parser = LcovParser;
    assert!(parser.parse("", "coverage.lcov").is_err());
}

#[test]
fn parse_random_text_returns_error() {
    let parser = LcovParser;
    assert!(parser.parse("some random text\n", "coverage.lcov").is_err());
}

#[test]
fn parse_empty_sf_path_skipped() {
    let lcov = "\
SF:
LF:5
LH:3
end_of_record
SF:/src/real.ts
LF:10
LH:8
end_of_record
";
    let report = parse_lcov(lcov);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "/src/real.ts");
    assert_eq!(report.summary.lines.covered, 8);
    assert_eq!(report.summary.lines.total, 10);
}

#[test]
fn parse_crlf_line_endings() {
    let lcov = "SF:/src/file.ts\r\nLF:5\r\nLH:3\r\nend_of_record\r\n";
    let report = parse_lcov(lcov);
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.summary.lines.covered, 3);
    assert_eq!(report.summary.lines.total, 5);
}

#[test]
fn parse_percentage_rounds_correctly() {
    let lcov = "\
SF:/src/file.ts
LF:3
LH:1
end_of_record
";
    let report = parse_lcov(lcov);
    // 1/3 * 100 = 33.333... rounds to 33.0
    assert_eq!(report.summary.lines.percentage, 33.0);
}

#[test]
fn parse_zero_total_percentage_is_zero() {
    let lcov = "\
SF:/src/file.ts
LF:0
LH:0
end_of_record
";
    let report = parse_lcov(lcov);
    assert_eq!(report.summary.lines.percentage, 0.0);
}

#[test]
fn parse_unterminated_section_before_empty_sf() {
    // Ensures an unterminated section is flushed when a new SF: line arrives,
    // even if the new SF: path is empty (and thus skipped).
    let lcov = "\
SF:/src/file1.ts
LF:10
LH:8
SF:
LF:5
LH:3
end_of_record
";
    let report = parse_lcov(lcov);
    // file1 should be flushed with its own metrics (10/8), not overwritten
    assert_eq!(report.files.len(), 1);
    assert_eq!(report.files[0].path, "/src/file1.ts");
    assert_eq!(report.files[0].lines.covered, 8);
    assert_eq!(report.files[0].lines.total, 10);
}

#[test]
fn parse_unterminated_section_before_next_sf() {
    // Ensures an unterminated section is flushed when a new valid SF: starts.
    let lcov = "\
SF:/src/file1.ts
LF:10
LH:8
SF:/src/file2.ts
LF:5
LH:3
end_of_record
";
    let report = parse_lcov(lcov);
    assert_eq!(report.files.len(), 2);
    assert_eq!(report.files[0].path, "/src/file1.ts");
    assert_eq!(report.files[0].lines.covered, 8);
    assert_eq!(report.files[0].lines.total, 10);
    assert_eq!(report.files[1].path, "/src/file2.ts");
    assert_eq!(report.files[1].lines.covered, 3);
    assert_eq!(report.files[1].lines.total, 5);
}

// ============================================================================
// WASM entry point
// ============================================================================

#[test]
fn parse_coverage_wasm_returns_valid_json() {
    let result = gaffer_parsers::parse_coverage(SAMPLE_LCOV, "coverage.lcov");
    assert!(result.is_ok());
    let json = result.unwrap();
    let parsed: gaffer_parsers::CoverageReport = serde_json::from_str(&json).unwrap();
    assert_eq!(parsed.format, "lcov");
    assert_eq!(parsed.files.len(), 2);
    assert_eq!(parsed.summary.lines.covered, 13);
    assert_eq!(parsed.summary.lines.percentage, 87.0);
}

#[test]
fn parse_coverage_wasm_returns_error_for_invalid_input() {
    let result = gaffer_parsers::parse_coverage("not lcov data", "file.lcov");
    assert!(result.is_err());
}

// ============================================================================
// Registry integration
// ============================================================================

#[test]
fn registry_includes_lcov_parser() {
    let registry = ParserRegistry::with_defaults();
    let ids = registry.parser_ids();
    assert!(ids.contains(&"lcov"), "Registry should include LCOV parser");
}

#[test]
fn registry_detects_lcov_content() {
    let registry = ParserRegistry::with_defaults();
    let result = registry.parse(SAMPLE_LCOV, "coverage.lcov");
    assert!(result.is_some(), "Registry should detect LCOV content");
    match result.unwrap().unwrap() {
        ParseResult::Coverage(report) => {
            assert_eq!(report.format, "lcov");
            assert_eq!(report.files.len(), 2);
        }
        _ => panic!("Expected Coverage result"),
    }
}

#[test]
fn registry_detects_lcov_by_content_markers() {
    let registry = ParserRegistry::with_defaults();
    let lcov = "SF:/src/file.ts\nDA:1,5\nLF:1\nLH:1\nend_of_record\n";
    let result = registry.parse(lcov, "report.txt");
    assert!(result.is_some(), "Registry should detect LCOV by content markers");
    match result.unwrap().unwrap() {
        ParseResult::Coverage(report) => {
            assert_eq!(report.format, "lcov");
        }
        _ => panic!("Expected Coverage result"),
    }
}
