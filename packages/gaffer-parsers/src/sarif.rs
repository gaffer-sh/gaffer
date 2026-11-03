use std::collections::{BTreeMap, HashMap, HashSet};

use serde::Deserialize;

use crate::detect::extract_json_top_level_keys;
use crate::registry::Parser;
use crate::types::{
    Finding, FindingLevel, FindingLocation, FindingsReport, FindingsRule, FindingsSummary,
    FindingsTool, ParseError, ParseResult, ResultType,
};

/// Partial fingerprint key GitHub code scanning and CodeQL write; preferred
/// over our own `g1:` hash because it survives lines moving above the finding.
const LINE_HASH_KEY: &str = "primaryLocationLineHash/v1";

/// Parser for SARIF 2.1.0 static-analysis logs, the cross-tool format written
/// natively by ruff, golangci-lint, Semgrep, CodeQL, Trivy and Checkov, and by
/// ESLint and clippy through formatter adapters.
pub struct SarifParser;

impl Parser for SarifParser {
    fn id(&self) -> &str {
        "sarif"
    }

    fn name(&self) -> &str {
        "SARIF 2.1.0"
    }

    fn priority(&self) -> u8 {
        80
    }

    fn result_type(&self) -> ResultType {
        ResultType::Findings
    }

    fn detect(&self, content: &str, filename: &str) -> u8 {
        let keys = extract_json_top_level_keys(content);
        let has = |name: &str| keys.iter().any(|k| k == name);
        if has("$schema") && schema_mentions_sarif(content) {
            100
        } else if has("version") && has("runs") {
            90
        } else if filename.to_ascii_lowercase().ends_with(".sarif") {
            60
        } else {
            0
        }
    }

    fn parse(&self, content: &str, _filename: &str) -> Result<ParseResult, ParseError> {
        let raw: RawLog = serde_json::from_str(content)
            .map_err(|e| ParseError::from(format!("Invalid SARIF log: {}", e)))?;

        let mut tools: Vec<FindingsTool> = Vec::new();
        let mut rules: Vec<FindingsRule> = Vec::new();
        let mut seen_rules: HashSet<String> = HashSet::new();
        let mut findings = Vec::new();

        for run in raw.runs {
            let tool = FindingsTool {
                name: run.tool.driver.name.clone(),
                version: run.tool.driver.version.clone(),
                information_uri: run.tool.driver.information_uri.clone(),
            };
            if !tools.contains(&tool) {
                tools.push(tool);
            }
            for rule in run.tool.components().flat_map(|c| &c.rules) {
                if seen_rules.insert(rule.id.clone()) {
                    rules.push(rule.to_rule());
                }
            }
            let lookup = RuleLookup::new(&run.tool);
            for result in run.results.unwrap_or_default() {
                findings.push(to_finding(result, &lookup, &run.tool.driver.name));
            }
        }

        Ok(ParseResult::Findings(FindingsReport {
            format: "sarif".to_string(),
            version: raw.version.unwrap_or_else(|| "2.1.0".to_string()),
            tools,
            summary: summarize(&findings),
            rules,
            findings,
        }))
    }
}

/// True when the first `"$schema"` value names SARIF. Reads only the string
/// after the key, so detection stays a scan rather than a parse.
fn schema_mentions_sarif(content: &str) -> bool {
    let value = || -> Option<&str> {
        let key = "\"$schema\"";
        let rest = &content[content.find(key)? + key.len()..];
        let rest = rest.trim_start().strip_prefix(':')?.trim_start().strip_prefix('"')?;
        Some(&rest[..rest.find('"')?])
    };
    value().is_some_and(|v| v.to_ascii_lowercase().contains("sarif"))
}

fn to_finding(result: RawResult, lookup: &RuleLookup, tool_name: &str) -> Finding {
    let (rule_id, rule) = lookup.resolve(&result);
    let level = result
        .level
        .as_deref()
        .and_then(parse_level)
        .or_else(|| rule.and_then(RawRule::default_level))
        .unwrap_or(FindingLevel::Warning);
    let location = result
        .locations
        .iter()
        .find_map(|l| l.physical_location.as_ref())
        .and_then(RawPhysicalLocation::to_location);
    let suppressed = !result.suppressions.is_empty()
        && result
            .suppressions
            .iter()
            .all(|s| s.status.as_deref() != Some("rejected"));
    Finding {
        fingerprint: fingerprint(result.partial_fingerprints.as_ref(), &rule_id, location.as_ref()),
        message: message(&result.message, rule),
        rule_id,
        tool_name: tool_name.to_string(),
        level,
        location,
        partial_fingerprints: result.partial_fingerprints,
        suppressed,
    }
}

