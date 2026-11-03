//! SARIF 2.1.0 findings parser. The ruff, ESLint and clippy fixtures are real
//! tool output (versions in `fixtures/sarif/README.md`); the CodeQL-style one is
//! hand-written to cover what those three never emit. Every expected number
//! below was counted from the fixture itself.

use gaffer_parsers::{
    parse_coverage, parse_findings, parse_mutation, parse_report, Finding, FindingLevel,
    FindingsReport, FindingsSummary, Parser, ParseResult, ParserRegistry, ResultType, SarifParser,
};

const RUFF: &str = "ruff.sarif";
const ESLINT: &str = "eslint.sarif";
const CLIPPY: &str = "clippy.sarif";
const SYNTHETIC: &str = "synthetic-codeql-style.sarif";

fn load_fixture(name: &str) -> String {
    let path = format!("{}/tests/fixtures/sarif/{}", env!("CARGO_MANIFEST_DIR"), name);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e))
}

fn parse(name: &str) -> FindingsReport {
    match SarifParser.parse(&load_fixture(name), name).unwrap() {
        ParseResult::Findings(report) => report,
        other => panic!("Expected Findings, got {other:?}"),
    }
}

fn summary(
    total: usize,
    error: usize,
    warning: usize,
    note: usize,
    rules_count: usize,
    files_count: usize,
) -> FindingsSummary {
    FindingsSummary { total, error, warning, note, none: 0, rules_count, files_count }
}

fn find<'a>(report: &'a FindingsReport, rule_id: &str) -> &'a Finding {
    report
        .findings
        .iter()
        .find(|f| f.rule_id == rule_id)
        .unwrap_or_else(|| panic!("no finding for {rule_id}"))
}

// ============================================================================
// Detection
// ============================================================================

#[test]
fn registry_detects_every_fixture_as_sarif_findings() {
    let registry = ParserRegistry::with_defaults();
    for name in [RUFF, ESLINT, CLIPPY, SYNTHETIC] {
        let m = registry.detect(&load_fixture(name), "results.json").unwrap();
        assert_eq!(m.parser_id, "sarif", "{name}");
        assert_eq!(m.parser_name, "SARIF 2.1.0");
        assert!(m.score >= 50, "{name} scored {}", m.score);
        assert_eq!(m.result_type, ResultType::Findings);
    }
}

#[test]
fn detection_scores_schema_then_keys_then_filename() {
    // All three real tools write a SARIF `$schema`.
    for name in [RUFF, ESLINT, CLIPPY] {
        assert_eq!(SarifParser.detect(&load_fixture(name), name), 100, "{name}");
    }
    let keys_only = r#"{"version":"2.1.0","runs":[]}"#;
    assert_eq!(SarifParser.detect(keys_only, "out.json"), 90);
    let other_schema = r#"{"$schema":"https://json-schema.org/draft-07/schema","runs":[]}"#;
    assert_eq!(SarifParser.detect(other_schema, "out.json"), 0);
    assert_eq!(SarifParser.detect(other_schema, "OUT.SARIF"), 60);
    assert_eq!(SarifParser.detect("{}", "out.json"), 0);
}

// ============================================================================
// Real tool output
// ============================================================================

#[test]
fn ruff_counts_rules_and_tool() {
    let r = parse(RUFF);
    assert_eq!(r.format, "sarif");
    assert_eq!(r.version, "2.1.0");
    assert_eq!(r.summary, summary(10, 10, 0, 0, 8, 2));
    assert_eq!(r.rules.len(), 8);
    assert_eq!(r.tools.len(), 1);
    assert_eq!(r.tools[0].name, "ruff");
    assert_eq!(r.tools[0].version.as_deref(), Some("0.17.3"));
    assert!(r.findings.iter().all(|f| f.tool_name == "ruff" && !f.suppressed));

    let e711 = r.rules.iter().find(|rule| rule.id == "E711").unwrap();
    // ruff puts the rule's name under `properties.name`, not SARIF's `name`.
    assert_eq!(e711.name, None);
    assert_eq!(e711.short_description.as_deref(), Some("Comparison to `None` should be `cond is None`"));
    assert_eq!(e711.help_uri.as_deref(), Some("https://docs.astral.sh/ruff/rules/none-comparison"));
    assert_eq!(e711.default_level, Some(FindingLevel::Error));
}

