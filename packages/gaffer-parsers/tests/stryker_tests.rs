//! Stryker mutation report parser. Fixtures are real emitter output from the
//! stryker-mutator/mutation-testing-elements `testResources` directory; every
//! expected number below was counted from the fixture itself.

use gaffer_parsers::{
    parse_coverage, parse_mutation, parse_report, MutantStatus, MutationReport, MutationSummary,
    Parser, ParseResult, ParserRegistry, ResultType, StrykerParser,
};

const STRYKERJS: &str = "strykerjs-svelte-mutation-report.json";
const INFECTION: &str = "infection-php-mutation-report.json";
const CARGO_MUTANTS: &str = "cargo-mutants-url-mutation-report.json";
const STRYKER4S: &str = "stryker4s-mutation-report.json";

fn load_fixture(name: &str) -> String {
    let path = format!(
        "{}/tests/fixtures/stryker/{}",
        env!("CARGO_MANIFEST_DIR"),
        name
    );
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e))
}

fn parse(name: &str) -> MutationReport {
    match StrykerParser.parse(&load_fixture(name), name).unwrap() {
        ParseResult::Mutation(report) => report,
        other => panic!("Expected Mutation, got {other:?}"),
    }
}

fn file<'a>(report: &'a MutationReport, path: &str) -> &'a gaffer_parsers::MutationFile {
    report
        .files
        .iter()
        .find(|f| f.path == path)
        .unwrap_or_else(|| panic!("no file {path}"))
}

// ============================================================================
// Detection
// ============================================================================

#[test]
fn registry_detects_every_fixture_as_stryker_mutation() {
    let registry = ParserRegistry::with_defaults();
    for name in [STRYKERJS, STRYKER4S, INFECTION, CARGO_MUTANTS] {
        let m = registry.detect(&load_fixture(name), name).unwrap();
        assert_eq!(m.parser_id, "stryker", "{name}");
        assert_eq!(m.parser_name, "Stryker mutation report");
        assert!(m.score >= 90, "{name} scored {}", m.score);
        assert_eq!(m.result_type, ResultType::Mutation);
    }
}

#[test]
fn detection_ignores_the_filename() {
    let content = load_fixture(STRYKERJS);
    assert_eq!(StrykerParser.detect(&content, "mutation.json"), 90);
    assert_eq!(StrykerParser.detect(&content, "report.json"), 90);
    assert_eq!(StrykerParser.detect(&content, "whatever"), 90);
}

