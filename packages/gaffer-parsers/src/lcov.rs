//! LCOV coverage report parser.
//!
//! LCOV is a line-oriented text format where each file section is delimited by:
//! - `SF:<path>` — start of file section
//! - `end_of_record` — end of file section
//!
//! Within each section we extract:
//! - `LF` / `LH` — lines found / lines hit
//! - `BRF` / `BRH` — branches found / branches hit
//! - `FNF` / `FNH` — functions found / functions hit

use crate::registry::Parser;
use crate::types::{
    CoverageReport, CoverageReportSummary, FileCoverage, ParseError, ParseResult,
    ResultType,
};
use crate::xml_helpers::make_metrics;

pub struct LcovParser;

/// Intermediate accumulator for a single file's coverage data.
#[derive(Debug, Default)]
struct FileAccumulator {
    path: String,
    lines_found: i32,
    lines_hit: i32,
    branches_found: i32,
    branches_hit: i32,
    functions_found: i32,
    functions_hit: i32,
}

impl Parser for LcovParser {
    fn id(&self) -> &str {
        "lcov"
    }

    fn name(&self) -> &str {
        "LCOV Coverage Report"
    }

    fn priority(&self) -> u8 {
        100
    }

    fn result_type(&self) -> ResultType {
        ResultType::Coverage
    }

    fn detect(&self, sample: &str, filename: &str) -> u8 {
        let lower = filename.to_lowercase();

        // Tier 1: file extension match
        if lower.ends_with(".lcov") || lower == "lcov.info" {
            return 100;
        }

        // Check basename for lcov.info
        if let Some(basename) = lower.rsplit('/').next() {
            if basename == "lcov.info" {
                return 100;
            }
        }

        // Tier 2: content-based detection
        let has_sf = sample.contains("SF:");
        let has_end = sample.contains("end_of_record");
        let has_da = sample.contains("DA:");
        let has_lf = sample.contains("LF:");

        if has_sf && has_end && (has_da || has_lf) {
            return 95;
        }

        if has_sf && has_end {
            return 80;
        }

        0
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let mut files: Vec<FileAccumulator> = Vec::new();
        let mut current: Option<FileAccumulator> = None;

        for line in content.lines() {
            let line = line.trim();

            if let Some(path) = line.strip_prefix("SF:") {
                // Flush any unterminated previous section
                if let Some(file) = current.take() {
                    files.push(file);
                }
                // Skip empty SF: paths
                if !path.is_empty() {
                    current = Some(FileAccumulator {
                        path: path.to_string(),
                        ..Default::default()
                    });
                }
            } else if line == "end_of_record" {
                if let Some(file) = current.take() {
                    files.push(file);
                }
            } else if let Some(current) = current.as_mut() {
                if let Some(val) = line.strip_prefix("LF:") {
                    current.lines_found = val.parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("LH:") {
                    current.lines_hit = val.parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("BRF:") {
                    current.branches_found = val.parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("BRH:") {
                    current.branches_hit = val.parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("FNF:") {
                    current.functions_found = val.parse().unwrap_or(0);
                } else if let Some(val) = line.strip_prefix("FNH:") {
                    current.functions_hit = val.parse().unwrap_or(0);
                }
            }
        }

        // Handle unterminated last section (some tools omit final end_of_record)
        if let Some(file) = current.take() {
            files.push(file);
        }

        if files.is_empty() {
            return Err(ParseError::from(
                "No valid file sections found in LCOV content".to_string(),
            ));
        }

        // Sum across all files for summary
        let mut total_lines_found = 0i32;
        let mut total_lines_hit = 0i32;
        let mut total_branches_found = 0i32;
        let mut total_branches_hit = 0i32;
        let mut total_functions_found = 0i32;
        let mut total_functions_hit = 0i32;

        let file_entries: Vec<FileCoverage> = files
            .iter()
            .map(|f| {
                total_lines_found += f.lines_found;
                total_lines_hit += f.lines_hit;
                total_branches_found += f.branches_found;
                total_branches_hit += f.branches_hit;
                total_functions_found += f.functions_found;
                total_functions_hit += f.functions_hit;

                FileCoverage {
                    path: f.path.clone(),
                    lines: make_metrics(f.lines_hit, f.lines_found),
                    branches: make_metrics(f.branches_hit, f.branches_found),
                    functions: make_metrics(f.functions_hit, f.functions_found),
                }
            })
            .collect();

        Ok(ParseResult::Coverage(CoverageReport {
            format: "lcov".to_string(),
            summary: CoverageReportSummary {
                lines: make_metrics(total_lines_hit, total_lines_found),
                branches: make_metrics(total_branches_hit, total_branches_found),
                functions: make_metrics(total_functions_hit, total_functions_found),
            },
            files: file_entries,
        }))
    }
}
