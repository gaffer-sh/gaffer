//! Affected test detection — map changed source files to relevant test specs.
//!
//! Two strategies:
//! 1. Naming convention (confidence 0.9): `src/auth.ts` → `src/auth.test.ts`
//! 2. Directory proximity (confidence 0.3): tests in sibling `__tests__/` directories

use std::collections::HashMap;
use std::path::Path;

use crate::types::AffectedTest;

/// Maximum entries to scan per directory (prevents slow scans in large monorepos).
const DIR_ENTRY_LIMIT: usize = 1000;

/// Maximum total affected tests to return.
const RESULT_CAP: usize = 100;

/// Common test file suffixes/patterns by ecosystem.
const TEST_SUFFIXES: &[&str] = &[
    ".test.ts", ".test.tsx", ".test.js", ".test.jsx", ".test.mjs",
    ".spec.ts", ".spec.tsx", ".spec.js", ".spec.jsx", ".spec.mjs",
    "_test.go",
    "_test.rs",
];

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

/// Check if a file path looks like a test file.
fn is_test_file(path: &str) -> bool {
    let lower = path.to_lowercase();
    TEST_SUFFIXES.iter().any(|s| lower.ends_with(s))
        || TEST_PREFIXES.iter().any(|p| {
            Path::new(&lower)
                .file_name()
                .and_then(|f| f.to_str())
                .is_some_and(|name| name.starts_with(p))
        })
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
}
