//! Enriched terminal output — ANSI-colored summary of test results.

use std::path::PathBuf;

use colored::Colorize;
use gaffer_core::types::{
    ComparisonResult, CoverageSummary, DurationAnalysis, FailureSearchResult, FlakyTestResult,
    HealthScore, RecentRun, RunReport, SyncResult, TestEvent, TestHistoryEntry, TestIntelligence,
    status,
};

fn trend_arrow(trend: &str) -> &'static str {
    match trend {
        "improving" => "^",
        "declining" => "v",
        "stable" => "~",
        _ => "",
    }
}

fn plural(count: usize, word: &str) -> String {
    format!("{} {}{}", count, word, if count == 1 { "" } else { "s" })
}

/// Truncate a string at a safe UTF-8 char boundary, appending "..." if truncated.
fn truncate_ellipsis(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        return s.to_string();
    }
    let target = max_len.saturating_sub(3);
    let mut end = target;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}...", &s[..end])
}

/// Print the main summary line.
pub fn print_summary(passed: i32, failed: i32, skipped: i32, duration_secs: f64) {
    let mut parts = vec![
        "gaffer".bold().to_string(),
        format!("{} passed", passed).green().to_string(),
    ];
    if failed > 0 {
        parts.push(format!("{} failed", failed).red().to_string());
    }
    if skipped > 0 {
        parts.push(format!("{} skipped", skipped).yellow().to_string());
    }
    parts.push(format!("{:.1}s", duration_secs).dimmed().to_string());

    eprintln!("\n{}", parts.join("  "));
}

/// Print health score and duration analysis.
pub fn print_health(health: &HealthScore, intelligence: Option<&TestIntelligence>) {
    let label_colored = match health.label.as_str() {
        "excellent" | "good" => health.label.to_string().green(),
        "fair" => health.label.to_string().yellow(),
        _ => health.label.to_string().red(),
    };

    let mut line = format!(
        "Health: {} ({}) {}",
        health.score,
        label_colored,
        trend_arrow(&health.trend),
    );

    if let Some(intel) = intelligence {
        line.push_str(&format!(
            "  Slow: p95 {:.1}ms",
            intel.duration_analysis.p95,
        ));
    }

    eprintln!("{}", line);
}

/// Print flaky test entries. Shared by `print_flaky` (capped at 5) and `print_flaky_list` (all).
fn print_flaky_entries(flaky: &[FlakyTestResult], max: Option<usize>, show_score: bool) {
    let display_count = max.unwrap_or(flaky.len()).min(flaky.len());

    for t in flaky.iter().take(display_count) {
        let pct = (t.flip_rate * 100.0).round().max(0.0) as u32;
        if show_score {
            eprintln!(
                "   {} > {} — {}% flip rate ({}/{} runs), score: {:.2}",
                t.file_path, t.test_name, pct, t.flip_count, t.total_runs, t.composite_score
            );
        } else {
            eprintln!(
                "   {} > {} — {}% flip rate ({}/{} runs)",
                t.file_path, t.test_name, pct, t.flip_count, t.total_runs
            );
        }
    }

    if let Some(max) = max {
        if flaky.len() > max {
            eprintln!("   ... and {} more", flaky.len() - max);
        }
    }
}

/// Print flaky test information (used by `gaffer test` output).
pub fn print_flaky(intelligence: &TestIntelligence) {
    let flaky = &intelligence.flaky_tests;
    if flaky.is_empty() {
        return;
    }

    eprintln!("{}", format!("Flaky: {}", plural(flaky.len(), "test")).yellow());
    print_flaky_entries(flaky, Some(5), false);
}

/// Print failure cluster information.
pub fn print_clusters(intelligence: &TestIntelligence) {
    let clusters = &intelligence.failure_clusters;
    if clusters.is_empty() {
        return;
    }

    let total_tests: u32 = clusters.iter().map(|c| c.count).sum();
    eprintln!(
        "Clusters: {} ({}) ",
        plural(clusters.len(), "pattern"),
        plural(total_tests as usize, "test"),
    );

    for c in clusters.iter().take(3) {
        let pattern = truncate_ellipsis(&c.pattern, 60);
        eprintln!("   \"{}\" — {}", pattern, plural(c.count as usize, "test"));
    }

    if clusters.len() > 3 {
        eprintln!("   ... and {} more", clusters.len() - 3);
    }
}

