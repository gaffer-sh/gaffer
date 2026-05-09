//! Rust import extractor (v0.1).
//!
//! Two edge sources, fundamentally different from JS-style imports:
//!
//! 1. `mod foo;` declarations → file edge to sibling `foo.rs` or `foo/mod.rs`.
//!    Forms the module tree within a single crate.
//! 2. `use <crate_name>::...` where `<crate_name>` matches a workspace crate
//!    → coarse edges to **all** source files in that crate's `src/`. Without
//!    name resolution we can't tell which symbol the `use` actually pulls
//!    from, so we connect to the whole crate. Over-approximate but useful:
//!    a Cargo `tests/foo.rs` integration test that does
//!    `use gaffer_parsers::*;` becomes connected to every file in
//!    `packages/gaffer-parsers/src/`.
//!
//! Inline `#[cfg(test)] mod tests` blocks are handled separately as a
//! "self-test" strategy in `affected.rs`, not via the import graph (a file
//! containing its own tests isn't an *edge* — it's a property of the file).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use walkdir::WalkDir;

use super::ImportExtractor;

/// Encodes the resolution intent in the specifier string so a single trait
/// method can return both `mod` declarations and `use` chains. The walker
/// passes these back to `resolve()` for path resolution.
const MOD_PREFIX: &str = "mod:";
const USE_PREFIX: &str = "use:";

pub struct RustExtractor {
    /// Lazy workspace crate index — built once on first `resolve()` call,
    /// reused for the lifetime of the extractor.
    crate_index: Mutex<Option<HashMap<String, PathBuf>>>,
}

impl RustExtractor {
    pub fn new() -> Self {
        Self {
            crate_index: Mutex::new(None),
        }
    }

    /// Get or build the workspace crate index: crate name (underscored) → src/ dir.
    fn ensure_index(&self, project_root: &Path) -> HashMap<String, PathBuf> {
        let mut guard = self.crate_index.lock().expect("crate_index lock");
        if let Some(idx) = guard.as_ref() {
            return idx.clone();
        }
        let idx = build_crate_index(project_root);
        *guard = Some(idx.clone());
        idx
    }
}

impl Default for RustExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn mod_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // `mod foo;` — must be terminated with `;` to be a module declaration
        // (vs `mod foo { ... }` which is an inline module, no file edge).
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]+\))?\s+)?mod\s+([A-Za-z_][A-Za-z0-9_]*)\s*;").unwrap()
    })
}

fn use_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Captures the leading path segment of a `use` statement.
        // Matches `use foo::...`, `use foo::{...}`, `use foo;`, but skips
        // `crate::`, `self::`, `super::` (which are within-crate references
        // that the mod-tree already covers).
        Regex::new(r"(?m)^\s*(?:pub(?:\([^)]+\))?\s+)?use\s+([A-Za-z_][A-Za-z0-9_]*)").unwrap()
    })
}

impl ImportExtractor for RustExtractor {
    fn extensions(&self) -> &'static [&'static str] {
        &["rs"]
    }

    fn extract_imports(&self, source: &str) -> Vec<String> {
        let cleaned = strip_rust_comments(source);
        let mut specifiers = Vec::new();

        for caps in mod_regex().captures_iter(&cleaned) {
            if let Some(name) = caps.get(1) {
                specifiers.push(format!("{}{}", MOD_PREFIX, name.as_str()));
            }
        }

        for caps in use_regex().captures_iter(&cleaned) {
            if let Some(seg) = caps.get(1) {
                let head = seg.as_str();
                // Skip within-crate references — covered by mod tree.
                if matches!(head, "crate" | "self" | "super" | "std" | "core" | "alloc") {
                    continue;
                }
                specifiers.push(format!("{}{}", USE_PREFIX, head));
            }
        }

        specifiers
    }

    fn resolve(&self, specifier: &str, importing_file: &Path, project_root: &Path) -> Option<PathBuf> {
        if let Some(name) = specifier.strip_prefix(MOD_PREFIX) {
            return resolve_mod(name, importing_file, project_root);
        }
        if let Some(crate_name) = specifier.strip_prefix(USE_PREFIX) {
            // Cross-crate use → edge to the crate's lib.rs as the canonical
            // entry point. The walker emits one edge per use; the crate's
            // public surface is reachable from lib.rs via mod declarations,
            // so BFS will find downstream files via existing mod: edges.
            return resolve_workspace_crate(crate_name, &self.ensure_index(project_root), project_root);
        }
        None
    }
}

