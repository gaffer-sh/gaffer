//! Walk a project tree and build an `ImportGraph` from all source files.
//!
//! Two entry points:
//! - `build_graph`: full in-memory build, no persistence. Suitable for
//!   tests, one-shot runs, and any caller that doesn't want to write to
//!   disk.
//! - `build_graph_cached`: incremental build backed by SQLite. Reads
//!   per-file mtimes from the cache, re-extracts only files whose mtime
//!   differs, and reuses cached edges for the rest. First run is the
//!   same cost as `build_graph` plus a write; subsequent runs are
//!   typically 10–50× faster on a clean working tree.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::Path;

use walkdir::{DirEntry, WalkDir};

use super::cache::{mtime_ns, path_to_key, GraphCache};
use super::{
    GoExtractor, ImportExtractor, ImportGraph, PythonExtractor, RustExtractor, TypeScriptExtractor,
};

/// Hard cap on files scanned. Beyond this, the graph is partial — better
/// to return *something* than to hang on a giant monorepo.
const MAX_FILES: usize = 10_000;

/// Directory names skipped during the walk (matched anywhere in the tree).
/// Conservative defaults across language ecosystems. A future improvement
/// is to honor `.gitignore` instead of (or in addition to) this list.
const SKIP_DIRS: &[&str] = &[
    // VCS / tooling
    ".git",
    ".hg",
    ".svn",
    // JS/TS ecosystem
    "node_modules",
    "dist",
    "build",
    ".next",
    ".nuxt",
    ".output",
    ".turbo",
    ".vercel",
    "coverage",
    ".cache",
    // Rust
    "target",
    // Python
    "__pycache__",
    ".venv",
    "venv",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    "site-packages",
    ".tox",
    "egg-info",
    // Ruby
    "vendor",
    ".bundle",
    // Java / Kotlin / Android / iOS
    ".gradle",
    ".idea",
    "Pods",
    "DerivedData",
    // Go vendoring lives under `vendor/` (shared with Ruby above)
    // Tool-specific
    ".gaffer",
    // Worktree-style sibling checkouts of the same repo. Without these,
    // the walker double-counts every source file and the Rust crate
    // index can resolve crates to the wrong copy.
    "worktrees",
    ".worktrees",
];

/// Bundle of language-specific extractors keyed by file extension.
/// The extractors own per-project state (crate/package/module indexes)
/// that's lazy-built on first resolution call, so the bundle should
/// outlive a single graph build but not be reused across project roots.
struct Extractors {
    extractors: Vec<Box<dyn ImportExtractor>>,
    by_ext: HashMap<&'static str, usize>,
}

impl Extractors {
    fn new() -> Self {
        let extractors: Vec<Box<dyn ImportExtractor>> = vec![
            Box::new(TypeScriptExtractor::new()),
            Box::new(RustExtractor::new()),
            Box::new(PythonExtractor::new()),
            Box::new(GoExtractor::new()),
        ];
        let mut by_ext = HashMap::new();
        for (i, ex) in extractors.iter().enumerate() {
            for ext in ex.extensions() {
                by_ext.insert(*ext, i);
            }
        }
        Self { extractors, by_ext }
    }

    fn for_extension(&self, ext: &str) -> Option<&dyn ImportExtractor> {
        self.by_ext.get(ext).map(|i| self.extractors[*i].as_ref())
    }
}

/// Build an import graph for the project rooted at `project_root`.
/// Returns the graph plus a count of files scanned (for diagnostics).
pub fn build_graph(project_root: &Path) -> (ImportGraph, usize) {
    let mut graph = ImportGraph::new();
    let mut scanned: usize = 0;
    let extractors = Extractors::new();

    for entry in walk(project_root) {
        if scanned >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let extractor = match extractors.for_extension(ext) {
            Some(e) => e,
            None => continue,
        };

        scanned += 1;
        let rel = match path.strip_prefix(project_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };

        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gaffer] graph: skipping {}: {}", rel.display(), e);
                continue;
            }
        };

        for spec in extractor.extract_imports(&source) {
            for target in extractor.resolve_many(&spec, path, project_root) {
                graph.add_edge(rel.clone(), target);
            }
        }
    }

    (graph, scanned)
}

