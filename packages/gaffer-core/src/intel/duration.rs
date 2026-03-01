//! Duration analytics — ported from `server/utils/duration-analytics.ts`.
//!
//! Computes percentiles (p50/p75/p90/p95/p99), mean duration, and identifies
//! the slowest tests in a run.

use crate::types::{DurationAnalysis, SlowestTest};

/// Round to 2 decimal places.
fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// Analyze test durations and produce percentile statistics + slowest tests.
///
/// Input: slice of (test_name, file_path, duration_ms) tuples.
/// Uses linear interpolation for percentile calculation (matching TS implementation).
pub fn analyze_duration(tests: &[(String, String, f64)]) -> DurationAnalysis {
    if tests.is_empty() {
        return DurationAnalysis {
            p50: 0.0,
            p75: 0.0,
            p90: 0.0,
            p95: 0.0,
            p99: 0.0,
            mean: 0.0,
            slowest_tests: Vec::new(),
        };
    }

    // Collect all durations for percentile computation
    let mut durations: Vec<f64> = tests.iter().map(|(_, _, d)| *d).collect();
    durations.sort_by(|a, b| a.total_cmp(b));

    let mean = durations.iter().sum::<f64>() / durations.len() as f64;

    let p50 = calculate_percentile(&durations, 50.0);
    let p75 = calculate_percentile(&durations, 75.0);
    let p90 = calculate_percentile(&durations, 90.0);
    let p95 = calculate_percentile(&durations, 95.0);
    let p99 = calculate_percentile(&durations, 99.0);

    // Identify top 10 slowest tests
    let mut by_duration: Vec<(usize, f64)> = tests
        .iter()
        .enumerate()
        .map(|(i, (_, _, d))| (i, *d))
        .collect();
    by_duration.sort_by(|a, b| b.1.total_cmp(&a.1));

    let total = durations.len();
    let slowest_tests: Vec<SlowestTest> = by_duration
        .into_iter()
        .take(10)
        .map(|(i, dur)| {
            // Calculate percentile rank: what % of tests are slower or equal
            let rank = durations.partition_point(|&d| d <= dur);
            let percentile = (rank as f64 / total as f64) * 100.0;

            SlowestTest {
                test_name: tests[i].0.clone(),
                file_path: tests[i].1.clone(),
                duration_ms: dur,
                percentile: round2(percentile),
            }
        })
        .collect();

    DurationAnalysis {
        p50: round2(p50),
        p75: round2(p75),
        p90: round2(p90),
        p95: round2(p95),
        p99: round2(p99),
        mean: round2(mean),
        slowest_tests,
    }
}

/// Calculate a percentile value using linear interpolation.
/// `sorted` must be sorted ascending. `percentile` is 0–100.
fn calculate_percentile(sorted: &[f64], percentile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    if sorted.len() == 1 {
        return sorted[0];
    }

    let index = (percentile / 100.0) * (sorted.len() - 1) as f64;
    let lower = index.floor() as usize;
    let upper = index.ceil() as usize;

    if lower == upper {
        return sorted[lower];
    }

    let fraction = index - lower as f64;
    sorted[lower] + (sorted[upper] - sorted[lower]) * fraction
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_input() {
        let result = analyze_duration(&[]);
        assert_eq!(result.p50, 0.0);
        assert_eq!(result.mean, 0.0);
        assert!(result.slowest_tests.is_empty());
    }

    #[test]
    fn single_test() {
        let tests = vec![("test_a".to_string(), "src/a.ts".to_string(), 100.0)];
        let result = analyze_duration(&tests);
        assert_eq!(result.p50, 100.0);
        assert_eq!(result.p95, 100.0);
        assert_eq!(result.mean, 100.0);
        assert_eq!(result.slowest_tests.len(), 1);
        assert_eq!(result.slowest_tests[0].test_name, "test_a");
    }

    #[test]
    fn percentiles_with_known_dataset() {
        // 10 tests with durations 1..=10
        let tests: Vec<(String, String, f64)> = (1..=10)
            .map(|i| (format!("test_{}", i), "src/test.ts".to_string(), i as f64))
            .collect();

        let result = analyze_duration(&tests);

        // Mean should be 5.5
        assert!((result.mean - 5.5).abs() < 0.01);

        // P50 of [1,2,3,4,5,6,7,8,9,10]:
        // index = 0.5 * 9 = 4.5 → interpolate between sorted[4]=5 and sorted[5]=6 → 5.5
        assert!((result.p50 - 5.5).abs() < 0.01);

        // P95: index = 0.95 * 9 = 8.55 → interpolate between sorted[8]=9 and sorted[9]=10
        // 9 + (10-9) * 0.55 = 9.55
        assert!((result.p95 - 9.55).abs() < 0.01);
    }

    #[test]
    fn slowest_tests_limited_to_10() {
        let tests: Vec<(String, String, f64)> = (1..=20)
            .map(|i| (format!("test_{}", i), "src/test.ts".to_string(), i as f64))
            .collect();

        let result = analyze_duration(&tests);
        assert_eq!(result.slowest_tests.len(), 10);
        // First should be the slowest
        assert_eq!(result.slowest_tests[0].duration_ms, 20.0);
    }

    #[test]
    fn slowest_tests_sorted_by_duration_desc() {
        let tests = vec![
            ("fast".to_string(), "src/test.ts".to_string(), 10.0),
            ("slow".to_string(), "src/test.ts".to_string(), 1000.0),
            ("medium".to_string(), "src/test.ts".to_string(), 100.0),
        ];

        let result = analyze_duration(&tests);
        assert_eq!(result.slowest_tests[0].test_name, "slow");
        assert_eq!(result.slowest_tests[1].test_name, "medium");
        assert_eq!(result.slowest_tests[2].test_name, "fast");
    }

    #[test]
    fn percentile_of_two_values() {
        let sorted = vec![10.0, 20.0];
        assert_eq!(calculate_percentile(&sorted, 0.0), 10.0);
        assert_eq!(calculate_percentile(&sorted, 50.0), 15.0);
        assert_eq!(calculate_percentile(&sorted, 100.0), 20.0);
    }

    #[test]
    fn percentile_of_single_value() {
        let sorted = vec![42.0];
        assert_eq!(calculate_percentile(&sorted, 50.0), 42.0);
        assert_eq!(calculate_percentile(&sorted, 95.0), 42.0);
    }

    #[test]
    fn percentile_rank_is_reasonable() {
        let tests: Vec<(String, String, f64)> = (1..=100)
            .map(|i| (format!("test_{}", i), "src/test.ts".to_string(), i as f64))
            .collect();

        let result = analyze_duration(&tests);
        // The slowest test (100ms) should have percentile ~100
        assert!(result.slowest_tests[0].percentile >= 99.0);
    }
}
