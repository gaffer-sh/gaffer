//! `gaffer affected-tests --files <paths>` — map changed source files to relevant test specs.

use std::path::Path;

use anyhow::Result;
use gaffer_core::affected;
use gaffer_core::types::AffectedTestsResult;

use crate::framework;
use crate::OutputFormat;

/// Run the affected-tests command: scan for test files, generate run command.
pub fn run(project_root: &Path, files: &[String], format: &OutputFormat) -> Result<()> {
    let affected = affected::find_affected_tests(project_root, files);

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