/// Resolve `mod foo;` from `importing_file` to the corresponding source file.
/// Rust's rules: in `src/lib.rs` or `src/main.rs`, `mod foo;` resolves to
/// `src/foo.rs` or `src/foo/mod.rs`. In `src/bar.rs`, it resolves to
/// `src/bar/foo.rs` or `src/bar/foo/mod.rs`. (We don't handle the `#[path]`
/// attribute override — uncommon in practice.)
fn resolve_mod(name: &str, importing_file: &Path, project_root: &Path) -> Option<PathBuf> {
    let parent = importing_file.parent()?;
    let stem = importing_file
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("");

    // If the importer is `lib.rs`, `main.rs`, or `mod.rs`, sibling files apply.
    // Otherwise the module lives in a subdirectory named after the importer.
    let search_dir: PathBuf = if matches!(stem, "lib" | "main" | "mod") {
        parent.to_path_buf()
    } else {
        parent.join(stem)
    };

    // Try `<dir>/<name>.rs` first, then `<dir>/<name>/mod.rs`.
    let direct = search_dir.join(format!("{}.rs", name));
    if direct.is_file() {
        return canonicalize_within(&direct, project_root);
    }
    let mod_form = search_dir.join(name).join("mod.rs");
    if mod_form.is_file() {
        return canonicalize_within(&mod_form, project_root);
    }
    None
}

/// Resolve a cross-crate `use <crate>::...` to the crate's entry source file
/// (lib.rs or main.rs). The full mod-tree of the crate is reachable from
/// there via the existing mod-resolution edges, so BFS picks up downstream
/// files for free.
fn resolve_workspace_crate(
    crate_name: &str,
    index: &HashMap<String, PathBuf>,
    project_root: &Path,
) -> Option<PathBuf> {
    let src_dir = index.get(crate_name)?;
    for entry_name in ["lib.rs", "main.rs"] {
        let entry = src_dir.join(entry_name);
        if entry.is_file() {
            return canonicalize_within(&entry, project_root);
        }
    }
    None
}

/// Walk Cargo.toml files under the project and build a `crate_name → src/ dir` index.
/// Crate names are normalized to underscore form (Rust import-path convention).
fn build_crate_index(project_root: &Path) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for entry in WalkDir::new(project_root)
        .max_depth(8)
        .into_iter()
        .filter_entry(|e| {
            // Smaller skip list than the main walker — we only need to find
            // Cargo.toml files, so we can be aggressive about skipping. The
            // worktree skip is load-bearing: without it, the index resolves
            // workspace crates to the wrong copy.
            !matches!(
                e.file_name().to_str(),
                Some(
                    "node_modules"
                        | "target"
                        | ".git"
                        | ".gaffer"
                        | "dist"
                        | ".venv"
                        | "venv"
                        | "vendor"
                        | "worktrees"
                        | ".worktrees"
                )
            )
        })
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("[gaffer] graph: crate-index walk error: {}", err);
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.file_name() != "Cargo.toml" {
            continue;
        }
        let path = entry.path();
        let contents = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                // Skipping a Cargo.toml means the crate's source files won't
                // appear as `use:` resolution targets, which silently shrinks
                // the import graph. Surface it.
                eprintln!(
                    "[gaffer] graph: could not read {} for crate index: {}",
                    path.display(),
                    e,
                );
                continue;
            }
        };
        if let Some(name) = parse_crate_name(&contents) {
            // Library crate src/ is conventional. Skip workspace-only Cargo.toml
            // files (which have [workspace] but no [package]/no name).
            let crate_root = match path.parent() {
                Some(p) => p,
                None => continue,
            };
            let src = crate_root.join("src");
            if src.is_dir() {
                index.insert(name.replace('-', "_"), src);
            }
        }
    }
    index
}

/// Parse the `name = "..."` line from a `[package]` section of Cargo.toml.
/// Tiny TOML reader — proper toml crate would be cleaner, but adding a dep
/// just for this is overkill for a string-pluck.
fn parse_crate_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('[') {
            in_package = trimmed == "[package]";
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = trimmed.strip_prefix("name") {
            // name = "foo"
            let value = rest.trim_start();
            let value = value.strip_prefix('=')?.trim();
            let value = value.trim_matches('"').trim_matches('\'');
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// Strip Rust comments. Naive but matches the TS extractor's approach;
/// false-positive direction (counting an import inside a string literal) is safe.
fn strip_rust_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i = (i + 2).min(bytes.len());
        } else {
            out.push(bytes[i] as char);
            i += 1;
        }
    }
    out
}

fn canonicalize_within(path: &Path, project_root: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let root = project_root.canonicalize().ok()?;
    canonical
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_path_buf())
        .or(Some(canonical))
}

