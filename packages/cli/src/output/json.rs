//! JSON output mode — machine-parseable output to stdout for AI agents and CI pipelines.

use std::path::PathBuf;

use gaffer_core::types::{
    ComparisonResult, CoverageSummary, HealthScore, RunReport, SyncResult, TestEvent,
    TestIntelligence,
};
use serde::Serialize;

#[derive(Serialize)]
pub struct JsonOutput<'a> {
    pub summary: &'a gaffer_core::types::RunSummary,
    pub failures: Vec<FailureEntry<'a>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<&'a HealthScore>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<&'a TestIntelligence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub coverage: Option<&'a CoverageSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sync: Option<&'a SyncResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comparison: Option<&'a ComparisonResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub context_files: Vec<String>,
}

#[derive(Serialize)]
pub struct FailureEntry<'a> {
    pub name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<&'a str>,
    pub duration_ms: f64,
}

/// Serialize the full JSON output to stdout. Returns an error if serialization
/// fails (e.g. NaN/Infinity in f64 fields from malformed test reports).
pub fn print_json(
    report: &RunReport,
    failures: &[&TestEvent],
    context_files: &[PathBuf],
    coverage: Option<&CoverageSummary>,
    sync_result: Option<&SyncResult>,
    comparison: Option<&ComparisonResult>,
) -> Result<(), serde_json::Error> {
    let failure_entries: Vec<FailureEntry> = failures
        .iter()
        .map(|t| FailureEntry {
            name: &t.name,
            file: t.file_path.as_deref(),
            error: t.error.as_deref(),
            duration_ms: t.duration,
        })
        .collect();

    let context_strings: Vec<String> = context_files
        .iter()
        .map(|p| p.display().to_string())
        .collect();

    let output = JsonOutput {
        summary: &report.summary,
        failures: failure_entries,
        health: report.health.as_ref(),
        intelligence: report.intelligence.as_ref(),
        coverage,
        sync: sync_result,
        comparison,
        context_files: context_strings,
    };

    let json = serde_json::to_string(&output)?;
    println!("{}", json);
    Ok(())
}
