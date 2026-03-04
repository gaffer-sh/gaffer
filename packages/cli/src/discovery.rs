//! Artifact discovery — find files written during the current test run.
//!
//! Two strategies:
//! - `discover_reports`: gitignore-aware with overrides to whitelist report patterns
//!   so gitignored report directories are still searched.
//! - `discover_context_files`: gitignore-disabled, finds modified non-report files
//!   for diagnostic context (screenshots, logs, etc.).

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Path segments that indicate test fixtures (not real reports).
const EXCLUDED_SEGMENTS: &[&str] = &["fixtures", "__snapshots__", "examples"];

/// Returns true if the entry is a regular file modified at or after `not_before`.
fn is_fresh_file(entry: &ignore::DirEntry, not_before: SystemTime) -> bool {
    entry.file_type().is_some_and(|ft| ft.is_file())
        && entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|mtime| mtime >= not_before)
}

/// Discover report files matching the given glob patterns, rooted at project_root.
/// Only returns files modified at or after `not_before` (filters out stale reports
/// from previous runs).
pub fn discover_reports(
    project_root: &Path,
    patterns: &[String],
    not_before: SystemTime,
) -> Vec<PathBuf> {
    let mut ob = OverrideBuilder::new(project_root);

    for pattern in patterns {
        if let Err(e) = ob.add(pattern) {
            eprintln!("[gaffer] Warning: invalid glob pattern '{}': {}", pattern, e);
            continue;
        }

        // Also add directory-level overrides to prevent gitignore from pruning
        // parent directories. e.g., "**/test-reports/**/*.xml" needs
        // "**/test-reports/" to stay walkable.
        for dir_override in extract_dir_overrides(pattern) {
            let _ = ob.add(&dir_override);
        }
    }

    let overrides = match ob.build() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("[gaffer] Warning: failed to compile patterns: {}", e);
            return Vec::new();
        }
    };

    let mut found = Vec::new();

    let walker = WalkBuilder::new(project_root)
        .hidden(false)
        .overrides(overrides)
        .filter_entry(|e| e.file_name() != ".git")
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        if !is_fresh_file(&entry, not_before) {
            continue;
        }

        if is_fixture_path(entry.path()) {
            continue;
        }

        found.push(entry.into_path());
    }

    found
}

/// Extract directory-level override patterns from a file pattern.
/// These prevent gitignore from pruning directories our patterns need.
///
/// e.g., "**/test-reports/**/*.xml" → ["**/test-reports/", "**/test-reports/**/"]
///       "**/coverage/lcov.info"    → ["**/coverage/"]
///       "**/junit*.xml"            → [] (no directory component to protect)
fn extract_dir_overrides(pattern: &str) -> Vec<String> {
    let mut overrides = Vec::new();
    let parts: Vec<&str> = pattern.split('/').collect();

    let mut prefix = String::new();
    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            break; // last segment is the filename pattern, skip
        }

        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);

        // Only add overrides for paths containing at least one concrete
        // (non-wildcard) directory name
        let has_concrete = prefix.split('/').any(|s| !s.is_empty() && !s.contains('*'));
        if has_concrete {
            overrides.push(format!("{}/", prefix));
        }
    }

    overrides
}

/// Directories excluded from context file discovery at any nesting depth.
/// Pruned in `filter_entry()` — the walker never descends into them.
const CONTEXT_EXCLUDED_DIRS: &[&str] = &[
    "node_modules",
    ".git",
    ".gaffer",
    ".next",
    ".nuxt",
    ".turbo",
    ".svelte-kit",
    "target",
    "dist",
    "__pycache__",
    ".tox",
    ".venv",
    "venv",
    ".gradle",
    ".m2",
];

/// Maximum entries per top-level subdirectory before bailing out (layer 2).
const SUBTREE_ENTRY_LIMIT: usize = 1000;

/// Maximum total context files returned (layer 3).
const CONTEXT_FILE_CAP: usize = 50;

/// Discover files modified during the test run that aren't test reports.
/// Does not respect .gitignore rules — gitignored files like test-results/ and
/// logs/ are valuable context. Uses three layers of noise filtering:
/// known exclusions (`CONTEXT_EXCLUDED_DIRS`), subtree bail-out at
/// `SUBTREE_ENTRY_LIMIT` entries, and a `CONTEXT_FILE_CAP`-file result cap.
/// Paths in the returned vec are relative to `project_root`.
pub fn discover_context_files(
    project_root: &Path,
    not_before: SystemTime,
    exclude: &[PathBuf],
) -> Vec<PathBuf> {
    use std::collections::{HashMap, HashSet};

    let exclude_set: HashSet<&Path> = exclude.iter().map(|p| p.as_path()).collect();

    // Entry counts per immediate child directory of project root (layer 2).
    let mut subtree_counts: HashMap<PathBuf, usize> = HashMap::new();
    let mut walk_errors: usize = 0;
    let mut found: Vec<PathBuf> = Vec::new();

    let walker = WalkBuilder::new(project_root)
        .hidden(false)
        .git_ignore(false)
        .ignore(false)
        .filter_entry(|e| {
            if let Some(name) = e.file_name().to_str() {
                if e.depth() > 0 && CONTEXT_EXCLUDED_DIRS.contains(&name) {
                    return false;
                }
            }
            true
        })
        .build();

    for entry in walker {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => {
                walk_errors += 1;
                continue;
            }
        };

        let path = entry.path();

        // Layer 2: bail out of subtrees with too many entries.
        if let Ok(rel) = path.strip_prefix(project_root) {
            if let Some(top_component) = rel.components().next() {
                let top_dir = PathBuf::from(top_component.as_os_str());
                let count = subtree_counts.entry(top_dir.clone()).or_insert(0);
                *count += 1;
                if *count == SUBTREE_ENTRY_LIMIT + 1 {
                    eprintln!(
                        "[gaffer] Warning: skipping '{}/': exceeded {} entries during context discovery",
                        top_dir.display(),
                        SUBTREE_ENTRY_LIMIT,
                    );
                }
                if *count > SUBTREE_ENTRY_LIMIT {
                    continue;
                }
            }
        }

        if !is_fresh_file(&entry, not_before) {
            continue;
        }

        // Exclude files already parsed as reports (compared before relativizing).
        if exclude_set.contains(path) {
            continue;
        }

        let rel_path = path.strip_prefix(project_root)
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|_| path.to_path_buf());

        found.push(rel_path);

        // Layer 3: result cap
        if found.len() >= CONTEXT_FILE_CAP {
            eprintln!(
                "[gaffer] Warning: context file limit reached ({}), some files omitted",
                CONTEXT_FILE_CAP,
            );
            break;
        }
    }

    if walk_errors > 0 {
        eprintln!(
            "[gaffer] Warning: {} entries skipped during context file discovery (permission denied or I/O error)",
            walk_errors,
        );
    }

    found.sort();
    found
}

/// Returns true if the path contains segments that indicate it's a test fixture.
fn is_fixture_path(path: &Path) -> bool {
    path.components().any(|c| {
        if let std::path::Component::Normal(s) = c {
            if let Some(s) = s.to_str() {
                return EXCLUDED_SEGMENTS.contains(&s);
            }
        }
        false
    })
}
