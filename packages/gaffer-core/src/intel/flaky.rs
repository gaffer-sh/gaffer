//! Flaky test detection — ported from `server/utils/flaky-detection.ts`.
//!
//! Detects flaky tests by analyzing status transitions ("flips") across historical runs.
//! A test that alternates between pass and fail is considered flaky.

use std::collections::HashMap;

use crate::db::HistoricalTest;
use crate::types::FlakyTestResult;

/// Minimum number of executions required for reliable flaky detection.
const MIN_SAMPLE_SIZE: usize = 5;

/// Minimum flip rate threshold — tests below this are not considered flaky.
const FLIP_RATE_THRESHOLD: f64 = 0.1;

/// Detect flaky tests from historical test results.
///
/// Algorithm:
/// 1. Group test executions by test name
/// 2. Track sequence of pass/fail (ignore skipped/todo)
/// 3. Count "flips" (pass→fail or fail→pass transitions)
/// 4. flip_rate = flips / (appearances - 1)
/// 5. Composite score = flip_rate * 0.4 + failure_rate * 0.4 + 0 * 0.2 (no duration variability locally)
pub fn detect_flaky_tests(history: &[HistoricalTest]) -> Vec<FlakyTestResult> {
    // Group by test name, preserving chronological order (input is ordered by started_at ASC)
    let mut test_history: HashMap<&str, TestHistory> = HashMap::new();

    for test in history {
        if !matches!(test.status.as_str(), "passed" | "failed") {
            continue;
        }

        let entry = test_history.entry(&test.name).or_insert_with(|| TestHistory {
            statuses: Vec::new(),
            timestamps: Vec::new(),
            file_path: test.file_path.clone(),
        });

        entry.statuses.push(&test.status);
        entry.timestamps.push(&test.started_at);
        // Update file_path to most recent non-empty value
        if !test.file_path.is_empty() {
            entry.file_path = test.file_path.clone();
        }
    }

    let mut flaky_tests = Vec::new();

    for (name, hist) in &test_history {
        if hist.statuses.len() < MIN_SAMPLE_SIZE {
            continue;
        }

        // Count flips (status transitions), tracking when the last flip occurred
        let mut flips: u32 = 0;
        let mut last_flip_timestamp: Option<&str> = None;
        for (i, pair) in hist.statuses.windows(2).enumerate() {
            if pair[0] != pair[1] {
                flips += 1;
                // The flip is detected at position i+1 (the second element of the window)
                last_flip_timestamp = Some(hist.timestamps[i + 1]);
            }
        }

        let total_runs = hist.statuses.len() as u32;
        let flip_rate = flips as f64 / (total_runs - 1) as f64;

        if flip_rate < FLIP_RATE_THRESHOLD {
            continue;
        }

        // Compute failure rate for composite score
        let failed_count = hist.statuses.iter().filter(|s| **s == "failed").count();
        let failure_rate = failed_count as f64 / total_runs as f64;

        // Composite: flip_rate * 0.4 + failure_rate * 0.4 + duration_variability * 0.2
        // Duration variability is 0 here (we don't track per-test duration spread locally)
        let composite_score = (flip_rate * 0.4 + failure_rate * 0.4).clamp(0.0, 1.0);
        let composite_score = (composite_score * 100.0).round() / 100.0;

        let last_flipped_at = last_flip_timestamp.map(|s| s.to_string());

        flaky_tests.push(FlakyTestResult {
            test_name: name.to_string(),
            file_path: hist.file_path.clone(),
            flip_rate,
            flip_count: flips,
            total_runs,
            composite_score,
            last_flipped_at,
        });
    }

    // Sort by composite score descending (most flaky first)
    flaky_tests.sort_by(|a, b| b.composite_score.total_cmp(&a.composite_score));

    flaky_tests
}

struct TestHistory<'a> {
    statuses: Vec<&'a str>,
    /// Timestamps corresponding to each status entry, for tracking when flips occurred.
    timestamps: Vec<&'a str>,
    file_path: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test(name: &str, status: &str, run_id: &str, started_at: &str) -> HistoricalTest {
        HistoricalTest {
            name: name.to_string(),
            status: status.to_string(),
            duration_ms: 100.0,
            file_path: "src/test.ts".to_string(),
            run_id: run_id.to_string(),
            started_at: started_at.to_string(),
        }
    }

    #[test]
    fn detects_flaky_test_with_alternating_results() {
        // A test that alternates pass/fail across 6 runs = 5 flips / 5 transitions = 100% flip rate
        let history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                let status = if i % 2 == 0 { "passed" } else { "failed" };
                make_test("flaky_test", status, &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1))
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].test_name, "flaky_test");
        assert!((result[0].flip_rate - 1.0).abs() < f64::EPSILON);
        assert_eq!(result[0].flip_count, 5);
        assert_eq!(result[0].total_runs, 6);
    }

    #[test]
    fn ignores_stable_passing_test() {
        let history: Vec<HistoricalTest> = (0..10)
            .map(|i| make_test("stable_test", "passed", &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1)))
            .collect();

        let result = detect_flaky_tests(&history);
        assert!(result.is_empty());
    }

    #[test]
    fn ignores_stable_failing_test() {
        let history: Vec<HistoricalTest> = (0..10)
            .map(|i| make_test("broken_test", "failed", &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1)))
            .collect();

        let result = detect_flaky_tests(&history);
        assert!(result.is_empty());
    }

    #[test]
    fn requires_minimum_sample_size() {
        // Only 3 runs — below minimum of 5
        let history = vec![
            make_test("test", "passed", "run-1", "2026-01-01T00:00:00Z"),
            make_test("test", "failed", "run-2", "2026-01-02T00:00:00Z"),
            make_test("test", "passed", "run-3", "2026-01-03T00:00:00Z"),
        ];

        let result = detect_flaky_tests(&history);
        assert!(result.is_empty());
    }

    #[test]
    fn returns_empty_for_no_history() {
        let result = detect_flaky_tests(&[]);
        assert!(result.is_empty());
    }

    #[test]
    fn skips_non_pass_fail_statuses() {
        let mut history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                let status = if i % 2 == 0 { "passed" } else { "failed" };
                make_test("test", status, &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1))
            })
            .collect();

        // Add skipped entries — should be ignored
        for i in 0..5 {
            history.push(make_test("test", "skipped", &format!("run-skip-{}", i), &format!("2026-02-{:02}T00:00:00Z", i + 1)));
        }

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].total_runs, 6); // Only counted pass/fail
    }

    #[test]
    fn composite_score_is_bounded() {
        let history: Vec<HistoricalTest> = (0..10)
            .map(|i| {
                let status = if i % 2 == 0 { "passed" } else { "failed" };
                make_test("test", status, &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1))
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        assert!(result[0].composite_score >= 0.0);
        assert!(result[0].composite_score <= 1.0);
    }

    #[test]
    fn sorts_by_composite_score_descending() {
        let mut history = Vec::new();

        // Very flaky test: alternates every run
        for i in 0..10 {
            let status = if i % 2 == 0 { "passed" } else { "failed" };
            history.push(make_test("very_flaky", status, &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1)));
        }

        // Somewhat flaky test: flips occasionally
        for i in 0..10 {
            let status = if i < 7 { "passed" } else { "failed" };
            history.push(make_test("somewhat_flaky", status, &format!("run-{}", i), &format!("2026-01-{:02}T00:00:00Z", i + 1)));
        }

        let result = detect_flaky_tests(&history);
        assert!(result.len() >= 1);
        if result.len() >= 2 {
            assert!(result[0].composite_score >= result[1].composite_score);
        }
    }
}
