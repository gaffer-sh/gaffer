pub mod detect;
mod clover;
mod cobertura;
mod ctrf;
mod jacoco;
mod jest_vitest_json;
mod junit;
mod lcov;
mod playwright_json;
mod trx;
pub(crate) mod xml_helpers;
pub mod registry;
pub mod types;

pub use clover::CloverParser;
pub use cobertura::CoberturaParser;
pub use ctrf::CtrfParser;
pub use jacoco::JacocoParser;
pub use jest_vitest_json::JestVitestParser;
pub use junit::JUnitParser;
pub use lcov::LcovParser;
pub use playwright_json::PlaywrightJsonParser;
pub use trx::TrxParser;
pub use registry::{Parser, ParserRegistry};
pub use types::{
    CoverageMetrics, CoverageReport, CoverageReportSummary, DetectionMatch, FileCoverage,
    ParseError, ParseResult, ParsedReport, ResultType, Summary, TestCase, TestStatus,
};

use std::sync::OnceLock;
use wasm_bindgen::prelude::*;

static REGISTRY: OnceLock<ParserRegistry> = OnceLock::new();

fn get_registry() -> &'static ParserRegistry {
    REGISTRY.get_or_init(ParserRegistry::with_defaults)
}

#[wasm_bindgen]
pub fn detect_format(content: &str, filename: &str) -> Result<String, String> {
    match get_registry().detect(content, filename) {
        Some(detection) => serde_json::to_string(&detection)
            .map_err(|e| format!("JSON serialization failed: {}", e)),
        None => Ok("null".to_string()),
    }
}

#[wasm_bindgen]
pub fn parse_report(content: &str, filename: &str) -> Result<String, String> {
    match get_registry().parse(content, filename) {
        Some(Ok(ParseResult::Coverage(report))) => Err(format!(
            "File detected as coverage format ({}), not a test report. Use parse_coverage() instead.",
            report.format
        )),
        Some(Ok(result)) => serde_json::to_string(&result)
            .map_err(|e| format!("JSON serialization failed: {}", e)),
        Some(Err(e)) => Err(e.message),
        None => Err("No parser matched the input".to_string()),
    }
}

#[wasm_bindgen]
pub fn parse_coverage(content: &str, filename: &str) -> Result<String, String> {
    match get_registry().parse(content, filename) {
        Some(Ok(ParseResult::Coverage(report))) => serde_json::to_string(&report)
            .map_err(|e| format!("JSON serialization failed: {}", e)),
        Some(Ok(_)) => Err("Not a coverage format".to_string()),
        Some(Err(e)) => Err(e.message),
        None => Err("No coverage parser matched the input".to_string()),
    }
}
