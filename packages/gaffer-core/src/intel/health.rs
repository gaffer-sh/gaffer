//! Health score calculation — ported from `server/utils/health-score.ts`.
//!
//! Computes a 0–100 health score combining pass rate, stability (flakiness),
//! and trend direction.

use crate::types::HealthScore;

/// Calculate health score from run metrics.
///
/// Formula (matching TS implementation):
///   pass_rate * 0.6 + (100 - flaky_percentage) * 0.3 + trend_bonus * 0.1
///
/// - pass_rate: 0–100 (percentage of tests that passed)
/// - flaky_percentage: 0–100 (percentage of tests that are flaky)
/// - trend_bonus: 100 (improving), 50 (stable), 0 (declining)
pub fn calculate_health_score(
    total: i32,
    passed: i32,
    flaky_count: u32,
    previous_score: Option<f64>,
) -> HealthScore {
    if total == 0 {
        return HealthScore {
            score: 0.0,
            label: "critical".to_string(),
            trend: "stable".to_string(),
            previous_score: None,
        };
    }

    let pass_rate = (passed as f64 / total as f64) * 100.0;
    let flaky_percentage = (flaky_count as f64 / total as f64) * 100.0;
    let base = pass_rate * 0.6 + (100.0 - flaky_percentage) * 0.3;

    // Compute a neutral score (trend_bonus = 50) to determine trend direction
    let neutral_score = (base + 50.0 * 0.1).clamp(0.0, 100.0).round();

    let trend = match previous_score {
        Some(prev) if neutral_score - prev > 2.0 => "improving",
        Some(prev) if neutral_score - prev < -2.0 => "declining",
        _ => "stable",
    };

    let trend_bonus = match trend {
        "improving" => 100.0,
        "declining" => 0.0,
        _ => 50.0,
    };

    let final_score = (base + trend_bonus * 0.1).clamp(0.0, 100.0).round();
    let label = get_health_label(final_score);

    HealthScore {
        score: final_score,
        label: label.to_string(),
        trend: trend.to_string(),
        previous_score,
    }
}

fn get_health_label(score: f64) -> &'static str {
    if score >= 90.0 {
        "excellent"
    } else if score >= 75.0 {
        "good"
    } else if score >= 50.0 {
        "fair"
    } else if score >= 25.0 {
        "poor"
    } else {
        "critical"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perfect_score() {
        let result = calculate_health_score(100, 100, 0, None);
        // pass_rate=100, flaky=0, trend=stable(50)
        // 100*0.6 + 100*0.3 + 50*0.1 = 60 + 30 + 5 = 95
        assert_eq!(result.score, 95.0);
        assert_eq!(result.label, "excellent");
        assert_eq!(result.trend, "stable");
    }

    #[test]
    fn zero_total_returns_critical() {
        let result = calculate_health_score(0, 0, 0, None);
        assert_eq!(result.score, 0.0);
        assert_eq!(result.label, "critical");
    }

    #[test]
    fn all_failing_tests() {
        let result = calculate_health_score(100, 0, 0, None);
        // pass_rate=0, flaky=0, trend=stable(50)
        // 0*0.6 + 100*0.3 + 50*0.1 = 0 + 30 + 5 = 35
        assert_eq!(result.score, 35.0);
        assert_eq!(result.label, "poor");
    }

    #[test]
    fn high_flaky_rate() {
        let result = calculate_health_score(100, 90, 50, None);
        // pass_rate=90, flaky=50%
        // 90*0.6 + 50*0.3 + 50*0.1 = 54 + 15 + 5 = 74
        assert_eq!(result.score, 74.0);
        assert_eq!(result.label, "fair");
    }

    #[test]
    fn improving_trend() {
        // Previous score was 60, now it should be ~95
        let result = calculate_health_score(100, 100, 0, Some(60.0));
        assert_eq!(result.trend, "improving");
        // With improving trend: 100*0.6 + 100*0.3 + 100*0.1 = 60 + 30 + 10 = 100
        assert_eq!(result.score, 100.0);
        assert_eq!(result.label, "excellent");
    }

    #[test]
    fn declining_trend() {
        // Previous score was 95, now much lower
        let result = calculate_health_score(100, 50, 0, Some(95.0));
        assert_eq!(result.trend, "declining");
        // With declining trend: 50*0.6 + 100*0.3 + 0*0.1 = 30 + 30 + 0 = 60
        assert_eq!(result.score, 60.0);
    }

    #[test]
    fn stable_trend_within_threshold() {
        // Previous score ~95, current should also be ~95 (within 5 points)
        let result = calculate_health_score(100, 100, 0, Some(93.0));
        assert_eq!(result.trend, "stable");
    }

    #[test]
    fn label_boundaries() {
        assert_eq!(get_health_label(100.0), "excellent");
        assert_eq!(get_health_label(90.0), "excellent");
        assert_eq!(get_health_label(89.9), "good");
        assert_eq!(get_health_label(75.0), "good");
        assert_eq!(get_health_label(74.9), "fair");
        assert_eq!(get_health_label(50.0), "fair");
        assert_eq!(get_health_label(49.9), "poor");
        assert_eq!(get_health_label(25.0), "poor");
        assert_eq!(get_health_label(24.9), "critical");
        assert_eq!(get_health_label(0.0), "critical");
    }
}
