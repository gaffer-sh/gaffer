# SARIF fixtures

Real tool output, generated on 2026-11-02 in a scratch directory outside the repo
(which is why `ruff.sarif` and `eslint.sarif` carry absolute `file:///private/tmp/...` uris).

| File | Tool | How it was produced |
|---|---|---|
| `ruff.sarif` | ruff 0.17.3 | `ruff check --select E,F,W --output-format sarif src` over two small Python files |
| `eslint.sarif` | ESLint 9.39.5 with `@microsoft/eslint-formatter-sarif` 3.1.0 | `eslint -f @microsoft/eslint-formatter-sarif -o results.sarif src` with `js.configs.recommended` plus `no-console`, `eqeqeq` and `prefer-const`; one `eslint-disable-next-line` gives an `inSource` suppression |
| `clippy.sarif` | clippy 0.1.99 (rust 1.99) with clippy-sarif 0.8.0 | `cargo clippy --message-format=json \| clippy-sarif` on a small binary crate |
| `synthetic-codeql-style.sarif` | none: hand-written | Covers what the three tools above never emit: `partialFingerprints["primaryLocationLineHash/v1"]`, a result with no `level` (rule `defaultConfiguration` fallback), `rule.toolComponent` index into `tool.extensions`, `message.arguments`, a markdown-only and an empty message, a rejected and an accepted suppression, a logical-only location, and a second run with `results: null`. Shaped after CodeQL's output, but not produced by CodeQL. |
