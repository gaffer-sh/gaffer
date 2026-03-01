//! Shared XML parsing helpers used by multiple parsers.
//!
//! Extracted from `junit.rs` and `trx.rs` to avoid duplication across XML-based parsers.

use crate::types::{CoverageMetrics, CoverageReportSummary, FileCoverage};

/// Strip a UTF-8 BOM (U+FEFF) from the beginning of the input, if present.
pub fn strip_bom(input: &str) -> &str {
    input.strip_prefix('\u{FEFF}').unwrap_or(input)
}

/// Extract a string attribute value from a `BytesStart` element by attribute name.
pub fn get_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<String> {
    for attr in e.attributes().flatten() {
        if attr.key.as_ref() == name {
            return String::from_utf8(attr.value.to_vec()).ok();
        }
    }
    None
}

/// Get the local name of an XML element, stripping any namespace prefix.
pub fn local_name(full_name: &[u8]) -> &[u8] {
    match full_name.iter().position(|&b| b == b':') {
        Some(pos) => &full_name[pos + 1..],
        None => full_name,
    }
}

/// Extract an integer attribute, returning `None` if missing or unparseable.
pub fn get_int_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<i32> {
    get_attr(e, name).and_then(|s| s.parse::<i32>().ok())
}

/// Extract a float attribute, returning `None` if missing or unparseable.
pub fn get_float_attr(e: &quick_xml::events::BytesStart, name: &[u8]) -> Option<f64> {
    get_attr(e, name).and_then(|s| s.parse::<f64>().ok())
}

/// Compute a coverage percentage, returning 0.0 when total is 0.
pub fn compute_percentage(covered: i32, total: i32) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (((covered as f64) / (total as f64)) * 100.0).round()
}

/// Build a `CoverageMetrics` from covered/total counts.
pub fn make_metrics(covered: i32, total: i32) -> CoverageMetrics {
    CoverageMetrics {
        covered,
        total,
        percentage: compute_percentage(covered, total),
    }
}

/// Calculate a `CoverageReportSummary` by aggregating all file-level coverage data.
pub fn calculate_summary_from_files(files: &[FileCoverage]) -> CoverageReportSummary {
    let mut lines_covered = 0i32;
    let mut lines_total = 0i32;
    let mut branches_covered = 0i32;
    let mut branches_total = 0i32;
    let mut functions_covered = 0i32;
    let mut functions_total = 0i32;

    for file in files {
        lines_covered += file.lines.covered;
        lines_total += file.lines.total;
        branches_covered += file.branches.covered;
        branches_total += file.branches.total;
        functions_covered += file.functions.covered;
        functions_total += file.functions.total;
    }

    CoverageReportSummary {
        lines: make_metrics(lines_covered, lines_total),
        branches: make_metrics(branches_covered, branches_total),
        functions: make_metrics(functions_covered, functions_total),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_bom_with_bom() {
        assert_eq!(strip_bom("\u{FEFF}hello"), "hello");
    }

    #[test]
    fn strip_bom_without_bom() {
        assert_eq!(strip_bom("hello"), "hello");
    }

    #[test]
    fn strip_bom_empty() {
        assert_eq!(strip_bom(""), "");
    }

    #[test]
    fn local_name_with_namespace() {
        assert_eq!(local_name(b"ns:element"), b"element");
    }

    #[test]
    fn local_name_without_namespace() {
        assert_eq!(local_name(b"element"), b"element");
    }

    #[test]
    fn local_name_empty() {
        assert_eq!(local_name(b""), b"");
    }

    #[test]
    fn compute_percentage_normal() {
        assert_eq!(compute_percentage(85, 100), 85.0);
    }

    #[test]
    fn compute_percentage_division_by_zero() {
        assert_eq!(compute_percentage(0, 0), 0.0);
    }

    #[test]
    fn compute_percentage_rounds() {
        assert_eq!(compute_percentage(1, 3), 33.0);
        assert_eq!(compute_percentage(2, 3), 67.0);
    }

    #[test]
    fn compute_percentage_full() {
        assert_eq!(compute_percentage(50, 50), 100.0);
    }

    #[test]
    fn make_metrics_builds_correctly() {
        let m = make_metrics(8, 10);
        assert_eq!(m.covered, 8);
        assert_eq!(m.total, 10);
        assert_eq!(m.percentage, 80.0);
    }

    #[test]
    fn make_metrics_zero_total() {
        let m = make_metrics(0, 0);
        assert_eq!(m.covered, 0);
        assert_eq!(m.total, 0);
        assert_eq!(m.percentage, 0.0);
    }

    #[test]
    fn calculate_summary_from_files_empty() {
        let summary = calculate_summary_from_files(&[]);
        assert_eq!(summary.lines.total, 0);
        assert_eq!(summary.lines.covered, 0);
        assert_eq!(summary.lines.percentage, 0.0);
    }

    #[test]
    fn calculate_summary_from_files_aggregates() {
        let files = vec![
            FileCoverage {
                path: "a.rs".to_string(),
                lines: make_metrics(8, 10),
                branches: make_metrics(2, 4),
                functions: make_metrics(3, 3),
            },
            FileCoverage {
                path: "b.rs".to_string(),
                lines: make_metrics(12, 20),
                branches: make_metrics(1, 2),
                functions: make_metrics(2, 5),
            },
        ];
        let summary = calculate_summary_from_files(&files);
        assert_eq!(summary.lines.covered, 20);
        assert_eq!(summary.lines.total, 30);
        assert_eq!(summary.lines.percentage, 67.0);
        assert_eq!(summary.branches.covered, 3);
        assert_eq!(summary.branches.total, 6);
        assert_eq!(summary.functions.covered, 5);
        assert_eq!(summary.functions.total, 8);
    }
}
