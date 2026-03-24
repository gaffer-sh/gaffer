//! JSON output mode — machine-parseable output to stdout for AI agents and CI pipelines.

use std::path::PathBuf;

use gaffer_core::types::{
    ClassificationResult, ComparisonResult, CoverageSummary, FailureClassification,
    HealthScore, RunReport, SyncResult, TestEvent, TestIntelligence,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub classification: Option<ClassificationOutput>,
}

#[derive(Serialize)]
pub struct ClassificationOutput {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub baseline_branch: Option<String>,
    pub classified_failures: Vec<ClassifiedFailureEntry>,
}

#[derive(Serialize)]
pub struct ClassifiedFailureEntry {
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    pub classification: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flip_rate: Option<f64>,
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
    classification: &ClassificationResult,
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

    let classification_output = if classification.classified_failures.is_empty() {
        None
    } else {
        Some(ClassificationOutput {
            baseline_branch: classification.baseline_branch.clone(),
            classified_failures: classification
                .classified_failures
                .iter()
                .map(|f| ClassifiedFailureEntry {
                    name: f.name.clone(),
                    file: f.file_path.clone(),
                    classification: f.classification.to_string(),
                    flip_rate: if f.classification == FailureClassification::Flaky {
                        f.flip_rate
                    } else {
                        None
                    },
                })
                .collect(),
        })
    };

    let output = JsonOutput {
        summary: &report.summary,
        failures: failure_entries,
        health: report.health.as_ref(),
        intelligence: report.intelligence.as_ref(),
        coverage,
        sync: sync_result,
        comparison,
        context_files: context_strings,
        classification: classification_output,
    };

    let json = serde_json::to_string(&output)?;
    println!("{}", json);
    Ok(())
}
