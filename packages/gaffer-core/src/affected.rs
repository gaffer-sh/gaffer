//! Affected test detection — map changed source files to relevant test specs.
//!
//! Three strategies, combined via noisy-OR over independent confidences:
//! 1. Naming convention (confidence 0.9): `src/auth.ts` → `src/auth.test.ts`
//! 2. Directory proximity (confidence 0.3): tests in sibling `__tests__/` directories
//! 3. Import-graph reverse-reachability (confidence 0.7): test files that
//!    transitively import the changed source. Opt-in via
//!    `find_affected_tests_with_graph` to avoid the per-call graph build cost
//!    on the heuristic-only path.

use std::collections::HashMap;
use std::path::Path;

use crate::error::GafferError;
use crate::graph::{build_graph, build_graph_cached, has_inline_rust_tests, GraphCache, ImportGraph};
use crate::types::AffectedTest;

/// Maximum entries to scan per directory (prevents slow scans in large monorepos).
const DIR_ENTRY_LIMIT: usize = 1000;

/// Maximum total affected tests to return.
const RESULT_CAP: usize = 100;

/// Common test file suffixes/patterns by ecosystem.
/// Note: `_tests.rs` (plural) is NOT here — it produces false positives on
/// Rust source files like `affected_tests.rs` (a command, not a test). Cargo
/// integration tests are detected via the `/tests/` path component instead;
/// see `is_test_file`.
const TEST_SUFFIXES: &[&str] = &[
    ".test.ts", ".test.tsx", ".test.js", ".test.jsx", ".test.mjs",
    ".spec.ts", ".spec.tsx", ".spec.js", ".spec.jsx", ".spec.mjs",
    "_test.go",
    "_test.rs",
    "_test.py",
];

/// Default confidence for graph-traversal-derived hits.
const GRAPH_CONFIDENCE: f64 = 0.7;

/// Default max BFS depth for graph traversal. 3 hops covers most real
/// service→consumer→test chains without blowing up the result set.
const GRAPH_MAX_DEPTH: usize = 3;

/// Common test file prefixes (Python style).
const TEST_PREFIXES: &[&str] = &["test_"];

/// Sibling directory names that typically contain tests.
const TEST_DIR_NAMES: &[&str] = &["__tests__", "tests", "test", "spec"];

/// Scan for test files affected by the given changed source files.
/// Returns deduplicated results sorted by confidence (highest first).
pub fn find_affected_tests(project_root: &Path, changed_files: &[String]) -> Vec<AffectedTest> {
    let mut results: HashMap<String, AffectedTest> = HashMap::new();

    for source_file in changed_files {
        let source_path = project_root.join(source_file);

        // Skip if the input is already a test file
        if is_test_file(source_file) {
            continue;
        }

        // Strategy 1: Naming convention (confidence 0.9)
        for test in find_by_naming_convention(project_root, source_file, &source_path) {
            let key = test.test_file.clone();
            results.entry(key)
                .and_modify(|existing| {
                    if test.confidence > existing.confidence {
                        *existing = test.clone();
                    }
                })
                .or_insert(test);
        }

        // Strategy 2: Directory proximity (confidence 0.3)
        for test in find_by_directory_proximity(project_root, source_file, &source_path) {
            let key = test.test_file.clone();
            results.entry(key)
                .and_modify(|existing| {
                    if test.confidence > existing.confidence {
                        *existing = test.clone();
                    }
                })
                .or_insert(test);
        }

        if results.len() >= RESULT_CAP {
            break;
        }
    }

    let mut affected: Vec<AffectedTest> = results.into_values().collect();
    affected.sort_by(|a, b| b.confidence.partial_cmp(&a.confidence).unwrap_or(std::cmp::Ordering::Equal));
    affected.truncate(RESULT_CAP);
    affected
}

/// Like `find_affected_tests`, plus a third strategy: build an import graph
/// of the project and include any test file that transitively imports a
/// changed source file (BFS depth ≤ 3, confidence 0.7). Results are unioned
/// across all three strategies via noisy-OR on the per-test-file confidence.
///
/// The graph build walks the project tree from scratch on every call. For
/// the persistent variant — first call walks, subsequent calls skip
/// unchanged files via mtime comparison — see
/// `find_affected_tests_with_graph_cached`.
pub fn find_affected_tests_with_graph(project_root: &Path, changed_files: &[String]) -> Vec<AffectedTest> {
    let mut hits: HashMap<String, Vec<AffectedTest>> = HashMap::new();
    apply_per_file_strategies(project_root, changed_files, &mut hits);
    // The graph build walks the entire project — skip it when there's nothing
    // for the BFS to start from. `apply_graph_strategy` already filters out
    // test-file inputs, so all-test-file inputs produce zero work there.
    if has_any_non_test_input(changed_files) {
        let (graph, _scanned) = build_graph(project_root);
        apply_graph_strategy(&graph, changed_files, &mut hits);
    }
    compose_results(hits)
}