#[test]
fn ruff_absolute_file_uri_loses_its_scheme() {
    let r = parse(RUFF);
    let loc = find(&r, "F841").location.as_ref().unwrap();
    assert!(!loc.uri.starts_with("file:"), "{}", loc.uri);
    assert!(loc.uri.ends_with("ruff/src/app/main.py"), "{}", loc.uri);
    assert_eq!((loc.start_line, loc.start_column, loc.end_line, loc.end_column), (Some(5), Some(5), Some(5), Some(11)));
}

#[test]
fn eslint_counts_levels_and_suppression() {
    let r = parse(ESLINT);
    assert_eq!(r.summary, summary(9, 6, 3, 0, 6, 2));
    assert_eq!(r.rules.len(), 6);
    assert_eq!(r.tools[0].name, "ESLint");
    assert_eq!(r.tools[0].version.as_deref(), Some("9.39.5"));
    assert_eq!(r.tools[0].information_uri.as_deref(), Some("https://eslint.org"));

    // `// eslint-disable-next-line` arrives as an inSource suppression; the
    // other results carry an empty `suppressions` array.
    let suppressed: Vec<_> = r.findings.iter().filter(|f| f.suppressed).collect();
    assert_eq!(suppressed.len(), 1);
    assert_eq!(suppressed[0].message, "'legacy' is assigned a value but never used.");
    // ESLint's formatter writes no partial fingerprints, so every one is `g1:`.
    assert!(r.findings.iter().all(|f| f.fingerprint.starts_with("g1:") && f.partial_fingerprints.is_none()));
}

#[test]
fn clippy_counts_and_relative_uris() {
    let r = parse(CLIPPY);
    assert_eq!(r.summary, summary(5, 0, 5, 0, 5, 2));
    assert_eq!(r.tools[0].name, "clippy");
    assert_eq!(r.tools[0].version, None);
    let f = find(&r, "clippy::needless_return");
    assert_eq!(f.message, "unneeded `return` statement");
    assert_eq!(f.location.as_ref().unwrap().uri, "src/util.rs");
}

#[test]
fn clippy_g1_fingerprint_matches_hand_computed_hash() {
    // FNV-1a 64 of "clippy::needless_return\0src/util.rs\03\05", computed
    // independently in Python.
    let r = parse(CLIPPY);
    let f = find(&r, "clippy::needless_return");
    assert_eq!(f.fingerprint, "g1:5b40acec9567ceae");
}

// ============================================================================
// Synthetic: fields the three real tools never emit
// ============================================================================

#[test]
fn synthetic_merges_runs_and_dedupes_rules_across_components() {
    let r = parse(SYNTHETIC);
    assert_eq!(r.summary, summary(3, 1, 1, 1, 2, 2));
    let tools: Vec<_> = r.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(tools, ["CodeQL", "Semgrep OSS"]);
    let rules: Vec<_> = r.rules.iter().map(|rule| rule.id.as_str()).collect();
    assert_eq!(rules, ["js/unused-local-variable", "js/sql-injection"]);
}

#[test]
fn synthetic_uses_the_line_hash_and_fills_arguments() {
    let f = &parse(SYNTHETIC).findings[0];
    assert_eq!(f.fingerprint, "s1:39fa2ee980eb94b0:1");
    assert_eq!(f.message, "Unused variable tmp.");
    assert_eq!(f.partial_fingerprints.as_ref().unwrap().len(), 2);
    // No result level: falls back to the rule's defaultConfiguration.
    assert_eq!(f.level, FindingLevel::Note);
    assert_eq!(f.location.as_ref().unwrap().uri, "src/app.js");
}

