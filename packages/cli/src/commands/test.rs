//! `gaffer test -- <cmd>` — spawn child process, parse artifacts, store, analyze, sync.

use std::path::PathBuf;
use std::process::Command;

use anyhow::{Context, Result};
use gaffer_core::parsers;
use gaffer_core::types::{CoverageSummary, GafferConfig, RunMetadata, RunSummary, TestEvent, status};
use gaffer_core::GafferCore;

use crate::config::Config;
use crate::discovery;
use crate::git;
use crate::output::{json, summary};
use crate::{FailOn, OutputFormat};

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
pub fn run(config: &Config, command: &[String], explicit_reports: &[String], format: &OutputFormat, show_errors: bool, compare: Option<&str>, fail_on: Option<&FailOn>) -> Result<i32> {
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

    // 4. Spawn child process
    // In JSON mode, redirect child stdout to stderr so only gaffer's JSON goes to stdout.
    let json_mode = matches!(format, OutputFormat::Json);
    let run_start = std::time::SystemTime::now();
    let start = std::time::Instant::now();
    let exit_code = spawn_child(command, json_mode)?;
    let duration_ms = start.elapsed().as_millis() as f64;

    // 5. Discover report files (only files written during this run)
    // When --report is passed with literal file paths (not globs), resolve them directly.
    // Otherwise, use glob-based discovery with freshness filtering.
    let post_test_start = std::time::Instant::now();
    let report_files = if !explicit_reports.is_empty() {
        resolve_explicit_reports(explicit_reports, &config.project_root, run_start)
    } else {
        discovery::discover_reports(&config.project_root, &config.report_patterns, run_start)
    };
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

    // 10. Collect failures (always needed for JSON; used by --show-errors for human output)
    let failures: Vec<&TestEvent> = all_tests.iter().filter(|t| t.status == status::FAILED).collect();

    let context_files: Vec<PathBuf> = if show_errors && !failures.is_empty() {
        discovery::discover_context_files(&config.project_root, run_start, &report_files)
    } else {
        Vec::new()
    };

    // 11. Build summary and finalize
    let passed = all_tests.iter().filter(|t| t.status == status::PASSED).count() as i32;
    let failed = failures.len() as i32;
    let skipped = all_tests.len() as i32 - passed - failed;
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

    // 13. Compare against baseline branch if --compare is set.
    // Comparison is non-critical — the user's test results, sync, and exit code are all
    // independent. All error paths degrade to a warning + skip so the run completes normally.
    let comparison = if let Some(baseline_branch) = compare {
        match core.compare_run(&run_id, &run_summary, baseline_branch) {
            Ok(Some(result)) => Some(result),
            Ok(None) => {
                eprintln!(
                    "[gaffer] Warning: no runs found on '{}'. Run tests on that branch first.",
                    baseline_branch
                );
                None
            }
            Err(e) => {
                eprintln!("[gaffer] Warning: comparison failed: {}", e);
                None
            }
        }
    } else {
        None
    };

    // 14. Classify failures against baseline branch
    let classify_start = std::time::Instant::now();
    let baseline_branch = compare
        .map(|s| s.to_string())
        .or_else(|| git::detect_default_branch());
    let classification = core.classify_failures(
        &run_id,
        baseline_branch.as_deref(),
        &all_tests,
    );
    debug_timing!("[gaffer] Classification: {:.1}ms", classify_start.elapsed().as_secs_f64() * 1000.0);

    debug_timing!("[gaffer] Total post-test: {:.1}ms", post_test_start.elapsed().as_secs_f64() * 1000.0);

    match format {
        OutputFormat::Human => {
            let error_failures = if show_errors { &failures[..] } else { &[] };
            summary::print_report(&report, error_failures, &context_files, coverage_summary.as_ref(), sync_result.as_ref(), comparison.as_ref(), &classification);
        }
        OutputFormat::Json => {
            if let Err(e) = json::print_json(&report, &failures, &context_files, coverage_summary.as_ref(), sync_result.as_ref(), comparison.as_ref(), &classification) {
                eprintln!("[gaffer] Error: failed to serialize JSON output: {}", e);
            }
        }
    }

    if report_files.is_empty() {
        if !explicit_reports.is_empty() {
            eprintln!(
                "\n[gaffer] No report files found at the specified --report path(s). \
                 Check that the path is correct and the file exists after your test command runs."
            );
        } else {
            eprintln!(
                "\n[gaffer] No report files found. Configure patterns in gaffer.toml or use --report.\n\
                 \n\
                 [test]\n\
                 report_patterns = [\"**/test-reports/**/*.xml\", \"**/coverage/lcov.info\"]\n\
                 \n\
                 Or use: gaffer test --report path/to/junit.xml -- <command>"
            );
        }
    }

    // 15. Determine exit code — --fail-on overrides child exit code
    let final_exit_code = if let Some(FailOn::New) = fail_on {
        use gaffer_core::types::FailureClassification;
        let has_new = classification.classified_failures.iter().any(|f| {
            matches!(f.classification, FailureClassification::New | FailureClassification::Unknown)
        });
        if has_new {
            exit_code.max(1) // ensure non-zero
        } else {
            0 // all failures are pre_existing or flaky — not the developer's fault
        }
    } else {
        exit_code
    };

    Ok(final_exit_code)
}

/// Spawn a child process. When `redirect_stdout` is true, the child's stdout
/// is sent to stderr so that only gaffer's JSON output occupies stdout.
fn spawn_child(command: &[String], redirect_stdout: bool) -> Result<i32> {
    if command.is_empty() {
        anyhow::bail!("No command provided. Usage: gaffer test -- <command>");
    }

    let mut cmd = Command::new(&command[0]);
    cmd.args(&command[1..]);

    if redirect_stdout {
        cmd.stdout(std::process::Stdio::from(std::io::stderr()));
        debug_timing!("[gaffer] JSON mode: child stdout redirected to stderr");
    }

    let status = cmd
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

/// Resolve explicit --report paths. Handles both literal file paths and glob patterns.
/// Literal paths are resolved directly (absolute or relative to project root).
/// Glob patterns (containing * or ?) are passed through glob-based discovery.
fn resolve_explicit_reports(
    reports: &[String],
    project_root: &std::path::Path,
    not_before: std::time::SystemTime,
) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut glob_patterns = Vec::new();

    for report in reports {
        let is_glob = report.contains('*') || report.contains('?');
        if is_glob {
            glob_patterns.push(report.clone());
        } else {
            // Treat as literal file path
            let path = std::path::Path::new(report);
            let resolved = if path.is_absolute() {
                path.to_path_buf()
            } else {
                project_root.join(path)
            };
            if resolved.is_file() {
                files.push(resolved);
            } else {
                eprintln!(
                    "[gaffer] Warning: report file not found: {}",
                    resolved.display()
                );
            }
        }
    }

    // Any glob patterns go through the standard discovery path
    if !glob_patterns.is_empty() {
        files.extend(discovery::discover_reports(project_root, &glob_patterns, not_before));
    }

    files
}
