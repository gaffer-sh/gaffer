//! `gaffer affected-tests --files <paths>` — map changed source files to relevant test specs.

use std::path::Path;

use anyhow::Result;
use gaffer_core::affected::{self, NoHistoryProvider};
use gaffer_core::types::{AffectedTestsResult, AffectedTestsSignalCoverage};

use crate::framework;
use crate::OutputFormat;

/// Compute affected tests + run command without printing.
///
/// Used by the standalone CLI command. For the integrated `gaffer test --affected`
/// path, use [`compute_with_argv`] instead: it returns the same result plus the
/// pre-tokenized argv so the spawn path doesn't have to round-trip through a
/// shell.
///
/// When `graph` is true, the import-graph strategy runs. By default the
/// graph is persisted at `<project>/.gaffer/graph.db` and incrementally
/// updated on subsequent calls (~10–50× faster than rebuilding from
/// scratch). Setting `no_cache` to true forces an in-memory build —
/// useful for ephemeral CI runs or read-only filesystems where writing
/// to `.gaffer/` is undesirable. Cache failures (corrupt DB, permission
/// denied) automatically degrade to in-memory with a stderr warning;
/// the user always gets a result.
///
/// History-derived signals (coverage_history, failure_history) require a
/// `HistorySignalProvider`. The CLI currently threads `NoHistoryProvider`,
/// which reports both signals as unavailable; callers (and agents) read
/// `signals.unavailable` to tell whether the run was degraded.
pub fn compute(
    project_root: &Path,
    files: &[String],
    graph: bool,
    no_cache: bool,
) -> AffectedTestsResult {
    compute_with_argv(project_root, files, graph, no_cache).0
}

/// Like [`compute`] but also returns the argv form of the `run_command`.
/// The argv is `Some` whenever `result.run_command` is `Some` and references
/// the same affected tests; integrated callers should spawn argv directly
/// rather than parsing the string back into tokens through a shell.
pub fn compute_with_argv(
    project_root: &Path,
    files: &[String],
    graph: bool,
    no_cache: bool,
) -> (AffectedTestsResult, Option<Vec<String>>) {
    let (affected, signals) = if graph {
        run_graph_strategy(project_root, files, no_cache)
    } else {
        run_heuristic_strategy(project_root, files)
    };

    let frameworks = framework::detect_frameworks(project_root);
    let (argv, framework_name) = build_run_argv(project_root, &frameworks, &affected);
    let run_command = argv.as_ref().map(|a| a.join(" "));

    let result = AffectedTestsResult {
        affected,
        run_command,
        framework: framework_name,
        signals,
    };
    (result, argv)
}

/// Run the affected-tests command: compute results, print JSON / human / cmd-only.
///
/// `print_cmd` short-circuits all other output: stdout gets the bare `run_command`
/// string when one is available, and the process exits 1 when no command is
/// available so `gaffer test -- $(gaffer affected-tests --files X --print-cmd)`
/// naturally fails fast rather than spawning gaffer-test with an empty wrapped
/// command.
pub fn run(
    project_root: &Path,
    files: &[String],
    format: &OutputFormat,
    graph: bool,
    no_cache: bool,
    print_cmd: bool,
) -> Result<i32> {
    let result = compute(project_root, files, graph, no_cache);

    if print_cmd {
        match &result.run_command {
            Some(cmd) => {
                println!("{}", cmd);
                return Ok(0);
            }
            None => {
                if result.signals.unavailable.is_empty() {
                    eprintln!("[gaffer] No affected tests for the given files.");
                } else {
                    eprintln!(
                        "[gaffer] No affected tests, but signals were unavailable: {}. \
                         Consider escalating scope.",
                        result.signals.unavailable.join(", ")
                    );
                }
                return Ok(1);
            }
        }
    }

    match format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&result)?;
            println!("{}", json);
        }
        OutputFormat::Human => {
            print_human(&result);
        }
    }

    Ok(0)
}

/// Heuristic mode: naming + proximity, no graph build. Wraps the legacy
/// `find_affected_tests` so we can tack on the history-mode signal
/// coverage uniformly.
fn run_heuristic_strategy(
    project_root: &Path,
    files: &[String],
) -> (Vec<gaffer_core::types::AffectedTest>, AffectedTestsSignalCoverage) {
    let affected = affected::find_affected_tests(project_root, files);
    let signals = AffectedTestsSignalCoverage {
        attempted: vec!["naming_convention".into(), "directory_proximity".into()],
        unavailable: vec!["import_graph".into(), "coverage_history".into(), "failure_history".into()],
    };
    (affected, signals)
}