/// Cached variant of `find_affected_tests_with_graph`. The graph is
/// persisted to `cache_db` (typically `<project>/.gaffer/graph.db`);
/// the first call walks + extracts everything and writes the cache,
/// subsequent calls re-extract only files whose mtime has changed.
///
/// Returns `GafferError::Database` on cache open / SQL failures (corrupt DB,
/// permission denied, full disk). The CLI in `commands::affected_tests::run`
/// catches this, logs a warning, and falls back to the in-memory variant —
/// the user always gets a result.
pub fn find_affected_tests_with_graph_cached(
    project_root: &Path,
    changed_files: &[String],
    cache_db: &Path,
) -> Result<Vec<AffectedTest>, GafferError> {
    let mut hits: HashMap<String, Vec<AffectedTest>> = HashMap::new();
    apply_per_file_strategies(project_root, changed_files, &mut hits);

    if !has_any_non_test_input(changed_files) {
        return Ok(compose_results(hits));
    }

    let mut cache = GraphCache::open(cache_db)?;
    let (graph, _scanned, _re_extracted) = build_graph_cached(project_root, &mut cache)?;
    apply_graph_strategy(&graph, changed_files, &mut hits);

    Ok(compose_results(hits))
}

/// Whether the input has at least one non-test file — i.e., a candidate for
/// graph-strategy lookup. The graph strategy bails on test-file inputs by
/// design (changing a test doesn't make other tests "affected"), so an
/// all-test-file input means the graph build is wasted work.
fn has_any_non_test_input(changed_files: &[String]) -> bool {
    changed_files.iter().any(|f| !is_test_file(f))
}

/// Strategies 1, 2, 4: heuristic naming-convention + directory-proximity
/// + Rust inline self-test. All cheap and per-file; the cached and
/// in-memory variants share this step.
fn apply_per_file_strategies(
    project_root: &Path,
    changed_files: &[String],
    hits: &mut HashMap<String, Vec<AffectedTest>>,
) {
    for source_file in changed_files {
        let source_path = project_root.join(source_file);
        if is_test_file(source_file) {
            continue;
        }
        for t in find_by_naming_convention(project_root, source_file, &source_path) {
            hits.entry(t.test_file.clone()).or_default().push(t);
        }
        for t in find_by_directory_proximity(project_root, source_file, &source_path) {
            hits.entry(t.test_file.clone()).or_default().push(t);
        }
        // Rust files with `#[cfg(test)] mod tests` self-test: the file's own
        // unit tests run when you run `cargo test` for the crate. Surface the
        // file as its own test target. Confidence 0.9 — unambiguous when the
        // attribute is present.
        if source_file.ends_with(".rs") {
            if let Ok(source) = std::fs::read_to_string(&source_path) {
                if has_inline_rust_tests(&source) {
                    let t = AffectedTest {
                        test_file: source_file.clone(),
                        confidence: 0.9,
                        strategy: "rust_inline_tests".to_string(),
                        source_file: source_file.clone(),
                    };
                    hits.entry(t.test_file.clone()).or_default().push(t);
                }
            }
        }
    }
}

/// Strategy 3: import-graph reverse reachability. Filters the BFS
/// frontier to test files only.
fn apply_graph_strategy(
    graph: &ImportGraph,
    changed_files: &[String],
    hits: &mut HashMap<String, Vec<AffectedTest>>,
) {
    for source_file in changed_files {
        if is_test_file(source_file) {
            continue;
        }
        let target = Path::new(source_file);
        for importer in graph.reverse_reachable(target, GRAPH_MAX_DEPTH) {
            let importer_str = importer.to_string_lossy().to_string();
            if !is_test_file(&importer_str) {
                continue;
            }
            let t = AffectedTest {
                test_file: importer_str,
                confidence: GRAPH_CONFIDENCE,
                strategy: "import_graph".to_string(),
                source_file: source_file.clone(),
            };
            hits.entry(t.test_file.clone()).or_default().push(t);
        }
    }
}