/// Print coverage summary.
pub fn print_coverage(coverage: &CoverageSummary) {
    let pct = if coverage.lines.total > 0 {
        (coverage.lines.covered as f64 / coverage.lines.total as f64) * 100.0
    } else {
        0.0
    };

    let pct_colored = if pct >= 80.0 {
        format!("{:.1}%", pct).green()
    } else if pct >= 50.0 {
        format!("{:.1}%", pct).yellow()
    } else {
        format!("{:.1}%", pct).red()
    };

    eprintln!(
        "Coverage: {} lines ({}/{})",
        pct_colored, coverage.lines.covered, coverage.lines.total,
    );
}

/// Print sync result.
pub fn print_sync(result: &SyncResult) {
    if result.synced > 0 {
        eprintln!(
            "{}",
            format!("Synced: {} uploaded", plural(result.synced as usize, "run")).green()
        );
    }
    if result.failed > 0 {
        eprintln!(
            "{}",
            format!("Sync: {} failed (will retry)", result.failed).yellow()
        );
    }
}

// =============================================================================
// Query subcommand pretty-print functions
// =============================================================================

/// Print flaky tests list (standalone, for `gaffer query flaky`).
pub fn print_flaky_list(flaky: &[FlakyTestResult]) {
    if flaky.is_empty() {
        eprintln!("No flaky tests detected.");
        return;
    }

    eprintln!("{}", format!("Flaky: {}", plural(flaky.len(), "test")).yellow());
    print_flaky_entries(flaky, None, true);
}

/// Print duration analysis (for `gaffer query slowest`).
pub fn print_slowest(analysis: &DurationAnalysis) {
    if analysis.slowest_tests.is_empty() {
        eprintln!("No test data available.");
        return;
    }

    eprintln!(
        "Duration: p50 {:.1}ms  p95 {:.1}ms  p99 {:.1}ms  mean {:.1}ms",
        analysis.p50, analysis.p95, analysis.p99, analysis.mean
    );
    eprintln!();

    for (i, t) in analysis.slowest_tests.iter().enumerate() {
        eprintln!(
            "  {:>2}. {:>8.1}ms  {} > {}",
            i + 1,
            t.duration_ms,
            t.file_path,
            t.test_name
        );
    }
}

/// Print recent runs table (for `gaffer query runs`).
pub fn print_runs(runs: &[RecentRun]) {
    if runs.is_empty() {
        eprintln!("No test runs found.");
        return;
    }

    eprintln!("{}", format!("Recent runs: {}", runs.len()).bold());
    eprintln!();

    for run in runs {
        let branch = run.branch.as_deref().unwrap_or("-");
        let commit = run.commit_sha.as_deref().map(|c| &c[..7.min(c.len())]).unwrap_or("-");
        let duration_secs = run.duration_ms / 1000.0;

        let status_str = if run.failed > 0 {
            format!("{} passed {} failed", run.passed, run.failed).red().to_string()
        } else {
            format!("{} passed", run.passed).green().to_string()
        };

        eprintln!(
            "  {} {} {} ({}) {:.1}s  {}",
            &run.id[..8.min(run.id.len())],
            branch.dimmed(),
            commit.dimmed(),
            status_str,
            duration_secs,
            run.framework.dimmed(),
        );
    }
}

/// Print test history timeline (for `gaffer query history`).
pub fn print_history(history: &[TestHistoryEntry]) {
    if history.is_empty() {
        eprintln!("No history found for this test.");
        return;
    }

    eprintln!(
        "{}",
        format!("History: {} entries", history.len()).bold()
    );
    eprintln!();

    for entry in history {
        let status_colored = match entry.status.as_str() {
            status::PASSED => "PASS".green().to_string(),
            status::FAILED => "FAIL".red().to_string(),
            status::SKIPPED => "SKIP".yellow().to_string(),
            other => other.dimmed().to_string(),
        };

        let branch = entry.branch.as_deref().unwrap_or("-");
        let commit = entry.commit_sha.as_deref().map(|c| &c[..7.min(c.len())]).unwrap_or("-");

        eprint!(
            "  {} {:>8.1}ms  {} {}",
            status_colored,
            entry.duration_ms,
            branch.dimmed(),
            commit.dimmed(),
        );

        if let Some(err) = &entry.error_message {
            eprint!("  {}", truncate_ellipsis(err, 80).red());
        }

        eprintln!();
    }
}

