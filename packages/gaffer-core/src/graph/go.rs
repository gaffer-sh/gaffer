//! Go import extractor (v0.1).
//!
//! Go's import model is simpler than the others in shape but different in
//! semantics: imports are *package paths* (which are directories), and every
//! `.go` file in the imported directory is part of that package. So a single
//! import edge fans out to multiple files. We use the trait's `resolve_many`
//! override.
//!
//! Imports come in two shapes:
//!   import "fmt"
//!   import (
//!       "fmt"
//!       "github.com/foo/bar"
//!       myalias "github.com/foo/baz"
//!       . "github.com/foo/qux"      // dot import
//!       _ "github.com/foo/quux"     // blank import (side effect)
//!   )
//!
//! Resolution:
//!   - Build a module index from `go.mod` files (parses the `module` line).
//!   - Map import paths starting with a known module prefix to their
//!     directory inside the module.
//!   - Imports of stdlib (`"fmt"`, `"net/http"`) and external modules return
//!     no edges — out of scope.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use walkdir::WalkDir;

use super::ImportExtractor;

pub struct GoExtractor {
    /// Lazy module index: module path (from `module github.com/foo/bar`) →
    /// the directory containing the `go.mod` file.
    module_index: Mutex<Option<HashMap<String, PathBuf>>>,
}

impl GoExtractor {
    pub fn new() -> Self {
        Self {
            module_index: Mutex::new(None),
        }
    }

    fn ensure_index(&self, project_root: &Path) -> HashMap<String, PathBuf> {
        let mut guard = self.module_index.lock().expect("module_index lock");
        if let Some(idx) = guard.as_ref() {
            return idx.clone();
        }
        let idx = build_module_index(project_root);
        *guard = Some(idx.clone());
        idx
    }
}

impl Default for GoExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn import_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Captures any `"path/to/pkg"` that appears in an `import "..."` or
        // inside an `import ( ... )` block. We accept some over-matching
        // (the regex doesn't strictly track parenthesized blocks); imports
        // resolve to None for non-matching paths anyway, so the cost is
        // negligible. The leading-pattern guard ensures we don't pick up
        // string literals from actual code.
        Regex::new(
            r#"(?xm)
            (?:
              ^[\ \t]*import[\ \t]+(?:[A-Za-z_][A-Za-z0-9_]*[\ \t]+|\.\ |_\ )?"([^"]+)"
            |
              ^[\ \t]*(?:[A-Za-z_][A-Za-z0-9_]*[\ \t]+|\.\ |_\ )?"([^"]+)"
            )
            "#,
        )
        .expect("go import regex compiles")
    })
}

impl ImportExtractor for GoExtractor {
    fn extensions(&self) -> &'static [&'static str] {
        &["go"]
    }

    fn extract_imports(&self, source: &str) -> Vec<String> {
        // Restrict matches to inside `import (...)` blocks or after `import`
        // keyword. Doing this with a single regex over arbitrary code would
        // false-positive heavily, so first we slice to the import section.
        let import_blocks = extract_import_sections(source);
        let mut specs = Vec::new();
        for block in import_blocks {
            for caps in import_regex().captures_iter(&block) {
                if let Some(m) = caps.get(1).or_else(|| caps.get(2)) {
                    specs.push(m.as_str().to_string());
                }
            }
        }
        specs
    }

    fn resolve(&self, _specifier: &str, _importing_file: &Path, _project_root: &Path) -> Option<PathBuf> {
        // Single-target resolve is unused for Go — the walker calls
        // `resolve_many`. Always return None here.
        None
    }

    fn resolve_many(
        &self,
        specifier: &str,
        _importing_file: &Path,
        project_root: &Path,
    ) -> Vec<PathBuf> {
        let index = self.ensure_index(project_root);
        // Find the longest matching module prefix. `myapp/foo/bar` matches
        // module `myapp` if no longer module also matches (`myapp/foo`).
        let mut best: Option<(&String, &PathBuf)> = None;
        for (mod_path, mod_dir) in &index {
            if specifier == mod_path || specifier.starts_with(&format!("{}/", mod_path)) {
                if best.map(|(b, _)| b.len()).unwrap_or(0) < mod_path.len() {
                    best = Some((mod_path, mod_dir));
                }
            }
        }
        let (mod_path, mod_dir) = match best {
            Some(b) => b,
            None => return Vec::new(),
        };

        // Subpath inside the module: `myapp/foo/bar` minus `myapp` → `foo/bar`.
        let sub = specifier
            .strip_prefix(mod_path)
            .unwrap_or("")
            .trim_start_matches('/');
        let pkg_dir = if sub.is_empty() {
            mod_dir.clone()
        } else {
            mod_dir.join(sub)
        };

        if !pkg_dir.is_dir() {
            return Vec::new();
        }

        // Fan out to every .go file in the directory (excluding _test.go —
        // those are part of the package's own tests, not imported targets,
        // and they shouldn't appear as edge targets from external importers).
        let mut targets = Vec::new();
        if let Ok(entries) = fs::read_dir(&pkg_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let filename = match path.file_name().and_then(|f| f.to_str()) {
                    Some(f) => f,
                    None => continue,
                };
                if !filename.ends_with(".go") {
                    continue;
                }
                if filename.ends_with("_test.go") {
                    continue;
                }
                if let Some(canonical) = canonicalize_within(&path, project_root) {
                    targets.push(canonical);
                }
            }
        }
        targets
    }
}