/// Detect whether a Rust source file contains an inline `#[cfg(test)]` test
/// module. Used by `affected.rs::find_affected_tests_with_graph` to surface
/// the file itself as its own test target — the dominant Rust test layout.
pub fn has_inline_tests(source: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?m)^\s*#\[cfg\(test\)\]").expect("inline-test cfg regex compiles")
    });
    re.is_match(source)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_mod_declarations() {
        let src = r#"
            pub mod affected;
            mod private_thing;
            pub mod intel { /* inline mod, no edge */ }
        "#;
        let extractor = RustExtractor::new();
        let imports = extractor.extract_imports(src);
        assert!(imports.contains(&"mod:affected".to_string()));
        assert!(imports.contains(&"mod:private_thing".to_string()));
        // Inline `mod intel { ... }` should NOT be extracted (no `;`).
        assert!(!imports.iter().any(|s| s.contains("intel")));
    }

    #[test]
    fn extracts_use_for_external_crates_only() {
        let src = r#"
            use std::collections::HashMap;
            use crate::affected;
            use self::foo;
            use super::bar;
            use gaffer_parsers::JUnitParser;
            use serde_json;
        "#;
        let extractor = RustExtractor::new();
        let imports = extractor.extract_imports(src);
        assert!(imports.contains(&"use:gaffer_parsers".to_string()));
        assert!(imports.contains(&"use:serde_json".to_string()));
        // std/crate/self/super are filtered.
        assert!(!imports.iter().any(|s| s.contains("std")));
        assert!(!imports.iter().any(|s| s.contains("crate")));
        assert!(!imports.iter().any(|s| s.contains("self")));
        assert!(!imports.iter().any(|s| s.contains("super")));
    }

    #[test]
    fn ignores_use_in_comments() {
        let src = r#"
            // use fake_crate::Thing;
            /* use other_fake; */
            use real_crate::Real;
        "#;
        let extractor = RustExtractor::new();
        let imports = extractor.extract_imports(src);
        assert_eq!(imports, vec!["use:real_crate".to_string()]);
    }

    #[test]
    fn resolve_mod_from_lib_rs_finds_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "pub mod affected;").unwrap();
        std::fs::write(src.join("affected.rs"), "// implementation").unwrap();

        let extractor = RustExtractor::new();
        let resolved = extractor.resolve("mod:affected", &src.join("lib.rs"), dir.path());
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("affected.rs"));
    }

    #[test]
    fn resolve_mod_finds_dir_form() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let intel = src.join("intel");
        std::fs::create_dir_all(&intel).unwrap();
        std::fs::write(src.join("lib.rs"), "pub mod intel;").unwrap();
        std::fs::write(intel.join("mod.rs"), "// intel root").unwrap();

        let extractor = RustExtractor::new();
        let resolved = extractor.resolve("mod:intel", &src.join("lib.rs"), dir.path());
        let path = resolved.expect("expected to resolve mod:intel");
        let s = path.to_string_lossy();
        assert!(s.ends_with("intel/mod.rs") || s.ends_with("intel\\mod.rs"), "got {}", s);
    }

    #[test]
    fn resolve_use_finds_workspace_crate_lib() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("packages/my-crate");
        let src = pkg.join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(
            pkg.join("Cargo.toml"),
            "[package]\nname = \"my-crate\"\nversion = \"0.1.0\"\n",
        )
        .unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn x() {}").unwrap();

        let extractor = RustExtractor::new();
        // Note: `my-crate` becomes `my_crate` in import paths.
        let resolved = extractor.resolve(
            "use:my_crate",
            // importing_file is irrelevant for use: resolution; just needs to exist
            &src.join("lib.rs"),
            dir.path(),
        );
        assert!(resolved.is_some(), "expected to resolve my_crate to its lib.rs");
        assert!(resolved.unwrap().ends_with("lib.rs"));
    }

    #[test]
    fn parse_crate_name_basic() {
        let toml = r#"
[package]
name = "gaffer-parsers"
version = "0.1.0"

[dependencies]
foo = "1.0"
        "#;
        assert_eq!(parse_crate_name(toml), Some("gaffer-parsers".to_string()));
    }

    #[test]
    fn parse_crate_name_skips_workspace_only() {
        let toml = r#"
[workspace]
members = ["packages/*"]
        "#;
        assert_eq!(parse_crate_name(toml), None);
    }

    #[test]
    fn detects_inline_tests() {
        assert!(has_inline_tests(
            r#"
            fn x() {}

            #[cfg(test)]
            mod tests {
                use super::*;
                #[test] fn it_works() {}
            }
            "#
        ));
    }

    #[test]
    fn does_not_falsely_detect_inline_tests() {
        assert!(!has_inline_tests(
            r#"
            // #[cfg(test)] in a comment doesn't count
            fn x() {}
            "#
        ));
        assert!(!has_inline_tests("fn x() {}"));
    }
}
