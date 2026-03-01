//! Enriched terminal output — ANSI-colored summary of test results.

use colored::Colorize;
use gaffer_core::types::{CoverageSummary, HealthScore, RunReport, SyncResult, TestIntelligence};

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

/// Print flaky test information.
pub fn print_flaky(intelligence: &TestIntelligence) {
    let flaky = &intelligence.flaky_tests;
    if flaky.is_empty() {
        return;
    }

    eprintln!("{}", format!("Flaky: {}", plural(flaky.len(), "test")).yellow());

    for t in flaky.iter().take(5) {
        let pct = (t.flip_rate * 100.0).round() as u32;
        eprintln!(
            "   {} > {} — {}% flip rate ({}/{} runs)",
            t.file_path, t.test_name, pct, t.flip_count, t.total_runs
        );
    }

    if flaky.len() > 5 {
        eprintln!("   ... and {} more", flaky.len() - 5);
    }
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
        let pattern = if c.pattern.len() > 60 {
            let mut end = 57;
            while end > 0 && !c.pattern.is_char_boundary(end) {
                end -= 1;
            }
            format!("{}...", &c.pattern[..end])
        } else {
            c.pattern.clone()
        };
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

/// Print the full enriched report.
pub fn print_report(report: &RunReport, coverage: Option<&CoverageSummary>, sync_result: Option<&SyncResult>) {
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

    if let Some(cov) = coverage {
        print_coverage(cov);
    }

    if let Some(sync) = sync_result {
        print_sync(sync);
    }
}
