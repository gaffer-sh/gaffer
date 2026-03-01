//! Failure clustering — ported from `server/utils/failure-clustering.ts`.
//!
//! Groups failed tests with similar error messages using Levenshtein distance
//! to identify patterns caused by the same root issue.

use regex::Regex;
use std::sync::LazyLock;

use crate::db::FailedTest;
use crate::types::FailureCluster;

/// Similarity threshold — errors must be at least 70% similar to cluster together.
/// Matches the TypeScript original (`DEFAULT_SIMILARITY_THRESHOLD = 0.7`).
const SIMILARITY_THRESHOLD: f64 = 0.7;

/// Maximum error message length for comparison (performance guard).
const MAX_ERROR_LENGTH: usize = 500;

/// Cluster failed tests by error message similarity.
///
/// Algorithm:
/// 1. Normalize error messages (strip line numbers, UUIDs, timestamps, etc.)
/// 2. For each unassigned failure, create a new cluster
/// 3. Find all other unassigned failures with similarity ≥ threshold, add to cluster
/// 4. Sort clusters by count descending
pub fn cluster_failures(failures: &[FailedTest]) -> Vec<FailureCluster> {
    if failures.is_empty() {
        return Vec::new();
    }

    // Split into tests with and without error messages
    let (with_errors, without_errors): (Vec<_>, Vec<_>) = failures
        .iter()
        .partition(|f| !f.error.trim().is_empty());

    let mut clusters = Vec::new();

    // Cluster tests with error messages by Levenshtein similarity
    if !with_errors.is_empty() {
        let normalized: Vec<String> = with_errors
            .iter()
            .map(|f| {
                let norm = normalize_error_message(&f.error);
                truncate_at_char_boundary(&norm, MAX_ERROR_LENGTH)
            })
            .collect();

        let mut assigned = vec![false; with_errors.len()];

        for i in 0..with_errors.len() {
            if assigned[i] {
                continue;
            }

            assigned[i] = true;
            let mut test_names = vec![with_errors[i].name.clone()];
            let mut file_paths = vec![with_errors[i].file_path.clone()];

            for j in (i + 1)..with_errors.len() {
                if assigned[j] {
                    continue;
                }

                let sim = string_similarity(&normalized[i], &normalized[j]);
                if sim >= SIMILARITY_THRESHOLD {
                    assigned[j] = true;
                    test_names.push(with_errors[j].name.clone());
                    file_paths.push(with_errors[j].file_path.clone());
                }
            }

            // Deduplicate file paths
            file_paths.sort();
            file_paths.dedup();

            clusters.push(FailureCluster {
                pattern: normalized[i].clone(),
                count: test_names.len() as u32,
                test_names,
                file_paths,
                similarity: SIMILARITY_THRESHOLD,
            });
        }
    }

    // Group tests without error messages by file path
    if !without_errors.is_empty() {
        let mut by_file: std::collections::HashMap<&str, Vec<&FailedTest>> =
            std::collections::HashMap::new();
        for f in &without_errors {
            let key = if f.file_path.is_empty() {
                "(unknown file)"
            } else {
                &f.file_path
            };
            by_file.entry(key).or_default().push(f);
        }

        for (file_path, group) in by_file {
            let test_names: Vec<String> = group.iter().map(|f| f.name.clone()).collect();
            clusters.push(FailureCluster {
                pattern: format!("Failed tests in {} (no error message captured)", file_path),
                count: test_names.len() as u32,
                test_names,
                file_paths: vec![file_path.to_string()],
                similarity: SIMILARITY_THRESHOLD,
            });
        }
    }

    // Sort by count descending
    clusters.sort_by(|a, b| b.count.cmp(&a.count));

    clusters
}