/// `text`, else `markdown`, else the rule's short description. `{0}`-style
/// placeholders are filled from `arguments` when the tool supplied any.
fn message(m: &RawMessage, rule: Option<&RawRule>) -> String {
    let text = m
        .text
        .as_deref()
        .or(m.markdown.as_deref())
        .or_else(|| rule.and_then(|r| r.short_description.as_ref()?.text.as_deref()))
        .unwrap_or("");
    m.arguments
        .iter()
        .enumerate()
        .fold(text.to_string(), |acc, (i, arg)| acc.replace(&format!("{{{i}}}"), arg))
}

/// `s1:` + the tool's line hash when present, else `g1:` + FNV-1a 64 of
/// rule id, uri, start line and start column joined by NUL.
fn fingerprint(
    partial: Option<&BTreeMap<String, String>>,
    rule_id: &str,
    location: Option<&FindingLocation>,
) -> String {
    if let Some(hash) = partial.and_then(|p| p.get(LINE_HASH_KEY)) {
        return format!("s1:{hash}");
    }
    let num = |n: Option<u32>| n.map(|v| v.to_string()).unwrap_or_default();
    let (uri, line, column) = match location {
        Some(l) => (l.uri.as_str(), num(l.start_line), num(l.start_column)),
        None => ("", String::new(), String::new()),
    };
    let key = format!("{rule_id}\0{uri}\0{line}\0{column}");
    format!("g1:{:016x}", fnv1a_64(key.as_bytes()))
}

