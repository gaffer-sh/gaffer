//! TypeScript / JavaScript import extractor (regex-based, v0.1).
//!
//! Handles the common static import forms:
//!   import X from 'path'
//!   import { X } from 'path'
//!   import * as X from 'path'
//!   import 'path'                  (side-effect)
//!   import('path')                 (dynamic)
//!   require('path')                (CommonJS)
//!   export ... from 'path'         (re-export)
//!
//! Resolution rules (intra-project only):
//!   - Relative specifiers (./ ../) resolve against the importing file's dir
//!   - Specifiers without an extension try .ts/.tsx/.js/.jsx/.mjs in order,
//!     plus /index.ts variants for directory imports
//!   - Bare specifiers (`react`, `@scope/pkg`) return None — out of scope

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use regex::Regex;

use super::ImportExtractor;

/// Resolution candidates tried in order for an extension-less import.
/// Must stay in sync with `extensions()` below — anything we walk is also
/// something `./foo` should be allowed to resolve to.
const RESOLVE_EXTENSIONS: &[&str] = &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"];

pub struct TypeScriptExtractor;

impl TypeScriptExtractor {
    pub fn new() -> Self {
        Self
    }
}

impl Default for TypeScriptExtractor {
    fn default() -> Self {
        Self::new()
    }
}

