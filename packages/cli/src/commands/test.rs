//! `gaffer test -- <cmd>` — spawn child process, parse artifacts, store, analyze, sync.

use std::process::Command;

use anyhow::{Context, Result};
use gaffer_core::parsers;
use gaffer_core::types::{CoverageSummary, GafferConfig, RunMetadata, RunSummary, TestEvent};
use gaffer_core::GafferCore;

use crate::config::Config;
use crate::discovery;
use crate::git;
use crate::output::summary;

/// Returns true if GAFFER_DEBUG=1 (or any truthy value) is set.
fn is_debug() -> bool {
    std::env::var("GAFFER_DEBUG").is_ok_and(|v| !v.is_empty() && v != "0")
}

/// Print a debug timing message (only when GAFFER_DEBUG is set).
macro_rules! debug_timing {
    ($($arg:tt)*) => {
        if is_debug() {
            eprintln!($($arg)*);
        }
    };
}

/// Run the test command: spawn child, parse reports, store, analyze, print, sync.
pub fn run(config: &Config, command: &[String], explicit_reports: &[String]) -> Result<i32> {
    // 1. Detect git metadata
    let branch = git::detect_branch();
    let commit = git::detect_commit();
    let ci_provider = git::detect_ci_provider();

    // 2. Initialize gaffer-core
    let core = GafferCore::new(GafferConfig {
        token: config.token.clone(),
        api_url: config.api_url.clone(),
        project_root: config.project_root.to_string_lossy().to_string(),
    })
    .context("Failed to initialize gaffer")?;

    // 3. Create test run
    let run_id = core
        .start_run(RunMetadata {
            branch,
            commit,
            ci_provider,
            framework: "cli".to_string(),
        })
        .context("Failed to start run")?;

    // 4. Spawn child process (inherit stdout/stderr)
    let run_start = std::time::SystemTime::now();
    let start = std::time::Instant::now();
    let exit_code = spawn_child(command)?;
    let duration_ms = start.elapsed().as_millis() as f64;

    // 5. Discover report files (only files written during this run)
    let post_test_start = std::time::Instant::now();
    let patterns = if !explicit_reports.is_empty() {
        explicit_reports.to_vec()
    } else {
        config.report_patterns.clone()
    };
    let report_files = discovery::discover_reports(&config.project_root, &patterns, run_start);
    debug_timing!("[gaffer] Discovery: {} files in {:.1}ms", report_files.len(), post_test_start.elapsed().as_secs_f64() * 1000.0);

    // 6. Parse reports: separate test reports from coverage files
    let parse_start = std::time::Instant::now();
    let mut all_tests: Vec<TestEvent> = Vec::new();
    let mut framework = "unknown".to_string();
    let mut coverage_file: Option<(String, String)> = None; // (content, filename)
    let mut coverage_summary: Option<CoverageSummary> = None;

    for path in &report_files {
        let content = match std::fs::read_to_string(path) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[gaffer] Warning: could not read {}: {}", path.display(), e);
                continue;
            }
        };

        let result_type = parsers::detect_result_type(path, &content);

        match result_type {
            Some(parsers::ResultType::Coverage) => {
                let filename = path.file_name().and_then(|f| f.to_str()).unwrap_or("coverage").to_string();
                if coverage_file.is_some() {
                    eprintln!(
                        "[gaffer] Warning: multiple coverage files found. Using '{}', ignoring previous. \
                         To use a specific file, pass --report <path>.",
                        path.display()
                    );
                }
                coverage_file = Some((content, filename));
            }
            Some(parsers::ResultType::TestReport) => {
                match parsers::parse_report(path, &content) {
                    Ok(report) => {
                        if framework == "unknown" {
                            framework = report.framework.clone();
                        }
                        all_tests.extend(report.tests);
                    }
                    Err(e) => {
                        eprintln!("[gaffer] Warning: could not parse {}: {}", path.display(), e);
                    }
                }
            }
            None => {
                debug_timing!("[gaffer] Skipping unrecognized file: {}", path.display());
            }
        }
    }
    debug_timing!("[gaffer] Parse: {} tests from {} files in {:.1}ms", all_tests.len(), report_files.len(), parse_start.elapsed().as_secs_f64() * 1000.0);

    // 7. Update framework if detected from reports
    if framework != "unknown" {
        if let Err(e) = core.update_framework(&run_id, &framework) {
            eprintln!("[gaffer] Warning: failed to update framework: {}", e);
        }
    }

    // 8. Record tests
    let record_start = std::time::Instant::now();
    for test in &all_tests {
        if let Err(e) = core.record_test(&run_id, test) {
            eprintln!("[gaffer] Warning: failed to record test: {}", e);
        }
    }
    debug_timing!("[gaffer] Record tests: {} inserts in {:.1}ms", all_tests.len(), record_start.elapsed().as_secs_f64() * 1000.0);

    // 9. Record coverage if present
    let cov_start = std::time::Instant::now();
    if let Some((content, filename)) = &coverage_file {
        match core.record_coverage(&run_id, content, filename) {
            Ok(cov) => coverage_summary = Some(cov),
            Err(e) => eprintln!("[gaffer] Warning: failed to parse coverage: {}", e),
        }
    }
    if coverage_file.is_some() {
        debug_timing!("[gaffer] Coverage: {:.1}ms", cov_start.elapsed().as_secs_f64() * 1000.0);
    }

    // 10. Build summary and finalize
    let passed = all_tests.iter().filter(|t| t.status == "passed").count() as i32;
    let failed = all_tests.iter().filter(|t| t.status == "failed").count() as i32;
    let skipped = all_tests
        .iter()
        .filter(|t| t.status != "passed" && t.status != "failed")
        .count() as i32;
    let run_summary = RunSummary {
        total: all_tests.len() as i32,
        passed,
        failed,
        skipped,
        duration: duration_ms,
    };

    let end_run_start = std::time::Instant::now();
    let report = core
        .end_run(&run_id, &run_summary)
        .context("Failed to end run")?;
    debug_timing!("[gaffer] End run (finalize + intelligence + payload): {:.1}ms", end_run_start.elapsed().as_secs_f64() * 1000.0);

    let sync_result = match core.sync() {
        Ok(result) => Some(result),
        Err(e) => {
            eprintln!("[gaffer] Warning: sync failed: {}", e);
            None
        }
    };

    debug_timing!("[gaffer] Total post-test: {:.1}ms", post_test_start.elapsed().as_secs_f64() * 1000.0);

    summary::print_report(&report, coverage_summary.as_ref(), sync_result.as_ref());

    if report_files.is_empty() {
        eprintln!(
            "\n[gaffer] No report files found. Configure patterns in gaffer.toml or use --report.\n\
             \n\
             [test]\n\
             report_patterns = [\"**/test-reports/**/*.xml\", \"**/coverage/lcov.info\"]\n\
             \n\
             Or use: gaffer test --report path/to/junit.xml -- <command>"
        );
    }

    // 11. Exit with child process exit code
    Ok(exit_code)
}

/// Spawn a child process, inheriting stdout/stderr.
fn spawn_child(command: &[String]) -> Result<i32> {
    if command.is_empty() {
        anyhow::bail!("No command provided. Usage: gaffer test -- <command>");
    }

    let status = Command::new(&command[0])
        .args(&command[1..])
        .status()
        .context(format!("Failed to spawn '{}'", command[0]))?;

    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            eprintln!("[gaffer] Warning: test process killed by signal {}", signal);
            return Ok(128 + signal);
        }
    }

    Ok(status.code().unwrap_or(1))
}
