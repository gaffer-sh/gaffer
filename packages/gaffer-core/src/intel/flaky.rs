//! Flaky test detection — ported from the dashboard's TS implementation.
//!
//! Detects flaky tests by analyzing status transitions ("flips") across historical runs
//! and the variance of failure messages. A test is flagged flaky if it alternates between
//! pass and fail, *or* if it consistently fails but with different error patterns —
//! the second case being a real flakiness signal that pure flip-rate analysis misses.

use std::collections::{HashMap, HashSet};

use crate::db::HistoricalTest;
use crate::intel::cluster::normalize_error_message;
use crate::types::FlakyTestResult;

/// Minimum number of executions required for reliable flaky detection.
const MIN_SAMPLE_SIZE: usize = 5;

/// Minimum flip rate threshold — tests below this are not considered flaky on flip-rate alone.
const FLIP_RATE_THRESHOLD: f64 = 0.1;

/// Minimum distinct failure-message fingerprints to count as variance-flaky.
/// A test that fails with 2+ different normalized errors is flagged even when flip_rate = 0.
const MIN_DISTINCT_FAILURE_PATTERNS: usize = 2;

/// Caps the pattern_variance contribution to the composite score.
/// 5+ distinct error patterns saturates the variance component at 1.0.
const PATTERN_VARIANCE_CAP: f64 = 5.0;

