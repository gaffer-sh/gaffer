//! Static import-graph layer for affected-test detection.
//!
//! Builds a directed graph `file → imported_files` from source code, then
//! answers "which files transitively import this changed file?" by BFS over
//! the *reverse* adjacency. Test files in the visited set become affected
//! tests with confidence 0.7 (vs 0.9 naming-convention, 0.3 directory-proximity).
//!
//! # Library choice (v0.1)
//!
//! This module currently uses regex-based extraction per language. The plan
//! at `~/.claude/plans/i-was-just-looking-shimmering-cat.md` recommends
//! `ast-grep-core` (tree-sitter under the hood) for the multi-language story
//! — that's a planned upgrade once we expand beyond TS/JS. The trait shape
//! here (`ImportExtractor`) is engine-agnostic so swapping the implementation
//! is mechanical.
//!
//! Regex is "good enough" for TS/JS imports because the syntax is tightly
//! constrained. It will be inadequate for Rust `use` chains and Python
//! relative imports — those languages should land with ast-grep, not regex.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

mod cache;
mod go;
mod python;
mod rust;
mod typescript;
mod walker;

pub use cache::GraphCache;
pub use go::GoExtractor;
pub use python::PythonExtractor;
pub use rust::{has_inline_tests as has_inline_rust_tests, RustExtractor};
pub use typescript::TypeScriptExtractor;
pub use walker::{build_graph, build_graph_cached};

/// Per-language import extractor. One impl per language family.
pub trait ImportExtractor {
    /// File extensions this extractor handles (e.g. `["ts", "tsx", "js"]`).
    fn extensions(&self) -> &'static [&'static str];

    /// Extract import specifiers from a single source file's contents.
    /// Returns raw specifier strings (e.g. `"./utils"`, `"@gaffer/pricing"`)
    /// — resolution to actual paths happens later in the walker.
    fn extract_imports(&self, source: &str) -> Vec<String>;

    /// Resolve an import specifier to a single project-relative path.
    /// Returns `None` for external/package imports we don't track (`react`,
    /// `@gaffer/pricing`, etc.) — only intra-project edges go in the graph.
    /// Default for languages where one import → one file.
    fn resolve(&self, specifier: &str, importing_file: &Path, project_root: &Path) -> Option<PathBuf>;

    /// Resolve an import to potentially multiple targets. Default impl wraps
    /// `resolve()`. Override for languages where one import edge fans out to
    /// many files — Go imports a *directory* (a package), and any of that
    /// directory's `.go` files are reachable. The walker calls this method,
    /// not `resolve()`, so single-target languages stay one-line correct.
    fn resolve_many(
        &self,
        specifier: &str,
        importing_file: &Path,
        project_root: &Path,
    ) -> Vec<PathBuf> {
        self.resolve(specifier, importing_file, project_root)
            .into_iter()
            .collect()
    }
}

/// File → set of project-relative paths it imports.
/// Reverse adjacency is built on demand when answering blast-radius queries.
#[derive(Debug, Default)]
pub struct ImportGraph {
    /// Forward edges: file → files it imports.
    forward: HashMap<PathBuf, HashSet<PathBuf>>,
}

impl ImportGraph {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn add_edge(&mut self, from: PathBuf, to: PathBuf) {
        self.forward
            .entry(canonicalize_relative(from))
            .or_default()
            .insert(canonicalize_relative(to));
    }

    pub fn node_count(&self) -> usize {
        let mut nodes: HashSet<&PathBuf> = self.forward.keys().collect();
        for tos in self.forward.values() {
            nodes.extend(tos.iter());
        }
        nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.forward.values().map(|s| s.len()).sum()
    }

    /// All files that transitively import `target`, up to `max_depth` hops.
    /// Depth 1 = direct importers; depth 2 = importers-of-importers; etc.
    /// `target` itself is not included in the result.
    pub fn reverse_reachable(&self, target: &Path, max_depth: usize) -> HashSet<PathBuf> {
        // Match the canonicalization applied in add_edge so callers can pass
        // raw `Path::new("src/foo.ts")` and still hit edges that were stored
        // with normalized representation.
        let target = canonicalize_relative(target.to_path_buf());
        let target = target.as_path();
        // Build reverse adjacency lazily for this query.
        let mut reverse: HashMap<&PathBuf, Vec<&PathBuf>> = HashMap::new();
        for (from, tos) in &self.forward {
            for to in tos {
                reverse.entry(to).or_default().push(from);
            }
        }

        let mut visited: HashSet<PathBuf> = HashSet::new();
        let mut queue: VecDeque<(&Path, usize)> = VecDeque::new();
        queue.push_back((target, 0));

        while let Some((node, depth)) = queue.pop_front() {
            if depth >= max_depth {
                continue;
            }
            if let Some(importers) = reverse.get(&node.to_path_buf()) {
                for importer in importers {
                    if visited.insert((*importer).clone()) {
                        queue.push_back((importer.as_path(), depth + 1));
                    }
                }
            }
        }

        visited
    }
}

/// Normalize a path for use as a graph node: forward slashes only, no leading
/// `./`. Edges hydrated from the SQLite cache arrive as forward-slash strings;
/// edges from freshly-extracted source arrive as platform-native `PathBuf`s.
/// Without this normalization the same logical file can produce two distinct
/// nodes and BFS misses edges bridging the cached and re-extracted halves.
pub(super) fn canonicalize_relative(p: PathBuf) -> PathBuf {
    let s = p.to_string_lossy();
    let s = s.replace('\\', "/");
    let stripped = s.strip_prefix("./").unwrap_or(s.as_str());
    PathBuf::from(stripped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reverse_reachable_finds_transitive_importers() {
        let mut g = ImportGraph::new();
        // a.ts -> b.ts -> c.ts
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("b.ts"));
        g.add_edge(PathBuf::from("b.ts"), PathBuf::from("c.ts"));

        // Who imports c.ts (transitively, depth 2)? Both a and b.
        let reachable = g.reverse_reachable(Path::new("c.ts"), 2);
        assert!(reachable.contains(&PathBuf::from("b.ts")));
        assert!(reachable.contains(&PathBuf::from("a.ts")));
        assert_eq!(reachable.len(), 2);
    }

    #[test]
    fn reverse_reachable_respects_depth_limit() {
        let mut g = ImportGraph::new();
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("b.ts"));
        g.add_edge(PathBuf::from("b.ts"), PathBuf::from("c.ts"));

        // Depth 1 = only direct importers of c.
        let reachable = g.reverse_reachable(Path::new("c.ts"), 1);
        assert!(reachable.contains(&PathBuf::from("b.ts")));
        assert!(!reachable.contains(&PathBuf::from("a.ts")));
    }

    #[test]
    fn reverse_reachable_excludes_target_itself() {
        let mut g = ImportGraph::new();
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("b.ts"));

        let reachable = g.reverse_reachable(Path::new("b.ts"), 5);
        assert!(!reachable.contains(&PathBuf::from("b.ts")));
    }

    #[test]
    fn node_and_edge_count_dedupe_correctly() {
        let mut g = ImportGraph::new();
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("b.ts"));
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("b.ts")); // duplicate
        g.add_edge(PathBuf::from("a.ts"), PathBuf::from("c.ts"));

        assert_eq!(g.edge_count(), 2);
        assert_eq!(g.node_count(), 3);
    }
}
