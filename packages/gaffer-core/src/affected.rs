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
use crate::types::{AffectedTest, AffectedTestSignal, AffectedTestsSignalCoverage};

/// History-derived candidates for a single changed source file. Produced by a
/// [`HistorySignalProvider`] (today: a dashboard API client; in tests: a
/// stub) and consumed by the `coverage_history` / `failure_history`
/// strategies in `find_affected_tests_with_history`.
#[derive(Debug, Clone)]
pub struct HistoryCandidate {
    /// Test file path, relative to the project root (matching the format
    /// the other strategies emit).
    pub test_file: String,
}

/// Source of history-derived signals (coverage history and failure history)
/// for the `affected-tests` pipeline.
///
/// The trait is intentionally narrow: callers ask for candidates per
/// changed source file, and a `None` return communicates "this signal is
/// unavailable on this run" (e.g. the CLI is running without a Gaffer
/// connection). Empty `Some(vec![])` means "we looked, found nothing" —
/// a real signal, not a degraded one. That distinction lets us populate
/// [`AffectedTestsSignalCoverage::unavailable`] correctly so a coding-agent
/// caller can tell whether to trust an empty result set.
pub trait HistorySignalProvider {
    /// Test files that historically executed coverage on the changed source
    /// file. Returns `None` if the provider has no data available
    /// (degraded mode).
    fn coverage_history_candidates(&self, source_file: &str) -> Option<Vec<HistoryCandidate>>;

    /// Test files that have historically failed in commits where the changed
    /// source file was in the diff. Returns `None` if the provider has no
    /// data available (degraded mode).
    fn failure_history_candidates(&self, source_file: &str) -> Option<Vec<HistoryCandidate>>;
}

/// A null provider that returns `None` for both signals. Used by callers
/// that don't have a Gaffer connection or aren't ready to thread one
/// through — keeps the existing `find_affected_tests_with_graph` /
/// `find_affected_tests` entry points unchanged in behavior.
#[derive(Debug, Default)]
pub struct NoHistoryProvider;

impl HistorySignalProvider for NoHistoryProvider {
    fn coverage_history_candidates(&self, _source_file: &str) -> Option<Vec<HistoryCandidate>> {
        None
    }
    fn failure_history_candidates(&self, _source_file: &str) -> Option<Vec<HistoryCandidate>> {
        None
    }
}

/// Confidence assigned to `coverage_history` hits. Lower than naming
/// convention (0.9) and import graph (0.7) because the granularity is
/// test-run-level not test-case-level — a coverage report records "this
/// run touched these lines" but doesn't slice down to which individual
/// test case did the touching. For E2E it's still the only signal that
/// can map server-route changes to specs that hit those routes by URL.
const COVERAGE_HISTORY_CONFIDENCE: f64 = 0.5;

/// Confidence assigned to `failure_history` hits. Lower than coverage
/// history because the correlation is statistical — a test that flaked
/// recently when this file was edited may or may not have actually
/// exercised it.
const FAILURE_HISTORY_CONFIDENCE: f64 = 0.4;

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

/// Result of `find_affected_tests_with_history`: the deduped affected list
/// plus signal-coverage metadata so callers can tell which strategies were
/// available on this run and which were skipped because their data source
/// was missing (degraded mode).
#[derive(Debug, Clone)]
pub struct AffectedTestsRun {
    pub affected: Vec<AffectedTest>,
    pub signals: AffectedTestsSignalCoverage,
}