/// Detect flaky tests from historical test results.
///
/// Algorithm:
/// 1. Group test executions by test name
/// 2. Track sequence of pass/fail (ignore skipped/todo)
/// 3. Count "flips" (pass→fail or fail→pass transitions)
/// 4. flip_rate = flips / (appearances - 1)
/// 5. distinct_failure_patterns = unique normalized error messages among the failures
/// 6. Flag the test as flaky when EITHER flip_rate >= FLIP_RATE_THRESHOLD OR
///    distinct_failure_patterns >= MIN_DISTINCT_FAILURE_PATTERNS
/// 7. Composite score = flip_rate * 0.4 + failure_rate * 0.4 + pattern_variance * 0.2
///    where pattern_variance is 0 when distinct_patterns <= 1, otherwise
///    `((distinct_patterns - 1) / (PATTERN_VARIANCE_CAP - 1)).min(1.0)` — so it
///    contributes 0 for stable failures and saturates at PATTERN_VARIANCE_CAP
///    distinct patterns.
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
            errors: Vec::new(),
            file_path: test.file_path.clone(),
        });

        entry.statuses.push(&test.status);
        entry.timestamps.push(&test.started_at);
        entry.errors.push(test.error_message.as_deref());
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

        // Fingerprint failures by normalized error message. Empty/missing errors don't count
        // toward the distinct-pattern set, so a test failing 5x with no error data won't be
        // flagged as variance-flaky on bad data alone.
        let mut failure_fingerprints: HashSet<String> = HashSet::new();
        for (&status, &error) in hist.statuses.iter().zip(hist.errors.iter()) {
            if status != "failed" {
                continue;
            }
            if let Some(msg) = error {
                let normalized = normalize_error_message(msg);
                if !normalized.is_empty() {
                    failure_fingerprints.insert(normalized);
                }
            }
        }
        let distinct_patterns = failure_fingerprints.len();

        let is_flip_flaky = flip_rate >= FLIP_RATE_THRESHOLD;
        let is_variance_flaky = distinct_patterns >= MIN_DISTINCT_FAILURE_PATTERNS;
        if !is_flip_flaky && !is_variance_flaky {
            continue;
        }

        // Compute failure rate for composite score
        let failed_count = hist.statuses.iter().filter(|s| **s == "failed").count();
        let failure_rate = failed_count as f64 / total_runs as f64;

        // pattern_variance saturates at PATTERN_VARIANCE_CAP distinct patterns. A test with one
        // (or zero) distinct error message contributes 0 to this term; a test with 5+ distinct
        // patterns contributes the full 0.2 weight.
        let pattern_variance = if distinct_patterns <= 1 {
            0.0
        } else {
            ((distinct_patterns - 1) as f64 / (PATTERN_VARIANCE_CAP - 1.0)).min(1.0)
        };

        let composite_score =
            (flip_rate * 0.4 + failure_rate * 0.4 + pattern_variance * 0.2).clamp(0.0, 1.0);
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
    /// Error messages corresponding to each status entry (None for passes / missing data).
    errors: Vec<Option<&'a str>>,
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
            error_message: None,
        }
    }

    fn make_failed_test(name: &str, run_id: &str, started_at: &str, error: &str) -> HistoricalTest {
        HistoricalTest {
            name: name.to_string(),
            status: "failed".to_string(),
            duration_ms: 100.0,
            file_path: "src/test.ts".to_string(),
            run_id: run_id.to_string(),
            started_at: started_at.to_string(),
            error_message: Some(error.to_string()),
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
    fn detects_always_failing_with_variant_errors() {
        // A test that fails 6/6 times but with two different errors:
        // flip_rate = 0 (no transitions), distinct patterns = 2 should flag it as flaky
        // even though pure pass/fail flip analysis would treat it as a stable failure.
        let history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                let error = if i % 2 == 0 {
                    "Target page, context or browser has been closed"
                } else {
                    "expect(getByText('3/6 complete')).toBeVisible() failed"
                };
                make_failed_test(
                    "Progress Updates > shows progress as steps are completed",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    error,
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1, "variant-error test should be flagged flaky despite zero flips");
        assert_eq!(result[0].flip_rate, 0.0);
        // Expected: 0 * 0.4 + 1.0 * 0.4 + (1 / 4) * 0.2 = 0.45
        // (zero flips, 100% failure rate, 2 distinct patterns contributing 0.25 variance)
        assert!(
            (result[0].composite_score - 0.45).abs() < 0.01,
            "expected composite ~0.45 for 2 distinct error patterns + full failure, got {}",
            result[0].composite_score
        );
    }

    #[test]
    fn ignores_always_failing_with_same_error() {
        // Stable failure: 6 failures all with the same error message. Distinct patterns = 1,
        // below MIN_DISTINCT_FAILURE_PATTERNS. flip_rate = 0. Should NOT be flagged.
        let history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                make_failed_test(
                    "broken_test",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    "AssertionError: expected 200 got 500",
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert!(result.is_empty(), "a deterministically-failing test with one error pattern is not flaky");
    }

    #[test]
    fn requires_minimum_sample_size_for_variance() {
        // 4 failures (one below MIN_SAMPLE_SIZE=5) with 4 distinct errors — variance condition
        // met on distinct patterns but blocked by the sample-size gate. Verifies the off-by-one
        // boundary, since 5 patterns at 5 runs would saturate variance.
        let history: Vec<HistoricalTest> = vec![
            make_failed_test("test", "run-1", "2026-01-01T00:00:00Z", "Connection refused"),
            make_failed_test("test", "run-2", "2026-01-02T00:00:00Z", "Timeout exceeded"),
            make_failed_test("test", "run-3", "2026-01-03T00:00:00Z", "404 Not Found"),
            make_failed_test("test", "run-4", "2026-01-04T00:00:00Z", "Permission denied"),
        ];

        let result = detect_flaky_tests(&history);
        assert!(
            result.is_empty(),
            "variance below MIN_SAMPLE_SIZE total runs should not flag"
        );
    }

    #[test]
    fn ignores_empty_string_error_messages() {
        // 6 failures with `Some("")` error messages: an instrumentation gap (test runner stripped
        // stderr) should not be confused with multiple failure patterns. Empty messages skip the
        // fingerprint set, so distinct_patterns = 0 and the test stays unflagged.
        let history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                make_failed_test(
                    "instrumentation_gap_test",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    "",
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert!(
            result.is_empty(),
            "empty error_message strings must not contribute distinct patterns; got {:?}",
            result
        );
    }

    #[test]
    fn flip_and_variance_signals_compose_in_composite() {
        // 5 fails + 1 pass with 3 distinct error patterns across the failures.
        // Sequence: pass, fail(A), fail(B), fail(A), fail(C), fail(A) → 1 flip transition.
        //   flip_rate = 1 / 5 = 0.2
        //   failure_rate = 5 / 6 ≈ 0.833
        //   distinct_patterns = 3, pattern_variance = (3-1)/(5-1) = 0.5
        //   composite ≈ 0.2*0.4 + 0.833*0.4 + 0.5*0.2 = 0.08 + 0.333 + 0.1 ≈ 0.513
        let history: Vec<HistoricalTest> = vec![
            make_test("mixed_signals_test", "passed", "run-1", "2026-01-01T00:00:00Z"),
            make_failed_test("mixed_signals_test", "run-2", "2026-01-02T00:00:00Z", "Error A"),
            make_failed_test("mixed_signals_test", "run-3", "2026-01-03T00:00:00Z", "Error B"),
            make_failed_test("mixed_signals_test", "run-4", "2026-01-04T00:00:00Z", "Error A"),
            make_failed_test("mixed_signals_test", "run-5", "2026-01-05T00:00:00Z", "Error C"),
            make_failed_test("mixed_signals_test", "run-6", "2026-01-06T00:00:00Z", "Error A"),
        ];

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        // 0.2 * 0.4 + (5/6) * 0.4 + 0.5 * 0.2 ≈ 0.08 + 0.333 + 0.10 = 0.513
        // The implementation rounds to two decimals before returning, so the asserted value
        // is what the caller will read.
        assert!(
            (result[0].composite_score - 0.51).abs() < 0.01,
            "expected composite ~0.51 for combined flip + variance, got {}",
            result[0].composite_score
        );
    }

    #[test]
    fn normalize_collapses_timestamp_variance_in_error_messages() {
        // Reuses the existing failure-clustering normalizer. Six failures whose error messages
        // differ only in their ISO timestamp should collapse to one fingerprint (the
        // timestamp regex rewrites them to `<timestamp>`), so the test must NOT be flagged
        // variance-flaky. Confirms agents don't get a flood of false positives on tests that
        // fail with a timestamp in the message but the same underlying error.
        let history: Vec<HistoricalTest> = (0..6)
            .map(|i| {
                make_failed_test(
                    "timestamped_failure_test",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    &format!("Request failed at 2026-01-15T10:30:{:02}Z", 40 + i),
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert!(
            result.is_empty(),
            "timestamp-variant errors should normalize to one fingerprint; got {:?}",
            result
        );
    }

    #[test]
    fn variance_score_in_linear_region() {
        // 5 failures with 4 distinct patterns hits the linear region before saturation:
        //   pattern_variance = (4-1)/(5-1) = 0.75
        //   composite = 0*0.4 + 1.0*0.4 + 0.75*0.2 = 0.55
        // Locks in the slope so an off-by-one in PATTERN_VARIANCE_CAP doesn't slip through
        // (the saturation test alone wouldn't catch a regression in the linear part).
        let errors = ["Error A", "Error B", "Error C", "Error D", "Error A"];
        let history: Vec<HistoricalTest> = errors
            .iter()
            .enumerate()
            .map(|(i, err)| {
                make_failed_test(
                    "linear_variance_test",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    err,
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        assert!(
            (result[0].composite_score - 0.55).abs() < 0.01,
            "expected composite ~0.55 for 4 distinct patterns + full failure, got {}",
            result[0].composite_score
        );
    }

    #[test]
    fn variance_score_scales_with_distinct_patterns() {
        // 5 failures with 5 distinct patterns should saturate the variance term (contributes full 0.2 weight).
        let errors = [
            "Connection refused",
            "Timeout exceeded",
            "404 Not Found",
            "Permission denied",
            "Out of memory",
        ];
        let history: Vec<HistoricalTest> = errors
            .iter()
            .enumerate()
            .map(|(i, err)| {
                make_failed_test(
                    "saturated_variance_test",
                    &format!("run-{}", i),
                    &format!("2026-01-{:02}T00:00:00Z", i + 1),
                    err,
                )
            })
            .collect();

        let result = detect_flaky_tests(&history);
        assert_eq!(result.len(), 1);
        // 5 distinct patterns saturates variance; failure_rate=1.0; flip_rate=0
        // Expected composite: 0 * 0.4 + 1.0 * 0.4 + 1.0 * 0.2 = 0.6
        assert!(
            (result[0].composite_score - 0.6).abs() < 0.01,
            "expected composite ~0.6 for max variance + full failure, got {}",
            result[0].composite_score
        );
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