/// Cached counterpart to `build_graph`. Reads per-file mtimes from the
/// cache; re-extracts only files whose mtime differs from the cached
/// value; deletes cache entries for files no longer in the working
/// tree.
///
/// On any cache error (corrupt DB, permission denied, etc.) the caller
/// is expected to log a warning and fall back to `build_graph`. The
/// CLI wrapper does this in `commands::affected_tests::run`.
///
/// Returns: graph, total files scanned, count of files re-extracted.
/// The "re-extracted" count is the diagnostic that tells you how much
/// work the cache saved (low number = big cache hit).
pub fn build_graph_cached(
    project_root: &Path,
    cache: &mut GraphCache,
) -> rusqlite::Result<(ImportGraph, usize, usize)> {
    let mut graph = ImportGraph::new();
    let extractors = Extractors::new();

    let cached_mtimes = cache.list_files()?;
    let mut seen_in_walk: HashSet<String> = HashSet::with_capacity(cached_mtimes.len());
    let mut scanned: usize = 0;
    let mut re_extracted: usize = 0;

    for entry in walk(project_root) {
        if scanned >= MAX_FILES {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = match path.extension().and_then(|e| e.to_str()) {
            Some(e) => e,
            None => continue,
        };
        let extractor = match extractors.for_extension(ext) {
            Some(e) => e,
            None => continue,
        };

        scanned += 1;
        let rel = match path.strip_prefix(project_root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        let key = path_to_key(&rel);
        seen_in_walk.insert(key.clone());

        // mtime comparison decides whether to re-extract.
        let mtime_read = entry.metadata().ok().and_then(|m| m.modified().ok());
        let current_mtime = mtime_read.map(mtime_ns).unwrap_or(0);

        if let Some(cached_mtime) = cached_mtimes.get(&key) {
            if *cached_mtime == current_mtime && current_mtime != 0 {
                // Unchanged: hydrate edges from cache.
                for to_key in cache.edges_from(&key)? {
                    graph.add_edge(rel.clone(), super::cache::key_to_path(&to_key));
                }
                continue;
            }
        }

        // New or modified: re-extract.
        re_extracted += 1;
        let source = match fs::read_to_string(path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[gaffer] graph: skipping {}: {}", rel.display(), e);
                continue;
            }
        };

        let mut targets_for_cache = Vec::new();
        for spec in extractor.extract_imports(&source) {
            for target in extractor.resolve_many(&spec, path, project_root) {
                targets_for_cache.push(path_to_key(&target));
                graph.add_edge(rel.clone(), target);
            }
        }
        if current_mtime != 0 {
            cache.replace_file_edges(&key, current_mtime, &targets_for_cache)?;
        } else {
            // Skip the cache write so we don't pin mtime=0, which would force
            // a re-extract on every subsequent run for this file. The graph
            // for *this* run is still correct — we just don't persist it.
            eprintln!(
                "[gaffer] graph: could not read mtime for {}; this file will not be cached",
                rel.display(),
            );
        }
    }

    // Files in cache but absent from this walk → deleted from working tree.
    // Remove them so stale edges don't pollute future reverse-reach queries.
    let stale: HashSet<String> = cached_mtimes
        .keys()
        .filter(|k| !seen_in_walk.contains(*k))
        .cloned()
        .collect();
    if !stale.is_empty() {
        cache.delete_files(&stale)?;
    }

    Ok((graph, scanned, re_extracted))
}

fn walk(project_root: &Path) -> impl Iterator<Item = walkdir::DirEntry> {
    WalkDir::new(project_root)
        .into_iter()
        .filter_entry(|e| !is_skipped_dir(e))
        .filter_map(|res| match res {
            Ok(e) => Some(e),
            Err(err) => {
                // Permission denied, broken symlink, transient I/O — keep
                // walking, but make the failure visible so users don't get
                // silently-incomplete affected-tests results.
                eprintln!("[gaffer] graph: walk error: {}", err);
                None
            }
        })
}

fn is_skipped_dir(entry: &DirEntry) -> bool {
    if !entry.file_type().is_dir() {
        return false;
    }
    entry
        .file_name()
        .to_str()
        .map(|n| SKIP_DIRS.contains(&n))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn builds_graph_for_simple_project() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();

        // a.ts imports b
        fs::write(
            src.join("a.ts"),
            "import { thing } from './b';\nexport const x = thing;",
        )
        .unwrap();
        // b.ts imports c
        fs::write(
            src.join("b.ts"),
            "import c from './c';\nexport const thing = c;",
        )
        .unwrap();
        // c.ts has no imports
        fs::write(src.join("c.ts"), "export default 42;").unwrap();

        let (graph, scanned) = build_graph(dir.path());
        assert_eq!(scanned, 3);
        assert_eq!(graph.node_count(), 3);
        assert_eq!(graph.edge_count(), 2);

        // Reverse-reachable from c should find both b and a (depth >= 2).
        let reachable = graph.reverse_reachable(Path::new("src/c.ts"), 5);
        assert!(reachable.iter().any(|p| p.ends_with("a.ts")), "got {:?}", reachable);
        assert!(reachable.iter().any(|p| p.ends_with("b.ts")), "got {:?}", reachable);
    }

    #[test]
    fn rust_cross_crate_use_chain() {
        // Mimic a workspace: packages/foo/{Cargo.toml,src/{lib.rs,inner.rs}}
        // Plus a tests/foo_tests.rs at the workspace root that does
        // `use foo::*;`. Reverse-reachable from inner.rs should include
        // foo_tests.rs at depth 3.
        let dir = tempfile::tempdir().unwrap();
        let pkg = dir.path().join("packages/foo");
        let src = pkg.join("src");
        let tests = dir.path().join("tests");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&tests).unwrap();
        fs::write(pkg.join("Cargo.toml"), "[package]\nname = \"foo\"\nversion = \"0.1.0\"\n").unwrap();
        fs::write(src.join("lib.rs"), "pub mod inner;").unwrap();
        fs::write(src.join("inner.rs"), "pub fn x() {}").unwrap();
        fs::write(tests.join("foo_tests.rs"), "use foo::x;\n#[test] fn it() {}").unwrap();

        let (graph, _scanned) = build_graph(dir.path());

        // Edges expected:
        //   tests/foo_tests.rs -> packages/foo/src/lib.rs   (cross-crate use)
        //   packages/foo/src/lib.rs -> packages/foo/src/inner.rs   (mod inner;)
        let reachable = graph.reverse_reachable(Path::new("packages/foo/src/inner.rs"), 3);
        assert!(
            reachable.iter().any(|p| p.ends_with("lib.rs")),
            "expected lib.rs in reverse-reachable, got {:?}",
            reachable,
        );
        assert!(
            reachable.iter().any(|p| p.ends_with("foo_tests.rs")),
            "expected foo_tests.rs in reverse-reachable, got {:?}",
            reachable,
        );
    }

    #[test]
    fn go_test_discovery_via_module_index() {
        // myapp module with utils package, plus an _test.go in a sibling
        // package that imports utils. Reverse-reachable from utils/a.go
        // should include the test file.
        let dir = tempfile::tempdir().unwrap();
        let mod_root = dir.path().join("myapp");
        let utils = mod_root.join("utils");
        let svc = mod_root.join("svc");
        fs::create_dir_all(&utils).unwrap();
        fs::create_dir_all(&svc).unwrap();
        fs::write(
            mod_root.join("go.mod"),
            "module github.com/example/myapp\n\ngo 1.21\n",
        )
        .unwrap();
        fs::write(utils.join("a.go"), "package utils\n").unwrap();
        fs::write(utils.join("b.go"), "package utils\n").unwrap();
        fs::write(
            svc.join("svc_test.go"),
            r#"package svc

import (
    "testing"
    "github.com/example/myapp/utils"
)

func TestThing(t *testing.T) { _ = utils.Thing }
"#,
        )
        .unwrap();

        let (graph, _scanned) = build_graph(dir.path());

        // Reverse-reachable from utils/a.go should include svc_test.go
        // (the import edge fans out to both a.go and b.go).
        let reachable = graph.reverse_reachable(Path::new("myapp/utils/a.go"), 3);
        assert!(
            reachable.iter().any(|p| p.ends_with("svc_test.go")),
            "expected svc_test.go in reachable, got {:?}",
            reachable,
        );
    }

    #[test]
    fn skips_node_modules() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let nm = dir.path().join("node_modules/some-pkg");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&nm).unwrap();

        fs::write(src.join("a.ts"), "export const x = 1;").unwrap();
        fs::write(nm.join("index.js"), "module.exports = 1;").unwrap();

        let (_, scanned) = build_graph(dir.path());
        assert_eq!(scanned, 1, "node_modules should be skipped");
    }

    #[test]
    fn cache_picks_up_real_content_change() {
        // Critical correctness test: when a file's content changes (and its
        // mtime advances), the cache must re-extract and reflect the new
        // edges — old edges from the previous content must not survive.
        // A bug that left the cache write off the modified path would still
        // pass mtime-only tests; this exercises content delta end-to-end.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("a.ts"), "import './b';").unwrap();
        fs::write(src.join("b.ts"), "").unwrap();
        fs::write(src.join("c.ts"), "").unwrap();

        let cache_path = dir.path().join("graph.db");

        // First run: a -> b
        {
            let mut cache = super::GraphCache::open(&cache_path).unwrap();
            let (graph, _scanned, _re) = build_graph_cached(dir.path(), &mut cache).unwrap();
            let reach_b = graph.reverse_reachable(Path::new("src/b.ts"), 3);
            assert!(reach_b.iter().any(|p| p.ends_with("a.ts")));
            let reach_c = graph.reverse_reachable(Path::new("src/c.ts"), 3);
            assert!(!reach_c.iter().any(|p| p.ends_with("a.ts")));
        }

        // Rewrite a.ts to import c instead of b. Bump mtime past 1s
        // resolution so platforms with coarse-grained mtime (HFS+) still see
        // the change.
        std::thread::sleep(std::time::Duration::from_millis(1100));
        fs::write(src.join("a.ts"), "import './c';").unwrap();

        // Second run: edges should reflect the new content (a -> c, NOT a -> b).
        {
            let mut cache = super::GraphCache::open(&cache_path).unwrap();
            let (graph, _scanned, _re) = build_graph_cached(dir.path(), &mut cache).unwrap();
            let reach_c = graph.reverse_reachable(Path::new("src/c.ts"), 3);
            assert!(
                reach_c.iter().any(|p| p.ends_with("a.ts")),
                "after rewrite, a.ts should reach c.ts via the new import; got {:?}",
                reach_c,
            );
            let reach_b = graph.reverse_reachable(Path::new("src/b.ts"), 3);
            assert!(
                !reach_b.iter().any(|p| p.ends_with("a.ts")),
                "stale a -> b edge survived rewrite; got {:?}",
                reach_b,
            );
        }
    }
}