/// Combine per-test observations via noisy-OR, sort by combined
/// confidence, cap at `RESULT_CAP`. Final stage shared across variants.
fn compose_results(hits: HashMap<String, Vec<AffectedTest>>) -> Vec<AffectedTest> {
    let mut combined: Vec<AffectedTest> = hits
        .into_iter()
        .map(|(_, observations)| combine_noisy_or(observations))
        .collect();
    combined.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    combined.truncate(RESULT_CAP);
    combined
}

/// Combine multiple independent confidence observations of the same test
/// file via noisy-OR. The returned `AffectedTest` carries the highest
/// individual strategy as the attribution and the combined confidence.
fn combine_noisy_or(mut observations: Vec<AffectedTest>) -> AffectedTest {
    debug_assert!(!observations.is_empty(), "combine_noisy_or called on empty");

    // Highest individual hit wins the strategy/source-file attribution.
    observations.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let primary = observations[0].clone();

    let mut p_not: f64 = 1.0;
    for obs in &observations {
        p_not *= 1.0 - obs.confidence.clamp(0.0, 1.0);
    }
    let combined = 1.0 - p_not;

    AffectedTest {
        test_file: primary.test_file,
        confidence: combined,
        strategy: primary.strategy,
        source_file: primary.source_file,
    }
}

/// Check if a file path looks like a test file.
fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    if TEST_SUFFIXES.iter().any(|s| lower.ends_with(s)) {
        return true;
    }
    if TEST_PREFIXES.iter().any(|p| {
        Path::new(&lower)
            .file_name()
            .and_then(|f| f.to_str())
            .is_some_and(|name| name.starts_with(p))
    }) {
        return true;
    }
    // Cargo integration tests: any .rs file inside a `tests/` directory.
    // Path-based to avoid false positives on Rust source files that happen
    // to end in `_tests.rs` (e.g. `affected_tests.rs` command file).
    if lower.ends_with(".rs") && (lower.contains("/tests/") || lower.starts_with("tests/")) {
        return true;
    }
    false
}

/// Strategy 1: Naming convention matching.
/// Given `src/auth.ts`, look for `src/auth.test.ts`, `src/auth.spec.ts`, etc.
fn find_by_naming_convention(project_root: &Path, source_file: &str, source_path: &Path) -> Vec<AffectedTest> {
    let mut results = Vec::new();

    let stem = match source_path.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s.to_string(),
        None => return results,
    };

    let parent = match source_path.parent() {
        Some(p) => p,
        None => return results,
    };

    // Try each test suffix
    for suffix in TEST_SUFFIXES {
        let test_name = format!("{}{}", stem, suffix);
        let test_path = parent.join(&test_name);
        if test_path.exists() {
            if let Ok(rel) = test_path.strip_prefix(project_root) {
                results.push(AffectedTest {
                    test_file: rel.to_string_lossy().to_string(),
                    confidence: 0.9,
                    strategy: "naming_convention".to_string(),
                    source_file: source_file.to_string(),
                });
            }
        }
    }

    // Python: test_auth.py for auth.py
    let ext = source_path.extension().and_then(|e| e.to_str()).unwrap_or("");
    if ext == "py" {
        let test_name = format!("test_{}.py", stem);
        let test_path = parent.join(&test_name);
        if test_path.exists() {
            if let Ok(rel) = test_path.strip_prefix(project_root) {
                results.push(AffectedTest {
                    test_file: rel.to_string_lossy().to_string(),
                    confidence: 0.9,
                    strategy: "naming_convention".to_string(),
                    source_file: source_file.to_string(),
                });
            }
        }
    }

    // Also check __tests__/ directory for same-named test
    for test_dir_name in TEST_DIR_NAMES {
        let test_dir = parent.join(test_dir_name);
        if test_dir.is_dir() {
            for suffix in TEST_SUFFIXES {
                let test_name = format!("{}{}", stem, suffix);
                let test_path = test_dir.join(&test_name);
                if test_path.exists() {
                    if let Ok(rel) = test_path.strip_prefix(project_root) {
                        results.push(AffectedTest {
                            test_file: rel.to_string_lossy().to_string(),
                            confidence: 0.9,
                            strategy: "naming_convention".to_string(),
                            source_file: source_file.to_string(),
                        });
                    }
                }
            }
        }
    }

    results
}

