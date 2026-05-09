//! Python import extractor (v0.1).
//!
//! Handles the standard import forms:
//!   import foo
//!   import foo.bar
//!   import foo as alias
//!   from foo import bar
//!   from foo.bar import baz
//!   from . import foo                  (relative, current package)
//!   from .foo import bar               (relative, sibling module)
//!   from ..foo.bar import baz          (relative, parent package)
//!
//! Resolution rules:
//!   - Relative imports resolve against the importing file's parent dir,
//!     walking up one level per leading `.` past the first.
//!   - Absolute imports (`import foo` / `from foo import bar`) need a
//!     package index since Python's resolution depends on `sys.path`,
//!     which we don't have. v0.1: walk for `__init__.py` files and build
//!     a package-name → directory index. If `foo` is found, edge to its
//!     `__init__.py`. Multiple matches → closest to project root wins
//!     (depth-first preference).
//!
//! Tests are a separate concern: pytest convention is `test_*.py` (already
//! covered by `TEST_PREFIXES`) plus `*_test.py` (added to `TEST_SUFFIXES`
//! for this language).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use regex::Regex;
use walkdir::WalkDir;

use super::ImportExtractor;

/// Encodes whether the specifier is relative (and how many `..` levels) or
/// absolute, plus the dotted path. Format examples:
///   "abs:foo.bar"
///   "rel:0:foo"        (single dot — current package)
///   "rel:1:foo.bar"    (..foo.bar — parent package)
const ABS_PREFIX: &str = "abs:";
const REL_PREFIX: &str = "rel:";

pub struct PythonExtractor {
    /// Lazy package index: dotted package name → its `__init__.py` path or
    /// module file path. Built once on first `resolve()` call.
    package_index: Mutex<Option<HashMap<String, PathBuf>>>,
}

impl PythonExtractor {
    pub fn new() -> Self {
        Self {
            package_index: Mutex::new(None),
        }
    }

    fn ensure_index(&self, project_root: &Path) -> HashMap<String, PathBuf> {
        let mut guard = self.package_index.lock().expect("package_index lock");
        if let Some(idx) = guard.as_ref() {
            return idx.clone();
        }
        let idx = build_package_index(project_root);
        *guard = Some(idx.clone());
        idx
    }
}

impl Default for PythonExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn import_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Two shapes:
        //   from <dots><module> import ...
        //   import <module>[, <module>...]
        Regex::new(
            r"(?xm)
            ^[\ \t]*
            (?:
              from \s+ (\.+) ([\w.]*) \s+ import
            |
              from \s+ ([\w.]+) \s+ import
            |
              import \s+ ([\w.]+)
            )
            ",
        )
        .expect("python import regex compiles")
    })
}

impl ImportExtractor for PythonExtractor {
    fn extensions(&self) -> &'static [&'static str] {
        &["py"]
    }

    fn extract_imports(&self, source: &str) -> Vec<String> {
        let cleaned = strip_python_comments(source);
        let mut specs = Vec::new();

        for caps in import_regex().captures_iter(&cleaned) {
            // Group 1 = dots, Group 2 = optional name (relative)
            // Group 3 = absolute `from X import ...`
            // Group 4 = absolute `import X`
            if let Some(dots) = caps.get(1) {
                let levels = dots.as_str().len().saturating_sub(1); // 1 dot = current pkg = 0 levels up
                let name = caps.get(2).map(|m| m.as_str()).unwrap_or("");
                specs.push(format!("{}{}:{}", REL_PREFIX, levels, name));
            } else if let Some(m) = caps.get(3) {
                specs.push(format!("{}{}", ABS_PREFIX, m.as_str()));
            } else if let Some(m) = caps.get(4) {
                // `import a, b, c` — captures only the first; multi-import on
                // one line is uncommon enough to skip in v0.1.
                specs.push(format!("{}{}", ABS_PREFIX, m.as_str()));
            }
        }

        specs
    }

    fn resolve(&self, specifier: &str, importing_file: &Path, project_root: &Path) -> Option<PathBuf> {
        if let Some(rest) = specifier.strip_prefix(REL_PREFIX) {
            let (levels_str, name) = rest.split_once(':')?;
            let levels: usize = levels_str.parse().ok()?;
            return resolve_relative(name, levels, importing_file, project_root);
        }
        if let Some(name) = specifier.strip_prefix(ABS_PREFIX) {
            return resolve_absolute(name, &self.ensure_index(project_root), project_root);
        }
        None
    }
}

/// Resolve a relative import (`from .foo import bar`, `from ..pkg import baz`)
/// against the importing file's package directory.
fn resolve_relative(
    name: &str,
    levels_up: usize,
    importing_file: &Path,
    project_root: &Path,
) -> Option<PathBuf> {
    // The first dot means "current package" (the file's parent dir). Each
    // additional dot walks up one level. So `from . import x` is levels=0
    // and lives in the file's parent directory; `from .. import x` walks
    // up once more.
    let mut base = importing_file.parent()?.to_path_buf();
    for _ in 0..levels_up {
        base = base.parent()?.to_path_buf();
    }

    if name.is_empty() {
        // `from . import foo` — target is `foo.py` or `foo/__init__.py` in base
        return None; // The actual `foo` is the next clause; we can't tell from this regex alone.
    }

    resolve_dotted(name, &base, project_root)
}