// Compiled regexes for error normalization
static RE_UUID: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)[a-f0-9]{8}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{4}-[a-f0-9]{12}")
        .expect("RE_UUID: invalid regex literal")
});
static RE_TIMESTAMP: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?Z?")
        .expect("RE_TIMESTAMP: invalid regex literal")
});
static RE_FILE_LINE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"/.+/([^/]+\.[jt]sx?):\d+(:\d+)?")
        .expect("RE_FILE_LINE: invalid regex literal")
});
static RE_ID_EQUALS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bid[=:]\s*\d+").expect("RE_ID_EQUALS: invalid regex literal"));
static RE_LARGE_NUMBER: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b\d{4,}\b").expect("RE_LARGE_NUMBER: invalid regex literal"));
static RE_HEX_ADDRESS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)0x[a-f0-9]+").expect("RE_HEX_ADDRESS: invalid regex literal"));
static RE_WHITESPACE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s+").expect("RE_WHITESPACE: invalid regex literal"));

/// Truncate a string at a safe UTF-8 char boundary, never exceeding `max_bytes`.
fn truncate_at_char_boundary(s: &str, max_bytes: usize) -> String {
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    s[..end].to_string()
}

/// Normalize an error message for clustering by stripping variable parts.
fn normalize_error_message(message: &str) -> String {
    if message.is_empty() {
        return String::new();
    }

    let s = RE_WHITESPACE.replace_all(message, " ");
    let s = RE_UUID.replace_all(&s, "<uuid>");
    let s = RE_TIMESTAMP.replace_all(&s, "<timestamp>");
    let s = RE_FILE_LINE.replace_all(&s, "$1:<line>");
    let s = RE_ID_EQUALS.replace_all(&s, "id=<id>");
    let s = RE_LARGE_NUMBER.replace_all(&s, "<id>");
    let s = RE_HEX_ADDRESS.replace_all(&s, "<address>");
    let s = RE_WHITESPACE.replace_all(&s, " ");
    s.trim().to_string()
}