/// History-aware variant of `find_affected_tests_with_graph`. Unions the
/// heuristic + (optional) graph strategies with `coverage_history` and
/// `failure_history` candidates from a [`HistorySignalProvider`].
///
/// The provider's `None` returns drive the `signals.unavailable` set, so
/// callers can distinguish "we ran every signal and found nothing" from
/// "we ran in degraded mode because Gaffer history wasn't reachable." For
/// the CLI use case this is what surfaces graph-only mode to coding-agent
/// consumers honestly.
///
/// When `use_graph` is false, only the heuristic + history strategies run.
/// When `use_graph` is true, the import-graph BFS runs in memory (no cache;
/// the cached variant has its own entry point because it returns a
/// `Result<_, GafferError>` that this signature does not expose).
pub fn find_affected_tests_with_history(
    project_root: &Path,
    changed_files: &[String],
    history: &dyn HistorySignalProvider,
    use_graph: bool,
) -> AffectedTestsRun {
    let mut hits: HashMap<String, Vec<AffectedTest>> = HashMap::new();
    let mut attempted: Vec<String> = Vec::new();
    let mut unavailable: Vec<String> = Vec::new();

    apply_per_file_strategies(project_root, changed_files, &mut hits);
    attempted.push("naming_convention".to_string());
    attempted.push("directory_proximity".to_string());

    if use_graph && has_any_non_test_input(changed_files) {
        let (graph, _scanned) = build_graph(project_root);
        apply_graph_strategy(&graph, changed_files, &mut hits);
        attempted.push("import_graph".to_string());
    }

    // History-derived signals. `None` from the provider means the data
    // source is unavailable on this run — surface that to the caller via
    // `unavailable`. `Some(vec![])` means the provider looked and found
    // nothing, which is a real signal and counts as `attempted`.
    let mut coverage_history_seen = false;
    let mut coverage_history_unavailable = false;
    let mut failure_history_seen = false;
    let mut failure_history_unavailable = false;
    for source_file in changed_files {
        if is_test_file(source_file) {
            continue;
        }
        match find_by_coverage_history(source_file, history) {
            Some(hits_for_file) => {
                coverage_history_seen = true;
                for t in hits_for_file {
                    hits.entry(t.test_file.clone()).or_default().push(t);
                }
            }
            None => coverage_history_unavailable = true,
        }
        match find_by_failure_history(source_file, history) {
            Some(hits_for_file) => {
                failure_history_seen = true;
                for t in hits_for_file {
                    hits.entry(t.test_file.clone()).or_default().push(t);
                }
            }
            None => failure_history_unavailable = true,
        }
    }
    if coverage_history_seen {
        attempted.push("coverage_history".to_string());
    } else if coverage_history_unavailable {
        unavailable.push("coverage_history".to_string());
    }
    if failure_history_seen {
        attempted.push("failure_history".to_string());
    } else if failure_history_unavailable {
        unavailable.push("failure_history".to_string());
    }

    AffectedTestsRun {
        affected: compose_results(hits),
        signals: AffectedTestsSignalCoverage { attempted, unavailable },
    }
}

/// Strategies 1, 2, 4: heuristic naming-convention, directory-proximity and
/// Rust inline self-test. All cheap and per-file; the cached and in-memory
/// variants share this step.
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
                        signals: Vec::new(),
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
                signals: Vec::new(),
            };
            hits.entry(t.test_file.clone()).or_default().push(t);
        }
    }
}

/// Combine per-test observations via noisy-OR, sort by combined
/// confidence, cap at `RESULT_CAP`. Final stage shared across variants.
fn compose_results(hits: HashMap<String, Vec<AffectedTest>>) -> Vec<AffectedTest> {
    let mut combined: Vec<AffectedTest> = hits.into_values().map(combine_noisy_or)
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
/// individual strategy as the attribution, the combined confidence, and
/// the full per-signal contribution list in `signals` (deduped by strategy,
/// keeping the max confidence per strategy).
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

    let signals = collect_signals(&observations);

    AffectedTest {
        test_file: primary.test_file,
        confidence: combined,
        strategy: primary.strategy,
        source_file: primary.source_file,
        signals,
    }
}

/// Collapse multiple observations into a deduped signal list, keeping the
/// max confidence per strategy. Sorted by confidence descending so the
/// strongest signal is first — agents reading the JSON can stop scanning
/// once they've seen enough.
fn collect_signals(observations: &[AffectedTest]) -> Vec<AffectedTestSignal> {
    let mut by_strategy: HashMap<&str, f64> = HashMap::new();
    for obs in observations {
        let entry = by_strategy.entry(obs.strategy.as_str()).or_insert(0.0);
        if obs.confidence > *entry {
            *entry = obs.confidence;
        }
    }
    let mut signals: Vec<AffectedTestSignal> = by_strategy
        .into_iter()
        .map(|(strategy, confidence)| AffectedTestSignal {
            strategy: strategy.to_string(),
            confidence,
        })
        .collect();
    signals.sort_by(|a, b| {
        b.confidence
            .partial_cmp(&a.confidence)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    signals
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
                    signals: Vec::new(),
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
                    signals: Vec::new(),
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
                            signals: Vec::new(),
                        });
                    }
                }
            }
        }
    }

    results
}