/// Resolve an absolute import (`import foo.bar`) against the package index.
/// First tries the full dotted path; falls back to the leading segment.
fn resolve_absolute(
    name: &str,
    index: &HashMap<String, PathBuf>,
    project_root: &Path,
) -> Option<PathBuf> {
    // Try most-specific match first. `import foo.bar.baz` should prefer
    // foo.bar.baz over foo.bar over foo.
    let parts: Vec<&str> = name.split('.').collect();
    for end in (1..=parts.len()).rev() {
        let key = parts[..end].join(".");
        if let Some(path) = index.get(&key) {
            return canonicalize_within(path, project_root);
        }
    }
    None
}

/// Walk a dotted name (`foo.bar`) under `base`: try `foo/bar.py`, then
/// `foo/bar/__init__.py`, then peel from the right (`foo.py`).
fn resolve_dotted(name: &str, base: &Path, project_root: &Path) -> Option<PathBuf> {
    let parts: Vec<&str> = name.split('.').collect();

    // Try as-file: base/foo/bar.py
    let mut path = base.to_path_buf();
    for p in &parts[..parts.len() - 1] {
        path.push(p);
    }
    path.push(format!("{}.py", parts.last().unwrap()));
    if path.is_file() {
        return canonicalize_within(&path, project_root);
    }

    // Try as-package: base/foo/bar/__init__.py
    let mut path = base.to_path_buf();
    for p in &parts {
        path.push(p);
    }
    path.push("__init__.py");
    if path.is_file() {
        return canonicalize_within(&path, project_root);
    }

    None
}

/// Build a Python package index by walking for `__init__.py` files.
/// The package's dotted name is derived from the directory chain.
/// Multiple matches: closest-to-root wins (filtered by depth in WalkDir order).
fn build_package_index(project_root: &Path) -> HashMap<String, PathBuf> {
    let mut index: HashMap<String, PathBuf> = HashMap::new();

    for entry in WalkDir::new(project_root)
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
                        | ".venv"
                        | "venv"
                        | "vendor"
                        | "__pycache__"
                        | "worktrees"
                        | ".worktrees"
                )
            )
        })
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("[gaffer] graph: package-index walk error: {}", err);
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }

        let path = entry.path();
        let filename = path.file_name().and_then(|f| f.to_str());

        // Bare module: foo.py at any level outside a package.
        if let Some(name) = filename.and_then(|n| n.strip_suffix(".py")) {
            if name == "__init__" {
                // Handle below in the package case.
            } else {
                // Module name = file stem; we can't tell its full dotted name
                // without knowing if its dir has an __init__.py. Defer to the
                // __init__.py loop, which establishes package roots; add bare
                // modules in a second pass.
            }
        }

        if filename != Some("__init__.py") {
            continue;
        }

        // Package directory is the file's parent. Build the dotted name by
        // walking up while each ancestor ALSO has an __init__.py.
        let pkg_dir = match path.parent() {
            Some(p) => p,
            None => continue,
        };

        let mut segments: Vec<String> = Vec::new();
        let mut cur = Some(pkg_dir);
        while let Some(d) = cur {
            if d == project_root {
                break;
            }
            // Stop if this ancestor isn't itself a package (no __init__.py).
            if d != pkg_dir && !d.join("__init__.py").is_file() {
                break;
            }
            if let Some(name) = d.file_name().and_then(|n| n.to_str()) {
                segments.push(name.to_string());
            }
            cur = d.parent();
        }
        segments.reverse();
        if segments.is_empty() {
            continue;
        }
        let dotted = segments.join(".");
        // Closest-to-root wins: only insert if not already present.
        index.entry(dotted).or_insert_with(|| path.to_path_buf());
    }

    // Second pass: index modules inside packages as `<pkg>.<module>` keys.
    // For `mypkg/util.py`, where `mypkg` is a known package, register
    // `mypkg.util → mypkg/util.py`. This is what makes `import mypkg.util`
    // resolvable.
    let package_dirs: Vec<(String, PathBuf)> = index
        .iter()
        .filter_map(|(name, init_path)| init_path.parent().map(|p| (name.clone(), p.to_path_buf())))
        .collect();
    for (pkg_name, pkg_dir) in package_dirs {
        if let Ok(entries) = fs::read_dir(&pkg_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_file() {
                    continue;
                }
                let stem = match path.file_stem().and_then(|s| s.to_str()) {
                    Some(s) if s != "__init__" => s.to_string(),
                    _ => continue,
                };
                if path.extension().and_then(|e| e.to_str()) != Some("py") {
                    continue;
                }
                let qualified = format!("{}.{}", pkg_name, stem);
                index.entry(qualified).or_insert(path);
            }
        }
    }

    // Third pass: add bare modules (foo.py) that live at the project root or
    // in directories that ARE packages (siblings of __init__.py). These are
    // findable as `import foo` from anywhere.
    let mut bare_modules: HashMap<String, PathBuf> = HashMap::new();
    for entry in WalkDir::new(project_root)
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
                        | ".venv"
                        | "venv"
                        | "vendor"
                        | "__pycache__"
                        | "worktrees"
                        | ".worktrees"
                )
            )
        })
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                eprintln!("[gaffer] graph: package-index walk error: {}", err);
                None
            }
        })
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let stem = match path.file_stem().and_then(|s| s.to_str()) {
            Some(s) if s != "__init__" => s,
            _ => continue,
        };
        if path.extension().and_then(|e| e.to_str()) != Some("py") {
            continue;
        }
        // Bare modules at project root level only — sub-package modules need
        // to be addressed by their package-qualified name, handled above.
        let parent = match path.parent() {
            Some(p) => p,
            None => continue,
        };
        if parent == project_root {
            bare_modules
                .entry(stem.to_string())
                .or_insert_with(|| path.to_path_buf());
        }
    }
    for (k, v) in bare_modules {
        index.entry(k).or_insert(v);
    }

    index
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