/// Calculate Levenshtein distance between two strings.
/// Uses the two-row optimization for O(min(m,n)) space.
fn levenshtein_distance(a: &str, b: &str) -> usize {
    if a == b {
        return 0;
    }
    if a.is_empty() {
        return b.len();
    }
    if b.is_empty() {
        return a.len();
    }

    // Use shorter string as "columns" to minimize memory
    let (a, b) = if a.len() > b.len() { (b, a) } else { (a, b) };

    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let a_len = a_bytes.len();

    let mut prev: Vec<usize> = (0..=a_len).collect();
    let mut curr = vec![0usize; a_len + 1];

    for j in 1..=b_bytes.len() {
        curr[0] = j;
        for i in 1..=a_len {
            let cost = usize::from(a_bytes[i - 1] != b_bytes[j - 1]);
            curr[i] = (prev[i] + 1).min(curr[i - 1] + 1).min(prev[i - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }

    prev[a_len]
}

/// Normalized similarity score between two strings (0.0–1.0).
fn string_similarity(a: &str, b: &str) -> f64 {
    if a == b {
        return 1.0;
    }
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }

    let max_len = a.len().max(b.len());
    let distance = levenshtein_distance(a, b);
    1.0 - (distance as f64 / max_len as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_failure(name: &str, error: &str) -> FailedTest {
        FailedTest {
            name: name.to_string(),
            file_path: "src/test.ts".to_string(),
            error: error.to_string(),
        }
    }

    // ---- Levenshtein ----

    #[test]
    fn levenshtein_identical_strings() {
        assert_eq!(levenshtein_distance("hello", "hello"), 0);
    }

    #[test]
    fn levenshtein_empty_strings() {
        assert_eq!(levenshtein_distance("", ""), 0);
        assert_eq!(levenshtein_distance("abc", ""), 3);
        assert_eq!(levenshtein_distance("", "abc"), 3);
    }

    #[test]
    fn levenshtein_single_edit() {
        assert_eq!(levenshtein_distance("kitten", "sitten"), 1);
        assert_eq!(levenshtein_distance("kitten", "kittens"), 1);
    }

    #[test]
    fn levenshtein_known_distance() {
        assert_eq!(levenshtein_distance("kitten", "sitting"), 3);
    }

    // ---- Similarity ----

    #[test]
    fn similarity_identical() {
        assert!((string_similarity("hello", "hello") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn similarity_completely_different() {
        let sim = string_similarity("abc", "xyz");
        assert!(sim < 0.5);
    }

    // ---- Normalization ----

    #[test]
    fn normalizes_uuids() {
        let msg = "Failed for user 123e4567-e89b-12d3-a456-426614174000";
        let result = normalize_error_message(msg);
        assert!(result.contains("<uuid>"));
        assert!(!result.contains("123e4567"));
    }

    #[test]
    fn normalizes_timestamps() {
        let msg = "Error at 2026-01-15T10:30:00Z";
        let result = normalize_error_message(msg);
        assert!(result.contains("<timestamp>"));
    }

    #[test]
    fn normalizes_hex_addresses() {
        let msg = "Segfault at 0xDEADBEEF";
        let result = normalize_error_message(msg);
        assert!(result.contains("<address>"));
    }

    #[test]
    fn normalizes_large_numbers() {
        let msg = "User 12345 not found";
        let result = normalize_error_message(msg);
        assert!(result.contains("<id>"));
    }

    #[test]
    fn normalizes_id_equals() {
        let msg = "Failed for id=42";
        // 42 is not 4+ digits, but id= pattern matches
        let result = normalize_error_message(msg);
        assert!(result.contains("id=<id>"));
    }

    // ---- Clustering ----

    #[test]
    fn empty_input_returns_empty() {
        assert!(cluster_failures(&[]).is_empty());
    }

    #[test]
    fn single_failure_creates_single_cluster() {
        let failures = vec![make_failure("test_a", "Expected 200, got 500")];
        let clusters = cluster_failures(&failures);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 1);
        assert_eq!(clusters[0].test_names, vec!["test_a"]);
    }

    #[test]
    fn similar_errors_cluster_together() {
        let failures = vec![
            make_failure("test_a", "Expected status 200, got 500"),
            make_failure("test_b", "Expected status 200, got 500"),
            make_failure("test_c", "Expected status 200, got 502"),
        ];
        let clusters = cluster_failures(&failures);
        // All should be in one cluster (very similar errors)
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].count, 3);
    }

    #[test]
    fn different_errors_create_separate_clusters() {
        let failures = vec![
            make_failure("test_a", "Connection timeout after 30s"),
            make_failure("test_b", "Cannot read property 'foo' of undefined"),
        ];
        let clusters = cluster_failures(&failures);
        assert_eq!(clusters.len(), 2);
    }

    #[test]
    fn tests_without_errors_grouped_by_file() {
        let failures = vec![
            FailedTest {
                name: "test_a".to_string(),
                file_path: "src/auth.test.ts".to_string(),
                error: String::new(),
            },
            FailedTest {
                name: "test_b".to_string(),
                file_path: "src/auth.test.ts".to_string(),
                error: String::new(),
            },
        ];
        let clusters = cluster_failures(&failures);
        assert_eq!(clusters.len(), 1);
        assert!(clusters[0].pattern.contains("auth.test.ts"));
        assert_eq!(clusters[0].count, 2);
    }

    #[test]
    fn sorted_by_count_descending() {
        let failures = vec![
            make_failure("test_a", "Error A specific message here"),
            make_failure("test_b", "Error B something completely different and unrelated to A"),
            make_failure("test_c", "Error B something completely different and unrelated to A too"),
            make_failure("test_d", "Error B something completely different and unrelated to A also"),
        ];
        let clusters = cluster_failures(&failures);
        assert!(clusters.len() >= 2);
        assert!(clusters[0].count >= clusters[1].count);
    }
}