/// Run the graph strategy with cache-first / in-memory fallback. Cache
/// errors are non-fatal: we log to stderr and rebuild from scratch.
/// Threads `NoHistoryProvider` through `find_affected_tests_with_history`
/// so the JSON output carries `signals.unavailable: [coverage_history,
/// failure_history]` — telling agent callers explicitly that the run is
/// in graph-only / degraded mode.
fn run_graph_strategy(
    project_root: &Path,
    files: &[String],
    no_cache: bool,
) -> (Vec<gaffer_core::types::AffectedTest>, AffectedTestsSignalCoverage) {
    if no_cache {
        return run_graph_in_memory(project_root, files);
    }
    let cache_db = project_root.join(".gaffer").join("graph.db");
    match affected::find_affected_tests_with_graph_cached(project_root, files, &cache_db) {
        Ok(affected) => {
            // The cached entry point returns only `affected`; reconstruct the matching
            // signal coverage here so JSON consumers see a consistent shape across
            // graph-cached, graph-rebuilt, and heuristic-only paths.
            let signals = AffectedTestsSignalCoverage {
                attempted: vec![
                    "naming_convention".into(),
                    "directory_proximity".into(),
                    "import_graph".into(),
                ],
                unavailable: vec!["coverage_history".into(), "failure_history".into()],
            };
            (affected, signals)
        }
        Err(e) => {
            eprintln!(
                "[gaffer] graph cache unavailable ({}); falling back to in-memory build. \
                 Pass --no-cache to silence this warning.",
                e
            );
            run_graph_in_memory(project_root, files)
        }
    }
}

/// In-memory graph build via the history-aware entry point. `NoHistoryProvider`
/// marks `coverage_history` / `failure_history` as unavailable, which is what
/// the CLI wants today (no Gaffer connection threaded through).
fn run_graph_in_memory(
    project_root: &Path,
    files: &[String],
) -> (Vec<gaffer_core::types::AffectedTest>, AffectedTestsSignalCoverage) {
    let run = affected::find_affected_tests_with_history(
        project_root,
        files,
        &NoHistoryProvider,
        /* use_graph */ true,
    );
    (run.affected, run.signals)
}

/// Build the argv (program + args, one element per token) for the detected framework
/// and affected test files. Used by both the JSON-formatted `run_command` string
/// (for display / external consumers) and the integrated `gaffer test --affected`
/// spawn path (no shell parsing, so paths with spaces or shell metacharacters are
/// passed through verbatim).
pub(crate) fn build_run_argv(
    project_root: &Path,
    frameworks: &[framework::Framework],
    affected: &[gaffer_core::types::AffectedTest],
) -> (Option<Vec<String>>, Option<String>) {
    if affected.is_empty() || frameworks.is_empty() {
        return (None, frameworks.first().map(|f| f.to_string()));
    }

    let test_files: Vec<String> = affected.iter().map(|a| a.test_file.clone()).collect();
    let fw = &frameworks[0];
    let pkg_mgr = affected::detect_package_manager(project_root).to_string();

    let (mut argv, name): (Vec<String>, &str) = match fw {
        framework::Framework::Vitest(_) => (vec![pkg_mgr, "vitest".into()], "vitest"),
        framework::Framework::Playwright(_) => {
            (vec![pkg_mgr, "playwright".into(), "test".into()], "playwright")
        }
        framework::Framework::Jest(_) => (vec![pkg_mgr, "jest".into()], "jest"),
        framework::Framework::Mocha(_) => (vec![pkg_mgr, "mocha".into()], "mocha"),
        framework::Framework::Pytest(_) => (vec!["pytest".into()], "pytest"),
        // Go needs package paths, not file paths — best effort.
        framework::Framework::Go => (vec!["go".into(), "test".into()], "go"),
        framework::Framework::Rspec => (vec!["rspec".into()], "rspec"),
        framework::Framework::DotNet(_) => {
            (vec!["dotnet".into(), "test".into(), "--filter".into()], "dotnet")
        }
        framework::Framework::CargoTest => (vec!["cargo".into(), "test".into()], "cargo"),
        framework::Framework::PHPUnit(_) => (vec!["phpunit".into()], "phpunit"),
    };

    argv.extend(test_files);
    (Some(argv), Some(name.to_string()))
}


fn print_human(result: &AffectedTestsResult) {
    if result.affected.is_empty() {
        eprintln!("No affected test files found.");
        return;
    }

    eprintln!("Affected tests: {} files", result.affected.len());
    eprintln!();

    for test in &result.affected {
        eprintln!(
            "  {:.0}%  {}  ({})",
            test.confidence * 100.0,
            test.test_file,
            test.strategy,
        );
    }

    if let Some(cmd) = &result.run_command {
        eprintln!();
        eprintln!("Run: {}", cmd);
    }
}
