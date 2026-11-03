use std::collections::BTreeMap;

use serde::Deserialize;

use crate::detect::extract_json_top_level_keys;
use crate::registry::Parser;
use crate::types::{
    Mutant, MutantStatus, MutationFile, MutationReport, MutationSummary, MutationTest,
    MutationThresholds, ParseError, ParseResult, ResultType,
};

/// Parser for the Stryker `mutation-testing-report-schema` JSON, written by
/// StrykerJS, Stryker.NET, Stryker4s, Infection's `stryker` logger and
/// cargo-mutants.
pub struct StrykerParser;

impl Parser for StrykerParser {
    fn id(&self) -> &str {
        "stryker"
    }

    fn name(&self) -> &str {
        "Stryker mutation report"
    }

    fn priority(&self) -> u8 {
        85
    }

    fn result_type(&self) -> ResultType {
        ResultType::Mutation
    }

    fn detect(&self, content: &str, _filename: &str) -> u8 {
        // The three keys the schema requires. No other supported format has
        // `schemaVersion` + `thresholds` at the root, so the filename adds nothing.
        let keys = extract_json_top_level_keys(content);
        let has = |name: &str| keys.iter().any(|k| k == name);
        if has("schemaVersion") && has("thresholds") && has("files") {
            90
        } else {
            0
        }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let raw: RawReport = serde_json::from_str(content)
            .map_err(|e| ParseError::from(format!("Invalid Stryker mutation report: {}", e)))?;

        let mut summary = MutationSummary::default();
        let files = raw
            .files
            .into_iter()
            .map(|(path, file)| {
                let mut file_summary = MutationSummary::default();
                for mutant in &file.mutants {
                    count(&mut file_summary, mutant.status);
                    count(&mut summary, mutant.status);
                }
                MutationFile {
                    path,
                    score: score(&file_summary),
                    mutants: file.mutants,
                }
            })
            .collect();
        summary.score = score(&summary);

        // testFiles keys are file paths or class names; only the tests matter.
        let tests = raw
            .test_files
            .unwrap_or_default()
            .into_values()
            .flat_map(|f| f.tests)
            .collect();

        Ok(ParseResult::Mutation(MutationReport {
            format: "stryker".to_string(),
            schema_version: raw.schema_version.into_string(),
            thresholds: raw.thresholds,
            summary,
            files,
            tests,
        }))
    }
}

fn count(summary: &mut MutationSummary, status: MutantStatus) {
    summary.total += 1;
    let bucket = match status {
        MutantStatus::Killed => &mut summary.killed,
        MutantStatus::Survived => &mut summary.survived,
        MutantStatus::NoCoverage => &mut summary.no_coverage,
        MutantStatus::Timeout => &mut summary.timeout,
        MutantStatus::CompileError => &mut summary.compile_error,
        MutantStatus::RuntimeError => &mut summary.runtime_error,
        MutantStatus::Ignored => &mut summary.ignored,
        MutantStatus::Pending => &mut summary.pending,
    };
    *bucket += 1;
}

/// Stryker's mutation score: detected / (detected + undetected), where
/// detected is killed + timeout and undetected is survived + no coverage.
/// Rounded to two decimals; `None` when nothing is valid to score.
fn score(s: &MutationSummary) -> Option<f64> {
    let detected = s.killed + s.timeout;
    let valid = detected + s.survived + s.no_coverage;
    if valid == 0 {
        return None;
    }
    let pct = detected as f64 * 100.0 / valid as f64;
    Some((pct * 100.0).round() / 100.0)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawReport {
    schema_version: SchemaVersion,
    thresholds: MutationThresholds,
    files: BTreeMap<String, RawFile>,
    test_files: Option<BTreeMap<String, RawTestFile>>,
}

/// The schema says string, but Infection writes `"schemaVersion": 1`.
#[derive(Deserialize)]
#[serde(untagged)]
enum SchemaVersion {
    Text(String),
    Number(serde_json::Number),
}

impl SchemaVersion {
    fn into_string(self) -> String {
        match self {
            SchemaVersion::Text(s) => s,
            SchemaVersion::Number(n) => n.to_string(),
        }
    }
}

#[derive(Deserialize)]
struct RawFile {
    mutants: Vec<Mutant>,
}

#[derive(Deserialize)]
struct RawTestFile {
    tests: Vec<MutationTest>,
}
