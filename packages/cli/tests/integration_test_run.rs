//! Integration tests for the local run lifecycle of `gaffer test`: every exit path
//! after `start_run` must leave the run in a terminal status, never `running`.

use std::process::Command;

use gaffer_core::db::Database;
use gaffer_core::types::RunStatus;

#[test]
fn spawn_failure_ends_the_run_as_failed() {
    let dir = tempfile::TempDir::new().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_gaffer"))
        .current_dir(dir.path())
        .env_remove("GAFFER_TOKEN")
        .args(["test", "--data-dir"])
        .arg(dir.path())
        .args(["--", "gaffer-no-such-command-gaf-554"])
        .output()
        .expect("failed to spawn gaffer");

    assert!(!output.status.success(), "expected non-zero exit");
    let db = Database::open(&dir.path().join(".gaffer").join("data.db")).unwrap();
    assert_eq!(db.get_run_count_by_status(RunStatus::Failed).unwrap(), 1);
    assert_eq!(db.get_run_count_by_status(RunStatus::Running).unwrap(), 0);
}

#[test]
fn a_run_with_no_token_says_results_were_not_uploaded() {
    let dir = tempfile::TempDir::new().unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gaffer"));
    for var in [
        "GAFFER_PROJECT_TOKEN",
        "GAFFER_UPLOAD_TOKEN",
        "GAFFER_TOKEN",
        "ACTIONS_ID_TOKEN_REQUEST_URL",
        "ACTIONS_ID_TOKEN_REQUEST_TOKEN",
    ] {
        cmd.env_remove(var);
    }
    let output = cmd
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .args(["test", "--data-dir"])
        .arg(dir.path())
        .args(["--", "true"])
        .output()
        .expect("failed to spawn gaffer");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Results were not uploaded") && stderr.contains("GAFFER_PROJECT_TOKEN"),
        "stderr was: {stderr}"
    );
}

const JUNIT_ONE_PASS: &str = r#"<?xml version="1.0"?><testsuite name="s" tests="1"><testcase name="a" classname="s" time="0.1"/></testsuite>"#;
const JEST_ONE_PASS: &str = r#"{"numTotalTests":1,"numPassedTests":1,"numFailedTests":0,"numPendingTests":0,"numTotalTestSuites":1,"numPassedTestSuites":1,"numFailedTestSuites":0,"numPendingTestSuites":0,"startTime":0,"success":true,"testResults":[{"name":"/p/a.test.js","startTime":0,"endTime":1,"assertionResults":[{"title":"a","fullName":"a","status":"passed","failureMessages":[]}]}]}"#;

/// Run `gaffer test --format json [extra] -- sh -c <script>` in a fresh git-like
/// project dir with `target/` and `TestResults/` gitignored, as stock projects do.
fn run_in_project(script: &str, extra: &[&str]) -> (serde_json::Value, String) {
    let dir = tempfile::TempDir::new().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    std::fs::write(dir.path().join(".gitignore"), "target/\nTestResults/\n").unwrap();

    let mut cmd = Command::new(env!("CARGO_BIN_EXE_gaffer"));
    for var in ["GAFFER_PROJECT_TOKEN", "GAFFER_UPLOAD_TOKEN", "GAFFER_TOKEN", "ACTIONS_ID_TOKEN_REQUEST_URL"] {
        cmd.env_remove(var);
    }
    let output = cmd
        .current_dir(dir.path())
        .env("XDG_CONFIG_HOME", dir.path())
        .args(["test", "--format", "json", "--data-dir"])
        .arg(dir.path())
        .args(extra)
        .args(["--", "sh", "-c", script])
        .output()
        .expect("failed to spawn gaffer");

    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let json = serde_json::from_slice(&output.stdout).unwrap_or_else(|e| panic!("bad JSON ({e}); stderr: {stderr}"));
    (json, stderr)
}

fn total(json: &serde_json::Value) -> i64 {
    json["summary"]["total"].as_i64().unwrap()
}

#[test]
fn finds_surefire_xml_with_no_flag() {
    let (json, stderr) = run_in_project(
        &format!("mkdir -p target/surefire-reports && echo '{JUNIT_ONE_PASS}' > target/surefire-reports/TEST-s.xml"),
        &[],
    );
    assert_eq!(total(&json), 1, "stderr: {stderr}");
}

#[test]
fn finds_dotnet_trx_with_no_flag() {
    let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/../gaffer-parsers/tests/fixtures/trx/xunit-results.trx");
    let (json, stderr) = run_in_project(&format!("mkdir -p Tests/TestResults && cp '{fixture}' Tests/TestResults/run.trx"), &[]);
    assert!(total(&json) > 0, "stderr: {stderr}");
}

#[test]
fn finds_jest_and_vitest_json_with_no_flag() {
    for name in ["jest-results.json", "vitest-results.json"] {
        let (json, stderr) = run_in_project(&format!("echo '{JEST_ONE_PASS}' > {name}"), &[]);
        assert_eq!(total(&json), 1, "{name}; stderr: {stderr}");
    }
}

#[test]
fn nothing_found_lists_the_patterns_it_checked() {
    let (_, stderr) = run_in_project("true", &[]);
    assert!(stderr.contains("**/target/surefire-reports/TEST-*.xml"), "stderr: {stderr}");
}

#[test]
fn unparseable_report_names_the_formats_and_gaffer_upload() {
    let (json, stderr) = run_in_project("echo '<html></html>' > index.html", &["--report", "index.html"]);
    assert_eq!(total(&json), 0);
    assert!(stderr.contains("JUnit XML") && stderr.contains("gaffer upload"), "stderr: {stderr}");
}

/// GAF-536 case 2: a report whose mtime predates the run is not summarised.
#[test]
fn report_older_than_the_run_is_ignored() {
    let (json, stderr) = run_in_project(
        &format!("mkdir -p test-reports && echo '{JUNIT_ONE_PASS}' > test-reports/junit.xml && touch -t 202001010000 test-reports/junit.xml"),
        &[],
    );
    assert_eq!(total(&json), 0, "stderr: {stderr}");
    assert!(stderr.contains("No report files found"), "stderr: {stderr}");
}

/// GAF-536 case 1: a discovered report that fails to parse is named after the summary,
/// not left as a bare "0 passed".
#[test]
fn discovered_report_that_fails_to_parse_is_named_after_the_summary() {
    let (json, stderr) = run_in_project(
        "mkdir -p .gaffer/reports && echo '{\"config\":{},\"suites\":[{\"title\":1}],\"stats\":{}}' > .gaffer/reports/playwright-results.json",
        &[],
    );
    assert_eq!(total(&json), 0);
    let note = stderr
        .lines()
        .find(|l| l.contains("could not be parsed"))
        .unwrap_or_else(|| panic!("no parse-failure note; stderr: {stderr}"));
    assert!(note.contains(".gaffer/reports/playwright-results.json"), "note: {note}");
    assert!(note.contains("not included in this run"), "note: {note}");
}