#[test]
fn detection_needs_all_three_required_keys() {
    let parser = StrykerParser;
    assert_eq!(parser.detect(r#"{"schemaVersion":"1","files":{}}"#, "mutation.json"), 0);
    assert_eq!(parser.detect(r#"{"thresholds":{},"files":{}}"#, "mutation.json"), 0);
    assert_eq!(parser.detect(r#"{"schemaVersion":"1","thresholds":{}}"#, "mutation.json"), 0);
    assert_eq!(parser.detect("TN:\nSF:a\nend_of_record\n", "mutation.json"), 0);
}

// ============================================================================
// StrykerJS 7.3.0, schemaVersion "1.0", with testFiles
// ============================================================================

#[test]
fn strykerjs_report_header() {
    let report = parse(STRYKERJS);
    assert_eq!(report.format, "stryker");
    assert_eq!(report.schema_version, "1.0");
    assert_eq!((report.thresholds.high, report.thresholds.low), (80, 60));
}

#[test]
fn strykerjs_overall_counts_and_score() {
    let report = parse(STRYKERJS);
    assert_eq!(
        report.summary,
        MutationSummary {
            total: 25,
            killed: 13,
            survived: 3,
            no_coverage: 6,
            timeout: 3,
            score: Some(64.0),
            ..MutationSummary::default()
        }
    );
}

#[test]
fn strykerjs_per_file_scores() {
    let report = parse(STRYKERJS);
    assert_eq!(report.files.len(), 2);
    let app = file(&report, "src/App.svelte");
    assert_eq!(app.mutants.len(), 23);
    // (13 killed + 3 timeout) / (16 + 3 survived + 4 no coverage)
    assert_eq!(app.score, Some(69.57));
    let main = file(&report, "src/main.js");
    assert_eq!(main.mutants.len(), 2);
    assert_eq!(main.score, Some(0.0));
}

#[test]
fn strykerjs_survived_mutant_keeps_covered_by() {
    let report = parse(STRYKERJS);
    let app = file(&report, "src/App.svelte");
    let survived = app.mutants.iter().find(|m| m.id == "5").unwrap();
    assert_eq!(survived.status, MutantStatus::Survived);
    assert_eq!(
        survived.covered_by.as_deref(),
        Some(&["0".to_string(), "1".to_string(), "3".to_string()][..])
    );
    assert_eq!(survived.killed_by, None);
    assert!(survived.replacement.is_some());
    assert!(survived.location.start.line >= 1);
}

#[test]
fn strykerjs_tests_are_flattened_from_test_files() {
    let report = parse(STRYKERJS);
    assert_eq!(report.tests.len(), 5);
    assert_eq!(report.tests[0].id, "0");
    assert_eq!(report.tests[0].name, "App.svelte says 'hello world!'");
    assert_eq!(report.tests[0].location, None);
}

// ============================================================================
// Infection (PHP), numeric schemaVersion 1, no testFiles
// ============================================================================

#[test]
fn infection_numeric_schema_version_becomes_a_string() {
    let report = parse(INFECTION);
    assert_eq!(report.schema_version, "1");
    assert_eq!((report.thresholds.high, report.thresholds.low), (80, 20));
    assert!(report.tests.is_empty());
}

#[test]
fn infection_counts_and_score_match_its_own_mutation_score() {
    let report = parse(INFECTION);
    // Infection writes "mutationScore": 53.84615384615385 at the root.
    assert_eq!(report.summary.total, 13);
    assert_eq!(report.summary.killed, 7);
    assert_eq!(report.summary.no_coverage, 6);
    assert_eq!(report.summary.survived, 0);
    assert_eq!(report.summary.score, Some(53.85));
    assert_eq!(report.files.len(), 1);
    // Paths are the files keys as given, absolute here.
    let f = file(
        &report,
        "/Users/tfidry/Project/Humbug/infection/src/Logger/TextFileLogger.php",
    );
    assert_eq!(f.score, Some(53.85));
    assert_eq!(f.mutants.iter().filter(|m| m.description.is_some()).count(), 12);
}

// ============================================================================
// cargo-mutants, schemaVersion "2"
// ============================================================================

#[test]
fn cargo_mutants_schema_v2_counts_and_scores() {
    let report = parse(CARGO_MUTANTS);
    assert_eq!(report.schema_version, "2");
    assert_eq!(
        report.summary,
        MutationSummary {
            total: 29,
            killed: 19,
            survived: 2,
            compile_error: 8,
            score: Some(90.48),
            ..MutationSummary::default()
        }
    );
    assert_eq!(file(&report, "url/src/origin.rs").score, Some(87.5));
    assert_eq!(file(&report, "url/src/path_segments.rs").score, Some(92.31));
    assert_eq!(report.tests.len(), 133);
}

#[test]
fn cargo_mutants_survived_mutant_has_empty_killed_by() {
    let report = parse(CARGO_MUTANTS);
    let origin = file(&report, "url/src/origin.rs");
    let survived = origin
        .mutants
        .iter()
        .find(|m| m.id == "url/src/origin.rs:71:9:71:43:FnValue:false")
        .unwrap();
    assert_eq!(survived.status, MutantStatus::Survived);
    assert_eq!(survived.killed_by.as_deref(), Some(&[][..]));
    assert_eq!(survived.covered_by, None);
}

// ============================================================================
// Scores and errors
// ============================================================================

#[test]
fn score_is_null_when_nothing_is_valid_to_score() {
    let content = r#"{"schemaVersion":"2","thresholds":{"high":80,"low":60},"files":{
        "a.js":{"language":"javascript","source":"","mutants":[
            {"id":"1","mutatorName":"X","status":"CompileError","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}},
            {"id":"2","mutatorName":"X","status":"Ignored","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}}
        ]},
        "b.js":{"language":"javascript","source":"","mutants":[]}
    }}"#;
    let ParseResult::Mutation(report) = StrykerParser.parse(content, "m.json").unwrap() else {
        panic!("Expected Mutation");
    };
    assert_eq!(report.summary.score, None);
    assert_eq!(report.summary.compile_error, 1);
    assert_eq!(report.summary.ignored, 1);
    assert_eq!(report.files[1].score, None);
    let json: serde_json::Value = serde_json::to_value(&report).unwrap();
    assert!(json["summary"]["score"].is_null());
    assert!(json["files"][1]["score"].is_null());
}

#[test]
fn unknown_status_is_a_parse_error() {
    let content = r#"{"schemaVersion":"1","thresholds":{"high":80,"low":60},"files":{"a.js":{"language":"javascript","source":"","mutants":[
        {"id":"1","mutatorName":"X","status":"Exploded","location":{"start":{"line":1,"column":1},"end":{"line":1,"column":2}}}]}}}"#;
    let err = StrykerParser.parse(content, "m.json").unwrap_err();
    assert!(err.message.contains("Invalid Stryker mutation report"), "{}", err.message);
}

// ============================================================================
// WASM surface
// ============================================================================

#[test]
fn parse_mutation_returns_the_contract_json() {
    let json: serde_json::Value =
        serde_json::from_str(&parse_mutation(&load_fixture(STRYKERJS), STRYKERJS).unwrap())
            .unwrap();
    assert_eq!(json["format"], "stryker");
    assert_eq!(json["schemaVersion"], "1.0");
    assert_eq!(json["thresholds"]["high"], 80);
    assert_eq!(json["summary"]["noCoverage"], 6);
    assert_eq!(json["summary"]["compileError"], 0);
    assert_eq!(json["summary"]["runtimeError"], 0);
    assert_eq!(json["summary"]["score"], 64.0);
    let app = &json["files"][0];
    assert_eq!(app["path"], "src/App.svelte");
    assert_eq!(app["score"], 69.57);
    let survived = app["mutants"]
        .as_array()
        .unwrap()
        .iter()
        .find(|m| m["id"] == "5")
        .unwrap();
    assert_eq!(survived["status"], "Survived");
    assert!(survived["mutatorName"].is_string());
    assert!(survived["location"]["start"]["line"].is_u64());
    assert!(survived["location"]["end"]["column"].is_u64());
    assert_eq!(survived["coveredBy"], serde_json::json!(["0", "1", "3"]));
    // Optional fields are omitted, not null.
    assert!(survived.get("killedBy").is_none());
    assert_eq!(json["tests"][0]["id"], "0");
    assert!(json["tests"][0].get("location").is_none());
}

#[test]
fn parse_report_rejects_a_mutation_file() {
    let err = parse_report(&load_fixture(STRYKERJS), STRYKERJS).unwrap_err();
    assert_eq!(err, "File detected as mutation format, use parse_mutation()");
}

#[test]
fn parse_coverage_rejects_a_mutation_file() {
    let err = parse_coverage(&load_fixture(INFECTION), INFECTION).unwrap_err();
    assert_eq!(err, "Not a coverage format");
}

#[test]
fn parse_mutation_rejects_other_formats() {
    let lcov = "TN:\nSF:src/file.ts\nDA:1,1\nLF:1\nLH:1\nend_of_record\n";
    assert_eq!(parse_mutation(lcov, "lcov.info").unwrap_err(), "Not a mutation format");
}

// ============================================================================
// Stryker4s (Scala), schemaVersion "1", no testFiles, no coveredBy/killedBy
// ============================================================================

#[test]
fn stryker4s_counts_and_scores() {
    let report = parse(STRYKER4S);
    assert_eq!(report.schema_version, "1");
    assert_eq!(report.files.len(), 32);
    assert_eq!(
        report.summary,
        MutationSummary {
            total: 231,
            killed: 162,
            survived: 69,
            score: Some(70.13),
            ..MutationSummary::default()
        }
    );
    assert_eq!(file(&report, "src/main/scala/stryker4s/Stryker4s.scala").score, Some(62.5));
    assert_eq!(
        file(&report, "src/main/scala/stryker4s/mutants/Mutator.scala").score,
        Some(90.63)
    );
    assert_eq!(
        file(&report, "src/main/scala/stryker4s/report/HtmlReporter.scala").score,
        Some(0.0)
    );
    assert!(report.tests.is_empty());
}

#[test]
fn stryker4s_survived_mutant_without_test_links() {
    let report = parse(STRYKER4S);
    let f = file(&report, "src/main/scala/stryker4s/Stryker4s.scala");
    let survived = f.mutants.iter().find(|m| m.id == "228").unwrap();
    assert_eq!(survived.status, MutantStatus::Survived);
    assert_eq!(survived.mutator_name, "EqualityOperator");
    assert_eq!(survived.replacement.as_deref(), Some(">"));
    assert_eq!((survived.location.start.line, survived.location.start.column), (36, 50));
    assert_eq!((survived.location.end.line, survived.location.end.column), (36, 52));
    assert_eq!(survived.covered_by, None);
    assert_eq!(survived.killed_by, None);
}