/// Strategy 4: Coverage-history reverse lookup.
///
/// For each changed source file, ask the [`HistorySignalProvider`] which
/// test files have historically been part of a test run that produced
/// coverage rows for that source file. Useful primarily for E2E specs that
/// don't `import` the code they exercise (they hit URLs), where naming,
/// proximity, and the import graph all return empty.
///
/// Returns `None` if the provider has no data on this source file. The
/// caller uses that to populate `AffectedTestsSignalCoverage::unavailable`.
fn find_by_coverage_history(
    source_file: &str,
    history: &dyn HistorySignalProvider,
) -> Option<Vec<AffectedTest>> {
    let candidates = history.coverage_history_candidates(source_file)?;
    Some(
        candidates
            .into_iter()
            .map(|c| AffectedTest {
                test_file: c.test_file,
                confidence: COVERAGE_HISTORY_CONFIDENCE,
                strategy: "coverage_history".to_string(),
                source_file: source_file.to_string(),
                signals: Vec::new(),
            })
            .collect(),
    )
}

/// Strategy 5: Failure-history correlation.
///
/// For each changed source file, ask the [`HistorySignalProvider`] which
/// test files have historically failed in commits that touched that source
/// file. Statistical signal, lower confidence than coverage history.
fn find_by_failure_history(
    source_file: &str,
    history: &dyn HistorySignalProvider,
) -> Option<Vec<AffectedTest>> {
    let candidates = history.failure_history_candidates(source_file)?;
    Some(
        candidates
            .into_iter()
            .map(|c| AffectedTest {
                test_file: c.test_file,
                confidence: FAILURE_HISTORY_CONFIDENCE,
                strategy: "failure_history".to_string(),
                source_file: source_file.to_string(),
                signals: Vec::new(),
            })
            .collect(),
    )
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
                        signals: Vec::new(),
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
                signals: Vec::new(),
            },
            AffectedTest {
                test_file: "x.test.ts".to_string(),
                confidence: 0.7,
                strategy: "import_graph".to_string(),
                source_file: "x.ts".to_string(),
                signals: Vec::new(),
            },
        ];
        let combined = combine_noisy_or(observations);
        assert!((combined.confidence - 0.97).abs() < 1e-6, "got {}", combined.confidence);
        // Higher-confidence strategy wins attribution.
        assert_eq!(combined.strategy, "naming_convention");
        // Both signals surface in the per-test signals list, deduped by
        // strategy, sorted highest-confidence first.
        assert_eq!(combined.signals.len(), 2);
        assert_eq!(combined.signals[0].strategy, "naming_convention");
        assert!((combined.signals[0].confidence - 0.9).abs() < 1e-6);
        assert_eq!(combined.signals[1].strategy, "import_graph");
        assert!((combined.signals[1].confidence - 0.7).abs() < 1e-6);
    }

    #[test]
    fn noisy_or_single_observation_passes_through() {
        let observations = vec![AffectedTest {
            test_file: "x.test.ts".to_string(),
            confidence: 0.7,
            strategy: "import_graph".to_string(),
            source_file: "x.ts".to_string(),
            signals: Vec::new(),
        }];
        let combined = combine_noisy_or(observations);
        assert!((combined.confidence - 0.7).abs() < 1e-6);
        assert_eq!(combined.strategy, "import_graph");
        assert_eq!(combined.signals.len(), 1);
        assert_eq!(combined.signals[0].strategy, "import_graph");
    }

    /// Test stub: returns whatever was registered for a given source file.
    /// `None` for the inner Option means the provider has no data
    /// (degraded mode) for that source file specifically; `Some(vec![])`
    /// means "we looked and found nothing." This distinction is what
    /// drives the signal-coverage tracking in real callers.
    struct StubHistoryProvider {
        coverage: HashMap<String, Option<Vec<HistoryCandidate>>>,
        failure: HashMap<String, Option<Vec<HistoryCandidate>>>,
    }

    impl StubHistoryProvider {
        fn new() -> Self {
            Self { coverage: HashMap::new(), failure: HashMap::new() }
        }
        fn with_coverage(mut self, src: &str, tests: Vec<&str>) -> Self {
            self.coverage.insert(
                src.to_string(),
                Some(tests.into_iter().map(|t| HistoryCandidate { test_file: t.to_string() }).collect()),
            );
            self
        }
        // Symmetric counterpart of `with_coverage`. No test exercises the
        // failure-history path yet; kept so the stub mirrors the two-signal
        // shape of HistorySignalProvider rather than documenting only half of it.
        #[allow(dead_code)]
        fn with_failure(mut self, src: &str, tests: Vec<&str>) -> Self {
            self.failure.insert(
                src.to_string(),
                Some(tests.into_iter().map(|t| HistoryCandidate { test_file: t.to_string() }).collect()),
            );
            self
        }
    }

    impl HistorySignalProvider for StubHistoryProvider {
        fn coverage_history_candidates(&self, source_file: &str) -> Option<Vec<HistoryCandidate>> {
            self.coverage.get(source_file).cloned().unwrap_or(None)
        }
        fn failure_history_candidates(&self, source_file: &str) -> Option<Vec<HistoryCandidate>> {
            self.failure.get(source_file).cloned().unwrap_or(None)
        }
    }

    #[test]
    fn no_history_provider_is_degraded_mode() {
        let provider = NoHistoryProvider;
        assert!(provider.coverage_history_candidates("any/file.ts").is_none());
        assert!(provider.failure_history_candidates("any/file.ts").is_none());
    }

    #[test]
    fn coverage_history_signal_surfaces_e2e_specs_that_lack_static_links() {
        // The case today's affected-tests can't handle: a server route file
        // that no spec imports, but which the E2E suite hits via URL. The
        // import graph + naming + proximity all return empty; coverage
        // history is the only signal that recovers the link.
        let dir = temp_dir();
        let route = dir.path().join("server/api/v1/coverage-summary.get.ts");
        let e2e = dir.path().join("e2e/projects.spec.ts");
        fs::create_dir_all(route.parent().unwrap()).unwrap();
        fs::create_dir_all(e2e.parent().unwrap()).unwrap();
        fs::write(&route, "export default {}").unwrap();
        fs::write(&e2e, "// e2e test, no imports of route").unwrap();

        let provider = StubHistoryProvider::new().with_coverage(
            "server/api/v1/coverage-summary.get.ts",
            vec!["e2e/projects.spec.ts"],
        );

        let run = find_affected_tests_with_history(
            dir.path(),
            &["server/api/v1/coverage-summary.get.ts".to_string()],
            &provider,
            /* use_graph */ false,
        );

        assert_eq!(run.affected.len(), 1);
        assert_eq!(run.affected[0].test_file, "e2e/projects.spec.ts");
        assert_eq!(run.affected[0].strategy, "coverage_history");
        assert!((run.affected[0].confidence - COVERAGE_HISTORY_CONFIDENCE).abs() < 1e-6);
        assert!(run.signals.attempted.contains(&"coverage_history".to_string()));
        assert!(!run.signals.unavailable.contains(&"coverage_history".to_string()));
    }

    #[test]
    fn missing_history_provider_marks_signals_unavailable() {
        // Same fixture, NoHistoryProvider — coverage_history and
        // failure_history must show up in `unavailable`, not `attempted`,
        // so the caller can tell the run is in degraded mode.
        let dir = temp_dir();
        let route = dir.path().join("server/api/v1/coverage-summary.get.ts");
        fs::create_dir_all(route.parent().unwrap()).unwrap();
        fs::write(&route, "export default {}").unwrap();

        let run = find_affected_tests_with_history(
            dir.path(),
            &["server/api/v1/coverage-summary.get.ts".to_string()],
            &NoHistoryProvider,
            /* use_graph */ false,
        );

        assert!(run.signals.unavailable.contains(&"coverage_history".to_string()));
        assert!(run.signals.unavailable.contains(&"failure_history".to_string()));
        assert!(!run.signals.attempted.contains(&"coverage_history".to_string()));
    }

    #[test]
    fn coverage_history_and_naming_convention_combine_via_noisy_or() {
        // Same test selected by naming convention (0.9) AND coverage
        // history (0.5). Combined: 1 - (1 - 0.9)(1 - 0.5) = 0.95.
        // Signals list should carry both contributions.
        let dir = temp_dir();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("auth.ts"), "").unwrap();
        fs::write(src.join("auth.test.ts"), "").unwrap();

        let provider = StubHistoryProvider::new()
            .with_coverage("src/auth.ts", vec!["src/auth.test.ts"]);

        let run = find_affected_tests_with_history(
            dir.path(),
            &["src/auth.ts".to_string()],
            &provider,
            /* use_graph */ false,
        );

        assert_eq!(run.affected.len(), 1);
        let test = &run.affected[0];
        assert_eq!(test.test_file, "src/auth.test.ts");
        assert!((test.confidence - 0.95).abs() < 1e-6, "got {}", test.confidence);
        // Highest-individual-confidence wins primary attribution.
        assert_eq!(test.strategy, "naming_convention");
        // Both signals must appear in the list.
        let strategies: Vec<&str> = test.signals.iter().map(|s| s.strategy.as_str()).collect();
        assert!(strategies.contains(&"naming_convention"));
        assert!(strategies.contains(&"coverage_history"));
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
