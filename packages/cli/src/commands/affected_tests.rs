//! `gaffer affected-tests --files <paths>` — map changed source files to relevant test specs.

use std::path::Path;

use anyhow::Result;
use gaffer_core::affected;
use gaffer_core::types::AffectedTestsResult;

use crate::framework;
use crate::OutputFormat;

/// Run the affected-tests command: scan for test files, generate run command.
///
/// When `graph` is true, the import-graph strategy runs. By default the
/// graph is persisted at `<project>/.gaffer/graph.db` and incrementally
/// updated on subsequent calls (~10–50× faster than rebuilding from
/// scratch). Setting `no_cache` to true forces an in-memory build —
/// useful for ephemeral CI runs or read-only filesystems where writing
/// to `.gaffer/` is undesirable. Cache failures (corrupt DB, permission
/// denied) automatically degrade to in-memory with a stderr warning;
/// the user always gets a result.
pub fn run(
    project_root: &Path,
    files: &[String],
    format: &OutputFormat,
    graph: bool,
    no_cache: bool,
) -> Result<()> {
    let affected = if graph {
        run_graph_strategy(project_root, files, no_cache)
    } else {
        affected::find_affected_tests(project_root, files)
    };

    // Detect framework for run command generation
    let frameworks = framework::detect_frameworks(project_root);
    let (run_command, framework_name) = generate_run_command(project_root, &frameworks, &affected);

    let result = AffectedTestsResult {
        affected,
        run_command,
        framework: framework_name,
    };

    match format {
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&result)?;
            println!("{}", json);
        }
        OutputFormat::Human => {
            print_human(&result);
        }
    }

    Ok(())
}

/// Run the graph strategy with cache-first / in-memory fallback. Cache
/// errors are non-fatal: we log to stderr and rebuild from scratch.
fn run_graph_strategy(
    project_root: &Path,
    files: &[String],
    no_cache: bool,
) -> Vec<gaffer_core::types::AffectedTest> {
    if no_cache {
        return affected::find_affected_tests_with_graph(project_root, files);
    }
    let cache_db = project_root.join(".gaffer").join("graph.db");
    match affected::find_affected_tests_with_graph_cached(project_root, files, &cache_db) {
        Ok(result) => result,
        Err(e) => {
            eprintln!(
                "[gaffer] graph cache unavailable ({}); falling back to in-memory build. \
                 Pass --no-cache to silence this warning.",
                e
            );
            affected::find_affected_tests_with_graph(project_root, files)
        }
    }
}

/// Generate a run command for the detected framework and affected test files.
fn generate_run_command(
    project_root: &Path,
    frameworks: &[framework::Framework],
    affected: &[gaffer_core::types::AffectedTest],
) -> (Option<String>, Option<String>) {
    if affected.is_empty() || frameworks.is_empty() {
        return (None, frameworks.first().map(|f| f.to_string()));
    }

    let test_files: Vec<&str> = affected.iter().map(|a| a.test_file.as_str()).collect();
    let files_arg = test_files.join(" ");

    let fw = &frameworks[0];
    let pkg_mgr = affected::detect_package_manager(project_root);

    let (cmd, name) = match fw {
        framework::Framework::Vitest(_) => {
            (format!("{} vitest {}", pkg_mgr, files_arg), "vitest".to_string())
        }
        framework::Framework::Playwright(_) => {
            (format!("{} playwright test {}", pkg_mgr, files_arg), "playwright".to_string())
        }
        framework::Framework::Jest(_) => {
            (format!("{} jest {}", pkg_mgr, files_arg), "jest".to_string())
        }
        framework::Framework::Mocha(_) => {
            (format!("{} mocha {}", pkg_mgr, files_arg), "mocha".to_string())
        }
        framework::Framework::Pytest(_) => {
            (format!("pytest {}", files_arg), "pytest".to_string())
        }
        framework::Framework::Go => {
            // Go needs package paths, not file paths — best effort
            (format!("go test {}", files_arg), "go".to_string())
        }
        framework::Framework::Rspec => {
            (format!("rspec {}", files_arg), "rspec".to_string())
        }
        framework::Framework::DotNet(_) => {
            (format!("dotnet test --filter {}", files_arg), "dotnet".to_string())
        }
        framework::Framework::CargoTest => {
            (format!("cargo test {}", files_arg), "cargo".to_string())
        }
        framework::Framework::PHPUnit(_) => {
            (format!("phpunit {}", files_arg), "phpunit".to_string())
        }
    };

    (Some(cmd), Some(name))
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