/// Pull out the substring(s) of source covered by `import "..."` and
/// `import (...)` blocks. Strips comments first so a commented-out
/// import block doesn't generate edges.
fn extract_import_sections(source: &str) -> Vec<String> {
    let cleaned = strip_go_comments(source);
    let mut sections = Vec::new();
    let mut chars = cleaned.char_indices().peekable();

    while let Some(&(idx, _)) = chars.peek() {
        // Look for `import` at start of line (loose: any preceding whitespace).
        if idx == 0 || cleaned.as_bytes()[idx - 1] == b'\n' {
            let rest = &cleaned[idx..];
            if let Some(after_import) = rest.strip_prefix("import") {
                let after_import = after_import.trim_start_matches([' ', '\t']);
                if after_import.starts_with('(') {
                    // Block import — find matching ).
                    if let Some(close) = after_import.find(')') {
                        sections.push(after_import[..close].to_string());
                        // Advance past the close paren.
                        let consumed = (rest.len() - after_import.len()) + close + 1;
                        for _ in 0..consumed {
                            chars.next();
                        }
                        continue;
                    }
                } else if after_import.starts_with('"')
                    || after_import.starts_with([' ', '\t'])
                        && after_import.trim_start().starts_with('"')
                {
                    // Single-import line — take just this line.
                    if let Some(eol) = after_import.find('\n') {
                        sections.push(after_import[..eol].to_string());
                    }
                }
            }
        }
        chars.next();
    }
    sections
}

fn build_module_index(project_root: &Path) -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    for entry in WalkDir::new(project_root)
        .max_depth(8)
        .into_iter()
        .filter_entry(|e| {
            !matches!(
                e.file_name().to_str(),
                Some(
                    "node_modules"
                        | "target"
                        | ".git"
                        | ".gaffer"
                        | "dist"
                        | "vendor"
                        | "worktrees"
                        | ".worktrees"
                )
            )
        })
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("[gaffer] graph: module-index walk error: {}", err);
                None
            }
        })
    {
        if !entry.file_type().is_file() || entry.file_name() != "go.mod" {
            continue;
        }
        let path = entry.path();
        let contents = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                // Skipping a go.mod means the module's package directories
                // won't fan out as resolve_many targets, which silently
                // shrinks the import graph. Surface it.
                eprintln!(
                    "[gaffer] graph: could not read {} for module index: {}",
                    path.display(),
                    e,
                );
                continue;
            }
        };
        if let Some(module_path) = parse_module_path(&contents) {
            if let Some(mod_dir) = path.parent() {
                index.insert(module_path, mod_dir.to_path_buf());
            }
        }
    }
    index
}

fn parse_module_path(go_mod: &str) -> Option<String> {
    for line in go_mod.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("module ") {
            return Some(rest.trim().to_string());
        }
    }
    None
}

fn strip_go_comments(source: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_single_import() {
        let src = r#"
package main

import "fmt"

func main() { fmt.Println("hi") }
        "#;
        let imports = GoExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["fmt".to_string()]);
    }

    #[test]
    fn extracts_block_import() {
        let src = r#"
package main

import (
    "fmt"
    "github.com/foo/bar"
    myalias "github.com/foo/baz"
    _ "github.com/foo/quux"
)
        "#;
        let imports = GoExtractor::new().extract_imports(src);
        assert!(imports.contains(&"fmt".to_string()));
        assert!(imports.contains(&"github.com/foo/bar".to_string()));
        assert!(imports.contains(&"github.com/foo/baz".to_string()));
        assert!(imports.contains(&"github.com/foo/quux".to_string()));
    }

    #[test]
    fn ignores_strings_outside_import_blocks() {
        let src = r#"
package main

import "fmt"

func main() {
    s := "not an import"
    fmt.Println(s)
}
        "#;
        let imports = GoExtractor::new().extract_imports(src);
        // Only the actual import; no false-positives from string literals.
        assert_eq!(imports, vec!["fmt".to_string()]);
    }

    #[test]
    fn ignores_imports_in_comments() {
        let src = r#"
package main

// import "fake"
/* import "alsofake" */
import "real"
        "#;
        let imports = GoExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["real".to_string()]);
    }

    #[test]
    fn parse_module_path_basic() {
        let go_mod = r#"
module github.com/foo/bar

go 1.21
        "#;
        assert_eq!(parse_module_path(go_mod), Some("github.com/foo/bar".to_string()));
    }

    #[test]
    fn resolve_many_fans_out_to_package_files() {
        let dir = tempfile::tempdir().unwrap();
        // Set up a go module at dir/myapp with go.mod
        let mod_root = dir.path().join("myapp");
        let pkg_dir = mod_root.join("utils");
        fs::create_dir_all(&pkg_dir).unwrap();
        fs::write(
            mod_root.join("go.mod"),
            "module github.com/example/myapp\n\ngo 1.21\n",
        )
        .unwrap();
        // Two .go files in the package + one _test.go (which should be excluded)
        fs::write(pkg_dir.join("a.go"), "package utils\n").unwrap();
        fs::write(pkg_dir.join("b.go"), "package utils\n").unwrap();
        fs::write(pkg_dir.join("a_test.go"), "package utils\n").unwrap();

        let extractor = GoExtractor::new();
        let importing = mod_root.join("main.go");
        let targets = extractor.resolve_many(
            "github.com/example/myapp/utils",
            &importing,
            dir.path(),
        );
        assert_eq!(targets.len(), 2, "should fan out to a.go and b.go (not a_test.go), got {:?}", targets);
        assert!(targets.iter().any(|p| p.ends_with("a.go")));
        assert!(targets.iter().any(|p| p.ends_with("b.go")));
    }

    #[test]
    fn resolve_many_returns_empty_for_unknown_module() {
        let dir = tempfile::tempdir().unwrap();
        let extractor = GoExtractor::new();
        let targets = extractor.resolve_many("fmt", &dir.path().join("main.go"), dir.path());
        assert!(targets.is_empty());
    }
}
