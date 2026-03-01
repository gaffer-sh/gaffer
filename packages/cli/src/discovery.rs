//! Artifact discovery — find report files written during the current test run.
//!
//! Uses the `ignore` crate with gitignore enabled for fast directory pruning,
//! and overrides to whitelist report patterns so gitignored report directories
//! are still searched.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

use ignore::overrides::OverrideBuilder;
use ignore::WalkBuilder;

/// Path segments that indicate test fixtures (not real reports).
const EXCLUDED_SEGMENTS: &[&str] = &["fixtures", "__snapshots__", "examples"];

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

        if !entry.file_type().is_some_and(|ft| ft.is_file()) {
            continue;
        }

        let fresh = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .map(|mtime| mtime >= not_before)
            .unwrap_or(false);
        if !fresh {
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