/// FNV-1a, 64-bit. Not cryptographic; it only has to be stable and spread well.
fn fnv1a_64(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &b| {
        (hash ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

fn parse_level(level: &str) -> Option<FindingLevel> {
    match level {
        "error" => Some(FindingLevel::Error),
        "warning" => Some(FindingLevel::Warning),
        "note" => Some(FindingLevel::Note),
        "none" => Some(FindingLevel::None),
        _ => None,
    }
}

fn strip_uri(uri: &str) -> String {
    uri.strip_prefix("file:///")
        .or_else(|| uri.strip_prefix("file://"))
        .or_else(|| uri.strip_prefix("./"))
        .unwrap_or(uri)
        .to_string()
}

fn summarize(findings: &[Finding]) -> FindingsSummary {
    let mut summary = FindingsSummary {
        total: findings.len(),
        rules_count: findings.iter().map(|f| f.rule_id.as_str()).collect::<HashSet<_>>().len(),
        files_count: findings
            .iter()
            .filter_map(|f| f.location.as_ref().map(|l| l.uri.as_str()))
            .collect::<HashSet<_>>()
            .len(),
        ..FindingsSummary::default()
    };
    for f in findings {
        match f.level {
            FindingLevel::Error => summary.error += 1,
            FindingLevel::Warning => summary.warning += 1,
            FindingLevel::Note => summary.note += 1,
            FindingLevel::None => summary.none += 1,
        }
    }
    summary
}

/// Resolves a result's rule within its run: by id across the driver and
/// extensions, else by index into the component `rule.toolComponent` names.
struct RuleLookup<'a> {
    tool: &'a RawTool,
    by_id: HashMap<&'a str, &'a RawRule>,
}

impl<'a> RuleLookup<'a> {
    fn new(tool: &'a RawTool) -> Self {
        let mut by_id = HashMap::new();
        for rule in tool.components().flat_map(|c| &c.rules) {
            by_id.entry(rule.id.as_str()).or_insert(rule);
        }
        RuleLookup { tool, by_id }
    }

    fn resolve(&self, result: &RawResult) -> (String, Option<&'a RawRule>) {
        let reference = result.rule.as_ref();
        if let Some(id) = result.rule_id.clone().or_else(|| reference?.id.clone()) {
            let rule = self.by_id.get(id.as_str()).copied();
            return (id, rule);
        }
        let component = reference
            .and_then(|r| index(r.tool_component.as_ref()?.index))
            .and_then(|i| self.tool.extensions.get(i))
            .unwrap_or(&self.tool.driver);
        let rule = index(result.rule_index)
            .or_else(|| index(reference?.index))
            .and_then(|i| component.rules.get(i));
        (rule.map(|r| r.id.clone()).unwrap_or_default(), rule)
    }
}

/// SARIF indexes default to -1, meaning absent.
fn index(i: Option<i64>) -> Option<usize> {
    usize::try_from(i?).ok()
}

// ============================================================================
// Raw SARIF shapes: only the fields the contract reads, all lenient.
// ============================================================================

#[derive(Deserialize)]
struct RawLog {
    version: Option<String>,
    #[serde(default)]
    runs: Vec<RawRun>,
}

#[derive(Deserialize)]
struct RawRun {
    tool: RawTool,
    /// `null` is legal and means the tool did not run.
    results: Option<Vec<RawResult>>,
}

#[derive(Deserialize)]
struct RawTool {
    driver: RawComponent,
    #[serde(default)]
    extensions: Vec<RawComponent>,
}

impl RawTool {
    fn components(&self) -> impl Iterator<Item = &RawComponent> {
        std::iter::once(&self.driver).chain(&self.extensions)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawComponent {
    #[serde(default)]
    name: String,
    version: Option<String>,
    information_uri: Option<String>,
    #[serde(default)]
    rules: Vec<RawRule>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRule {
    id: String,
    name: Option<String>,
    short_description: Option<RawText>,
    help_uri: Option<String>,
    default_configuration: Option<RawConfiguration>,
}

impl RawRule {
    fn default_level(&self) -> Option<FindingLevel> {
        parse_level(self.default_configuration.as_ref()?.level.as_deref()?)
    }

    fn to_rule(&self) -> FindingsRule {
        FindingsRule {
            id: self.id.clone(),
            name: self.name.clone(),
            short_description: self.short_description.as_ref().and_then(|d| d.text.clone()),
            help_uri: self.help_uri.clone(),
            default_level: self.default_level(),
        }
    }
}

#[derive(Deserialize)]
struct RawText {
    text: Option<String>,
}

#[derive(Deserialize)]
struct RawConfiguration {
    level: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawResult {
    rule_id: Option<String>,
    rule_index: Option<i64>,
    rule: Option<RawRuleReference>,
    level: Option<String>,
    #[serde(default)]
    message: RawMessage,
    #[serde(default)]
    locations: Vec<RawLocation>,
    partial_fingerprints: Option<BTreeMap<String, String>>,
    #[serde(default)]
    suppressions: Vec<RawSuppression>,
}

#[derive(Deserialize, Default)]
struct RawMessage {
    text: Option<String>,
    markdown: Option<String>,
    #[serde(default)]
    arguments: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRuleReference {
    id: Option<String>,
    index: Option<i64>,
    tool_component: Option<RawComponentReference>,
}

#[derive(Deserialize)]
struct RawComponentReference {
    index: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLocation {
    physical_location: Option<RawPhysicalLocation>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPhysicalLocation {
    artifact_location: Option<RawArtifactLocation>,
    region: Option<RawRegion>,
}

impl RawPhysicalLocation {
    fn to_location(&self) -> Option<FindingLocation> {
        let uri = self.artifact_location.as_ref()?.uri.as_deref()?;
        let region = self.region.as_ref();
        Some(FindingLocation {
            uri: strip_uri(uri),
            start_line: region.and_then(|r| r.start_line),
            start_column: region.and_then(|r| r.start_column),
            end_line: region.and_then(|r| r.end_line),
            end_column: region.and_then(|r| r.end_column),
        })
    }
}

#[derive(Deserialize)]
struct RawArtifactLocation {
    uri: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawRegion {
    start_line: Option<u32>,
    start_column: Option<u32>,
    end_line: Option<u32>,
    end_column: Option<u32>,
}

#[derive(Deserialize)]
struct RawSuppression {
    status: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_64_matches_published_test_vectors() {
        assert_eq!(fnv1a_64(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a_64(b"a"), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a_64(b"foobar"), 0x8594_4171_f739_67e8);
    }

    #[test]
    fn strip_uri_removes_one_file_scheme_or_dot_slash_prefix() {
        assert_eq!(strip_uri("file:///home/a/b.py"), "home/a/b.py");
        assert_eq!(strip_uri("file://host/b.py"), "host/b.py");
        assert_eq!(strip_uri("./src/b.py"), "src/b.py");
        assert_eq!(strip_uri("src/b.py"), "src/b.py");
    }

    #[test]
    fn message_fills_argument_placeholders() {
        let m = RawMessage {
            text: Some("{0} is unused in {1}".to_string()),
            markdown: None,
            arguments: vec!["x".to_string(), "f".to_string()],
        };
        assert_eq!(message(&m, None), "x is unused in f");
    }

    #[test]
    fn schema_value_must_mention_sarif() {
        assert!(schema_mentions_sarif(r#"{"$schema" : "https://json.schemastore.org/sarif-2.1.0.json"}"#));
        assert!(!schema_mentions_sarif(r#"{"$schema":"https://json-schema.org/draft-07/schema"}"#));
        assert!(!schema_mentions_sarif(r#"{"$schema": 3}"#));
    }
}