fn import_regex() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        // Captures the path from each shape. `[^'"`]+` matches the specifier
        // up to the closing quote (no support for template-literal imports).
        Regex::new(
            r#"(?x)
            (?:
              # import ... from 'path' / export ... from 'path'
              \b(?:import|export)\b [^'"`;]*? \bfrom\s*['"`]([^'"`]+)['"`]
            |
              # bare side-effect import: import 'path'
              \bimport\s*['"`]([^'"`]+)['"`]
            |
              # dynamic import('path')
              \bimport\s*\(\s*['"`]([^'"`]+)['"`]\s*\)
            |
              # CommonJS require('path')
              \brequire\s*\(\s*['"`]([^'"`]+)['"`]\s*\)
            )
            "#,
        )
        .expect("static import regex compiles")
    })
}

impl ImportExtractor for TypeScriptExtractor {
    fn extensions(&self) -> &'static [&'static str] {
        &["ts", "tsx", "js", "jsx", "mjs", "cjs", "mts", "cts"]
    }

    fn extract_imports(&self, source: &str) -> Vec<String> {
        // Strip line + block comments to avoid false positives from import
        // statements buried in commented-out code or JSDoc examples.
        let cleaned = strip_comments(source);

        let mut specifiers = Vec::new();
        for caps in import_regex().captures_iter(&cleaned) {
            // Each alternation produces a capture group; one of them is set.
            for i in 1..=4 {
                if let Some(m) = caps.get(i) {
                    specifiers.push(m.as_str().to_string());
                    break;
                }
            }
        }
        specifiers
    }

    fn resolve(&self, specifier: &str, importing_file: &Path, project_root: &Path) -> Option<PathBuf> {
        // Bare specifiers (no leading ./, ../, or /) are external packages or
        // path-mapped aliases. Skipping aliases is a known v0.1 limitation —
        // tsconfig.json `paths` resolution is significant follow-up work.
        let is_relative = specifier.starts_with("./") || specifier.starts_with("../");
        let is_absolute = specifier.starts_with('/');
        if !is_relative && !is_absolute {
            return None;
        }

        let base = if is_absolute {
            project_root.to_path_buf()
        } else {
            importing_file.parent()?.to_path_buf()
        };
        let raw = base.join(specifier.trim_start_matches('/'));

        try_resolve_path(&raw, project_root)
    }
}

/// Try a path with each candidate extension, falling back to `path/index.<ext>`.
fn try_resolve_path(raw: &Path, project_root: &Path) -> Option<PathBuf> {
    // Exact match (specifier already has a recognized extension).
    if raw.exists() && raw.is_file() {
        return canonicalize_within(raw, project_root);
    }

    // Try appending each extension.
    for ext in RESOLVE_EXTENSIONS {
        let with_ext = raw.with_extension(ext);
        if with_ext.exists() && with_ext.is_file() {
            return canonicalize_within(&with_ext, project_root);
        }
    }

    // Try as a directory: <raw>/index.<ext>.
    if raw.is_dir() {
        for ext in RESOLVE_EXTENSIONS {
            let idx = raw.join(format!("index.{}", ext));
            if idx.exists() && idx.is_file() {
                return canonicalize_within(&idx, project_root);
            }
        }
    }

    None
}

/// Return the path made relative to `project_root` if it lives inside the tree.
/// Falls back to canonicalized absolute path otherwise (resolves `..` segments).
fn canonicalize_within(path: &Path, project_root: &Path) -> Option<PathBuf> {
    let canonical = path.canonicalize().ok()?;
    let root = project_root.canonicalize().ok()?;
    canonical
        .strip_prefix(&root)
        .ok()
        .map(|p| p.to_path_buf())
        .or(Some(canonical))
}

/// Strip `// ...` line comments and `/* ... */` block comments.
/// Naive — does not respect string literals, but good enough that an import
/// inside a string won't be missed (false-positive direction is safe).
fn strip_comments(source: &str) -> String {
    let mut out = String::with_capacity(source.len());
    let bytes = source.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'/' {
            // Line comment until \n.
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            // Block comment until */.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_named_imports() {
        let src = r#"import { foo, bar } from './utils';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./utils".to_string()]);
    }

    #[test]
    fn extracts_default_import() {
        let src = r#"import React from 'react';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["react".to_string()]);
    }

    #[test]
    fn extracts_namespace_import() {
        let src = r#"import * as utils from './utils';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./utils".to_string()]);
    }

    #[test]
    fn extracts_side_effect_import() {
        let src = r#"import './polyfills';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./polyfills".to_string()]);
    }

    #[test]
    fn extracts_dynamic_import() {
        let src = r#"const mod = await import('./lazy');"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./lazy".to_string()]);
    }

    #[test]
    fn extracts_require() {
        let src = r#"const fs = require('fs');"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["fs".to_string()]);
    }

    #[test]
    fn extracts_re_export() {
        let src = r#"export { foo } from './foo';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./foo".to_string()]);
    }

    #[test]
    fn extracts_import_type() {
        // `import type` is the dominant pattern in Vue/Nuxt projects — the
        // regex must treat it just like a normal import. Lock that in.
        let src = r#"import type { User } from './types';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./types".to_string()]);
    }

    #[test]
    fn extracts_inline_type_specifier() {
        let src = r#"import { type User, getUser } from './users';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./users".to_string()]);
    }

    #[test]
    fn extracts_export_type_from() {
        let src = r#"export type { User } from './types';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./types".to_string()]);
    }

    #[test]
    fn extracts_export_star_from() {
        let src = r#"export * from './module';"#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./module".to_string()]);
    }

    #[test]
    fn extracts_multi_line_import() {
        // Imports broken across lines are common in formatted code.
        let src = "import {\n  Foo,\n  Bar,\n} from './shared';";
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./shared".to_string()]);
    }

    #[test]
    fn resolves_cts_and_mts_extensions() {
        // CommonJS-typed and ES-module-typed TypeScript files must resolve
        // when imported without an extension — otherwise edges into them
        // silently disappear.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("foo.ts"), "").unwrap();
        std::fs::write(src.join("commonjs.cts"), "").unwrap();
        std::fs::write(src.join("modular.mts"), "").unwrap();

        let extractor = TypeScriptExtractor::new();
        let from = src.join("foo.ts");
        let cjs = extractor.resolve("./commonjs", &from, dir.path());
        assert!(cjs.is_some_and(|p| p.ends_with("commonjs.cts")));
        let mjs = extractor.resolve("./modular", &from, dir.path());
        assert!(mjs.is_some_and(|p| p.ends_with("modular.mts")));
    }

    #[test]
    fn extracts_multiple_imports_in_one_file() {
        let src = r#"
            import { a } from './a';
            import b from '../b';
            import * as c from 'c';
            export { d } from './d';
        "#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports.len(), 4);
        assert!(imports.contains(&"./a".to_string()));
        assert!(imports.contains(&"../b".to_string()));
        assert!(imports.contains(&"c".to_string()));
        assert!(imports.contains(&"./d".to_string()));
    }

    #[test]
    fn ignores_imports_in_line_comments() {
        let src = r#"
            // import { fake } from './fake';
            import { real } from './real';
        "#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./real".to_string()]);
    }

    #[test]
    fn ignores_imports_in_block_comments() {
        let src = r#"
            /* import { fake } from './fake'; */
            import { real } from './real';
        "#;
        let imports = TypeScriptExtractor::new().extract_imports(src);
        assert_eq!(imports, vec!["./real".to_string()]);
    }

    #[test]
    fn resolve_returns_none_for_bare_specifier() {
        let extractor = TypeScriptExtractor::new();
        let result = extractor.resolve(
            "react",
            Path::new("/project/src/foo.ts"),
            Path::new("/project"),
        );
        assert_eq!(result, None);
    }

    #[test]
    fn resolve_finds_relative_with_extension() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("foo.ts"), "").unwrap();
        std::fs::write(src.join("bar.ts"), "").unwrap();

        let extractor = TypeScriptExtractor::new();
        let resolved = extractor.resolve(
            "./bar",
            &src.join("foo.ts"),
            dir.path(),
        );
        assert!(resolved.is_some(), "should resolve ./bar to bar.ts");
        let p = resolved.unwrap();
        assert!(p.ends_with("bar.ts"), "got {:?}", p);
    }

    #[test]
    fn resolve_finds_index_in_directory() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        let utils = src.join("utils");
        std::fs::create_dir_all(&utils).unwrap();
        std::fs::write(src.join("foo.ts"), "").unwrap();
        std::fs::write(utils.join("index.ts"), "").unwrap();

        let extractor = TypeScriptExtractor::new();
        let resolved = extractor.resolve(
            "./utils",
            &src.join("foo.ts"),
            dir.path(),
        );
        assert!(resolved.is_some(), "should resolve ./utils to utils/index.ts");
        let p = resolved.unwrap();
        assert!(p.ends_with("index.ts"), "got {:?}", p);
    }
}