#[test]
fn synthetic_resolves_rule_by_extension_index() {
    let f = &parse(SYNTHETIC).findings[1];
    assert_eq!(f.rule_id, "js/sql-injection");
    assert_eq!(f.level, FindingLevel::Error);
    assert_eq!(f.message, "Query built from [user input](1).");
    assert_eq!(f.location.as_ref().unwrap().uri, "src/db.js");
    // A rejected suppression does not suppress.
    assert!(!f.suppressed);
    assert_eq!(f.fingerprint, "g1:3b5078eb4b3ac740");
}

#[test]
fn synthetic_without_physical_location() {
    let f = &parse(SYNTHETIC).findings[2];
    assert_eq!(f.level, FindingLevel::Warning);
    assert_eq!(f.message, "Database query built from user-controlled sources");
    assert_eq!(f.location, None);
    assert!(f.suppressed);
    assert_eq!(f.fingerprint, "g1:bae5c8973f326497");
}

#[test]
fn level_defaults_to_warning_without_result_or_rule_level() {
    let content = r#"{"version":"2.1.0","runs":[{"tool":{"driver":{"name":"t"}},"results":[{"ruleId":"x","message":{"text":"m"}}]}]}"#;
    let ParseResult::Findings(r) = SarifParser.parse(content, "a.sarif").unwrap() else {
        panic!("not findings")
    };
    assert_eq!(r.findings[0].level, FindingLevel::Warning);
}

#[test]
fn invalid_json_is_a_parse_error() {
    let err = SarifParser.parse(r#"{"version":"2.1.0","runs":[{"#, "a.sarif").unwrap_err();
    assert!(err.message.starts_with("Invalid SARIF log"), "{}", err.message);
}

// ============================================================================
// WASM exports
// ============================================================================

#[test]
fn parse_findings_returns_the_contract_json() {
    let json: serde_json::Value =
        serde_json::from_str(&parse_findings(&load_fixture(SYNTHETIC), SYNTHETIC).unwrap()).unwrap();
    assert_eq!(json["format"], "sarif");
    assert_eq!(json["version"], "2.1.0");
    assert_eq!(json["summary"]["rulesCount"], 2);
    assert_eq!(json["summary"]["filesCount"], 2);
    assert_eq!(json["tools"][1]["informationUri"], "https://semgrep.dev");
    assert!(json["tools"][0].get("version").is_none());
    assert_eq!(json["rules"][1]["helpUri"], "https://codeql.github.com/codeql-query-help/javascript/js-sql-injection/");
    assert_eq!(json["rules"][0]["defaultLevel"], "note");

    let first = &json["findings"][0];
    assert_eq!(first["ruleId"], "js/unused-local-variable");
    assert_eq!(first["toolName"], "CodeQL");
    assert_eq!(first["level"], "note");
    assert_eq!(first["location"]["startLine"], 12);
    assert!(first["location"].get("endLine").is_none());
    assert_eq!(first["partialFingerprints"]["primaryLocationLineHash/v1"], "39fa2ee980eb94b0:1");
    assert_eq!(first["suppressed"], false);

    let third = &json["findings"][2];
    assert!(third["location"].is_null());
    assert!(third.get("partialFingerprints").is_none());
}

#[test]
fn other_exports_reject_a_sarif_file() {
    let expected = "File detected as findings format, use parse_findings()";
    for name in [RUFF, ESLINT, CLIPPY] {
        let content = load_fixture(name);
        assert_eq!(parse_report(&content, name).unwrap_err(), expected);
        assert_eq!(parse_coverage(&content, name).unwrap_err(), expected);
        assert_eq!(parse_mutation(&content, name).unwrap_err(), expected);
    }
}

#[test]
fn parse_findings_rejects_other_formats() {
    let lcov = "TN:\nSF:src/file.ts\nDA:1,1\nLF:1\nLH:1\nend_of_record\n";
    assert_eq!(parse_findings(lcov, "lcov.info").unwrap_err(), "Not a findings format");
}