/// Strip Python `# ...` line comments. No block comments in Python (triple-
/// quoted strings are NOT comments — they're string literals — but we don't
/// want to strip them either, since import statements inside docstrings would
/// be a false-positive direction we accept).
fn strip_python_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    for line in source.lines() {
        // Naive: doesn't handle `#` inside string literals. Real edge case.
        if let Some(idx) = line.find('#') {
            out.push_str(&line[..idx]);
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_simple_import() {
        let src = "import foo";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["abs:foo".to_string()]);
    }

    #[test]
    fn extracts_dotted_import() {
        let src = "import foo.bar.baz";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["abs:foo.bar.baz".to_string()]);
    }

    #[test]
    fn extracts_from_import() {
        let src = "from foo.bar import baz";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["abs:foo.bar".to_string()]);
    }

    #[test]
    fn extracts_relative_import_one_dot() {
        let src = "from .foo import bar";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["rel:0:foo".to_string()]);
    }

    #[test]
    fn extracts_relative_import_two_dots() {
        let src = "from ..pkg.foo import bar";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["rel:1:pkg.foo".to_string()]);
    }

    #[test]
    fn ignores_imports_in_comments() {
        let src = "# from fake import x\nfrom real import y";
        let imports = PythonExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["abs:real".to_string()]);
    }

    #[test]
    fn extracts_multiple_imports() {
        let src = r#"
import os
from typing import List, Dict
from .utils import helper
import myapp.models
        "#;
        let imports = PythonExtractor::new().extract_imports(src);
        assert!(imports.contains(&"abs:os".to_string()));
        assert!(imports.contains(&"abs:typing".to_string()));
        assert!(imports.contains(&"rel:0:utils".to_string()));
        assert!(imports.contains(&"abs:myapp.models".to_string()));
    }

    #[test]
    fn resolve_relative_finds_sibling() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("util.py"), "").unwrap();
        std::fs::write(pkg.join("main.py"), "from .util import x").unwrap();

        let extractor = PythonExtractor::new();
        let resolved = extractor.resolve("rel:0:util", &pkg.join("main.py"), dir.path());
        assert!(resolved.is_some());
        assert!(resolved.unwrap().ends_with("util.py"));
    }

    #[test]
    fn resolve_absolute_via_package_index() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("util.py"), "").unwrap();

        let extractor = PythonExtractor::new();
        let resolved = extractor.resolve(
            "abs:mypkg.util",
            &dir.path().join("test_main.py"),
            dir.path(),
        );
        assert!(resolved.is_some(), "should resolve mypkg.util via package index");
        assert!(resolved.unwrap().ends_with("util.py"));
    }

    #[test]
    fn resolve_absolute_falls_back_to_package_init() {
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();

        let extractor = PythonExtractor::new();
        let resolved = extractor.resolve(
            "abs:mypkg",
            &dir.path().join("test_main.py"),
            dir.path(),
        );
        assert!(resolved.is_some(), "should resolve mypkg to its __init__.py");
        assert!(resolved.unwrap().ends_with("__init__.py"));
    }

    #[test]
    fn end_to_end_python_test_discovery() {
        // tests/test_util.py imports util.py (via mypkg.util) → graph should
        // surface the test when we ask "who is affected by util.py changing?".
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("mypkg");
        let tests = dir.path().join("tests");
        std::fs::create_dir_all(&pkg).unwrap();
        std::fs::create_dir_all(&tests).unwrap();
        std::fs::write(pkg.join("__init__.py"), "").unwrap();
        std::fs::write(pkg.join("util.py"), "def x(): pass").unwrap();
        std::fs::write(
            tests.join("test_util.py"),
            "from mypkg.util import x\ndef test_x(): assert x() is None",
        )
        .unwrap();

        let (graph, _scanned) = crate::graph::build_graph(dir.path());

        let reachable = graph.reverse_reachable(Path::new("mypkg/util.py"), 3);
        assert!(
            reachable.iter().any(|p| p.ends_with("test_util.py")),
            "expected test_util.py in reachable set, got {:?}",
            reachable,
        );
    }
}