/// Strategy 2: Directory proximity matching.
/// Look for test files in sibling test directories that might test the same module.
fn find_by_directory_proximity(project_root: &Path, source_file: &str, source_path: &Path) -> Vec<AffectedTest> {
    let mut results = Vec::new();

    let parent = match source_path.parent() {
        Some(p) => p,
        None => return results,
    };

    for test_dir_name in TEST_DIR_NAMES {
        let test_dir = parent.join(test_dir_name);
        if !test_dir.is_dir() {
            continue;
        }

        let entries = match std::fs::read_dir(&test_dir) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let mut count = 0;
        for entry in entries.flatten() {
            count += 1;
            if count > DIR_ENTRY_LIMIT {
                eprintln!(
                    "[gaffer] Warning: skipping rest of '{}': exceeded {} entries",
                    test_dir.display(), DIR_ENTRY_LIMIT,
                );
                break;
            }

            let path = entry.path();
            if !path.is_file() {
                continue;
            }

            let filename = match path.file_name().and_then(|f| f.to_str()) {
                Some(f) => f.to_string(),
                None => continue,
            };

            if is_test_file(&filename) {
                if let Ok(rel) = path.strip_prefix(project_root) {
                    let rel_str = rel.to_string_lossy().to_string();
                    // Don't add if already found by naming convention
                    results.push(AffectedTest {
                        test_file: rel_str,
                        confidence: 0.3,
                        strategy: "directory_proximity".to_string(),
                        source_file: source_file.to_string(),
                    });
                }
            }
        }
    }

    results
}