/// Print failure search results (for `gaffer query failures`).
pub fn print_failures_search(failures: &[FailureSearchResult]) {
    if failures.is_empty() {
        eprintln!("No matching failures found.");
        return;
    }

    eprintln!(
        "{}",
        format!("Failures: {} matches", failures.len()).bold()
    );
    eprintln!();

    for f in failures {
        let file = f.file_path.as_deref().unwrap_or("-");
        let branch = f.branch.as_deref().unwrap_or("-");

        eprintln!("  {} > {}", file, f.name.red());
        if let Some(err) = &f.error_message {
            eprintln!("    {}", truncate_ellipsis(err, 100).dimmed());
        }
        eprintln!(
            "    {} {:.1}ms",
            branch.dimmed(),
            f.duration_ms,
        );
        eprintln!();
    }
}

/// Print comparison against a baseline branch.
pub fn print_comparison(comparison: &ComparisonResult) {
    let new_count = comparison.new_failures.len();
    let fixed_count = comparison.fixed.len();
    let pre_existing_count = comparison.pre_existing_failures.len();

    // Summary line
    let mut parts = vec![format!("vs {}: ", comparison.baseline_branch)];

    let mut stats = Vec::new();
    if new_count > 0 {
        stats.push(format!("{}", plural(new_count, "new failure")).red().to_string());
    }
    if fixed_count > 0 {
        stats.push(format!("{} fixed", fixed_count).green().to_string());
    }
    if pre_existing_count > 0 {
        stats.push(format!("{} pre-existing", pre_existing_count).dimmed().to_string());
    }
    if stats.is_empty() {
        stats.push("no changes".dimmed().to_string());
    }
    parts.push(stats.join(", "));

    // Append deltas if significant
    if comparison.pass_rate_delta.abs() >= 0.05 {
        let sign = if comparison.pass_rate_delta > 0.0 { "+" } else { "" };
        let delta_str = format!("  pass rate {}{:.1}%", sign, comparison.pass_rate_delta);
        if comparison.pass_rate_delta > 0.0 {
            parts.push(delta_str.green().to_string());
        } else {
            parts.push(delta_str.red().to_string());
        }
    }
    if comparison.duration_delta.abs() >= 50.0 {
        let sign = if comparison.duration_delta > 0.0 { "+" } else { "" };
        let secs = comparison.duration_delta / 1000.0;
        parts.push(format!("  duration {}{:.1}s", sign, secs).dimmed().to_string());
    }

    eprintln!("{}", parts.concat());

    // Detail lines
    for name in &comparison.new_failures {
        eprintln!("   {}  {}", "NEW".red().bold(), name);
    }
    for name in &comparison.fixed {
        eprintln!("   {}  {}", "FIX".green().bold(), name);
    }
}

/// Print per-failure error messages and stack traces. Unlike cluster/history
/// output, errors are not truncated — `--show-errors` gives full diagnostic output.
pub fn print_errors(failures: &[&TestEvent]) {
    if failures.is_empty() {
        return;
    }

    eprintln!(
        "\n{}",
        format!("Errors: {}", plural(failures.len(), "failed test")).red()
    );

    for t in failures {
        let file = t.file_path.as_deref().unwrap_or("unknown");
        eprintln!(
            "\n  {}  {} > {}",
            "FAIL".red().bold(),
            file,
            t.name,
        );

        if let Some(error) = &t.error {
            for line in error.lines() {
                eprintln!("       {}", line);
            }
        }
    }
}

/// Print context files modified during the test run. Paths are expected to be
/// relative to the project root (as produced by `discover_context_files`).
pub fn print_context(context_files: &[PathBuf]) {
    if context_files.is_empty() {
        return;
    }

    eprintln!(
        "\n{}",
        format!(
            "Context: {} modified during run",
            plural(context_files.len(), "file")
        )
        .dimmed()
    );

    for path in context_files {
        eprintln!("  {}", path.display());
    }
}

/// Print the full enriched report.
pub fn print_report(report: &RunReport, failures: &[&TestEvent], context_files: &[PathBuf], coverage: Option<&CoverageSummary>, sync_result: Option<&SyncResult>, comparison: Option<&ComparisonResult>) {
    let duration_secs = report.summary.duration / 1000.0;
    print_summary(
        report.summary.passed,
        report.summary.failed,
        report.summary.skipped,
        duration_secs,
    );

    if let Some(health) = &report.health {
        print_health(health, report.intelligence.as_ref());
    }

    if let Some(intel) = &report.intelligence {
        print_flaky(intel);
        print_clusters(intel);
    }

    if let Some(cmp) = comparison {
        print_comparison(cmp);
    }

    print_errors(failures);
    print_context(context_files);

    if let Some(cov) = coverage {
        print_coverage(cov);
    }

    if let Some(sync) = sync_result {
        print_sync(sync);
    }
}