/// Detect the package manager from lock files in the project root.
pub fn detect_package_manager(project_root: &Path) -> &'static str {
    if project_root.join("pnpm-lock.yaml").exists() {
        "pnpm"
    } else if project_root.join("yarn.lock").exists() {
        "yarn"
    } else if project_root.join("bun.lockb").exists() || project_root.join("bun.lock").exists() {
        "bun"
    } else {
        "npx"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn is_test_file_detects_test_suffixes() {
        assert!(is_test_file("auth.test.ts"));
        assert!(is_test_file("auth.spec.js"));
        assert!(is_test_file("auth_test.go"));
        assert!(is_test_file("test_auth.py"));
        assert!(!is_test_file("auth.ts"));
        assert!(!is_test_file("auth.py"));
    }

    #[test]
    fn is_test_file_recognizes_cargo_integration_tests() {
        assert!(is_test_file("packages/gaffer-parsers/tests/junit_tests.rs"));
        assert!(is_test_file("tests/integration.rs"));
    }

    #[test]
    fn is_test_file_does_not_falsely_flag_underscore_tests_rs_command_files() {
        // Regression: affected_tests.rs is a CLI command file, not a test.
        // Distinguishing rule: the `/tests/` directory must be in the path.
        assert!(!is_test_file("packages/cli/src/commands/affected_tests.rs"));
        assert!(!is_test_file("src/commands/server_tests.rs"));
    }

    #[test]
    fn naming_convention_finds_test_file() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("auth.ts"), "").unwrap();
        fs::write(src.join("auth.test.ts"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/auth.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].test_file, "src/auth.test.ts");
        assert_eq!(results[0].confidence, 0.9);
        assert_eq!(results[0].strategy, "naming_convention");
    }

    #[test]
    fn naming_convention_finds_spec_file() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("api.ts"), "").unwrap();
        fs::write(src.join("api.spec.ts"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/api.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].test_file, "src/api.spec.ts");
    }

    #[test]
    fn naming_convention_python_prefix() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("auth.py"), "").unwrap();
        fs::write(src.join("test_auth.py"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/auth.py".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].test_file, "src/test_auth.py");
    }

    #[test]
    fn naming_convention_in_tests_dir() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        let tests = dir.path().join("src/__tests__");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests).unwrap();
        fs::write(src.join("auth.ts"), "").unwrap();
        fs::write(tests.join("auth.test.ts"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/auth.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].test_file, "src/__tests__/auth.test.ts");
    }

    #[test]
    fn directory_proximity_finds_nearby_tests() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        let tests = dir.path().join("src/__tests__");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests).unwrap();
        fs::write(src.join("utils.ts"), "").unwrap();
        fs::write(tests.join("api.test.ts"), "").unwrap(); // different name

        let results = find_affected_tests(dir.path(), &["src/utils.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].confidence, 0.3);
        assert_eq!(results[0].strategy, "directory_proximity");
    }

    #[test]
    fn skips_input_test_files() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("auth.test.ts"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/auth.test.ts".to_string()]);
        assert!(results.is_empty());
    }

    #[test]
    fn deduplicates_keeping_highest_confidence() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        let tests = dir.path().join("src/__tests__");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests).unwrap();
        fs::write(src.join("auth.ts"), "").unwrap();
        fs::write(tests.join("auth.test.ts"), "").unwrap();

        // This file should be found by both naming convention (0.9) and proximity (0.3)
        let results = find_affected_tests(dir.path(), &["src/auth.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].confidence, 0.9); // highest wins
    }

    #[test]
    fn no_matches_returns_empty() {
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("auth.ts"), "").unwrap();

        let results = find_affected_tests(dir.path(), &["src/auth.ts".to_string()]);
        assert!(results.is_empty());
    }

    #[test]
    fn detect_package_manager_pnpm() {
        let dir = temp_dir();
        fs::write(dir.path().join("pnpm-lock.yaml"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), "pnpm");
    }

    #[test]
    fn detect_package_manager_yarn() {
        let dir = temp_dir();
        fs::write(dir.path().join("yarn.lock"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), "yarn");
    }

    #[test]
    fn detect_package_manager_bun() {
        let dir = temp_dir();
        fs::write(dir.path().join("bun.lockb"), "").unwrap();
        assert_eq!(detect_package_manager(dir.path()), "bun");
    }

    #[test]
    fn detect_package_manager_fallback_npx() {
        let dir = temp_dir();
        assert_eq!(detect_package_manager(dir.path()), "npx");
    }

    #[test]
    fn noisy_or_combines_independent_observations() {
        // 0.9 + 0.7 → 1 - (1 - 0.9)(1 - 0.7) = 1 - 0.03 = 0.97
        let observations = vec![
            AffectedTest {
                test_file: "x.test.ts".to_string(),
                confidence: 0.9,
                strategy: "naming_convention".to_string(),
                source_file: "x.ts".to_string(),
            },
            AffectedTest {
                test_file: "x.test.ts".to_string(),
                confidence: 0.7,
                strategy: "import_graph".to_string(),
                source_file: "x.ts".to_string(),
            },
        ];
        let combined = combine_noisy_or(observations);
        assert!((combined.confidence - 0.97).abs() < 1e-6, "got {}", combined.confidence);
        // Higher-confidence strategy wins attribution.
        assert_eq!(combined.strategy, "naming_convention");
    }

    #[test]
    fn noisy_or_single_observation_passes_through() {
        let observations = vec![AffectedTest {
            test_file: "x.test.ts".to_string(),
            confidence: 0.7,
            strategy: "import_graph".to_string(),
            source_file: "x.ts".to_string(),
        }];
        let combined = combine_noisy_or(observations);
        assert!((combined.confidence - 0.7).abs() < 1e-6);
        assert_eq!(combined.strategy, "import_graph");
    }

    #[test]
    fn graph_strategy_finds_test_via_import() {
        // Build a fixture: src/util.ts is imported by tests/util.test.ts, but
        // they live in different parent directories so heuristic finds nothing.
        let dir = temp_dir();
        let src = dir.path().join("src");
        let tests = dir.path().join("tests");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests).unwrap();
        fs::write(src.join("util.ts"), "export const x = 1;").unwrap();
        fs::write(
            tests.join("util.test.ts"),
            "import { x } from '../src/util';\ntest('x', () => {});",
        )
        .unwrap();

        // Heuristic-only: empty (parent dirs differ, no __tests__/ sibling).
        let heuristic = find_affected_tests(dir.path(), &["src/util.ts".to_string()]);
        assert_eq!(heuristic.len(), 0, "heuristic should miss this layout");

        // With graph: finds the test via reverse-import.
        let with_graph = find_affected_tests_with_graph(dir.path(), &["src/util.ts".to_string()]);
        assert_eq!(with_graph.len(), 1, "graph should find the test");
        assert!(with_graph[0].test_file.ends_with("util.test.ts"));
        assert_eq!(with_graph[0].strategy, "import_graph");
        assert!((with_graph[0].confidence - 0.7).abs() < 1e-6);
    }

    #[test]
    fn graph_strategy_unions_with_heuristic_via_noisy_or() {
        // src/util.ts has BOTH a sibling util.test.ts (naming-convention 0.9)
        // AND is imported by it (graph 0.7). Combined should be ~0.97.
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("util.ts"), "export const x = 1;").unwrap();
        fs::write(
            src.join("util.test.ts"),
            "import { x } from './util';\ntest('x', () => {});",
        )
        .unwrap();

        let results = find_affected_tests_with_graph(dir.path(), &["src/util.ts".to_string()]);
        assert_eq!(results.len(), 1);
        assert!(results[0].confidence > 0.95, "expected ~0.97, got {}", results[0].confidence);
        // Naming-convention wins attribution (highest individual hit).
        assert_eq!(results[0].strategy, "naming_convention");
    }
}
