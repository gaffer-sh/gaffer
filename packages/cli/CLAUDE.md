# Gaffer CLI — Working Instructions

Rust binary crate. Source in `src/`. Built artifact at `target/release/gaffer`.

The root `CLAUDE.md` already requires invoking the `rust` skill before editing any `.rs` file under this package — that still applies.

## Build

```sh
cargo build --release --manifest-path packages/cli/Cargo.toml
```

`cargo run` from the repo root works too:

```sh
cargo run --release --manifest-path packages/cli/Cargo.toml -- affected-tests --files <paths>
```

## Dogfooding — required workflow when changing affected-tests

Unit tests in `packages/gaffer-core/src/affected.rs` cover invariants on synthetic fixture trees. They do **not** validate qualitative behavior on real repos. When iterating on `affected-tests` you must also run the built binary against the gaffer monorepo itself and inspect the output.

### 1. Build before testing

```sh
cargo build --release --manifest-path packages/cli/Cargo.toml
```

### 2. Run against this repo

```sh
# A few representative scenarios — pick whichever maps to the change you're testing
./packages/cli/target/release/gaffer affected-tests --files packages/gaffer-core/src/affected.rs --format json
./packages/cli/target/release/gaffer affected-tests --files packages/gaffer-parsers/src/junit.rs --format json
./packages/cli/target/release/gaffer affected-tests --files packages/cli/src/commands/affected_tests.rs --format json

# Or pull from a real diff:
git diff --name-only HEAD~1 \
  | xargs -I{} ./packages/cli/target/release/gaffer affected-tests --files {} --format json
```

### 3. Diff heuristic vs `--graph` strategies

The graph layer is opt-in via `--graph` (no cargo feature flag — runtime toggle). Capture before/after on the same input:

```sh
# Heuristic only (baseline)
./target/release/gaffer affected-tests --files <files> --format json > /tmp/baseline.json

# With the graph layer on
./target/release/gaffer affected-tests --files <files> --format json --graph > /tmp/with-graph.json

diff /tmp/baseline.json /tmp/with-graph.json
```

Baseline outputs for representative scenarios are tracked alongside the integration tests that exercise this monorepo's actual layout. Update them deliberately, with reasoning, when behavior changes.

#### Graph cache (`--graph` default behavior)

When you pass `--graph`, the import graph is persisted to `<project>/.gaffer/graph.db` (SQLite). First call walks the project; subsequent calls re-extract only files whose mtime has changed (~10–50× speedup on a clean working tree).

Useful flags for development:

```sh
# Force a fresh in-memory build, bypassing the cache. Useful when iterating
# on the algorithm so stale edges don't pollute results.
./target/release/gaffer affected-tests --files <files> --graph --no-cache

# Reset the cache — delete and let the next call rebuild from scratch.
rm -rf .gaffer/graph.db
```

If the cache fails (corrupt DB, permission denied), the CLI prints a warning to stderr and falls back to in-memory automatically. Pass `--no-cache` to silence the warning when you know the cache won't be available.

### 4. Verify the suggested run command actually selects something real

```sh
$(./packages/cli/target/release/gaffer affected-tests --files <files> --format json | jq -r .run_command) --reporter=verbose
```

If the framework can't find the spec, the affected-tests output is wrong regardless of what unit tests say.

### 5. Report binary size on size-affecting changes

When adding parsers or other large deps, capture:

```sh
ls -la packages/cli/target/release/gaffer
```

…before and after, and include both numbers in the PR description.

## Release

CLI is released independently via `cargo-release`. See `docs/releases.md` and `scripts/sync-cli-repo.sh` (the public mirror at `gaffer-sh/gaffer`).

## Skill sync

When changing CLI behavior, `.claude/rules/cli-skill.md` lists which skill files in `.claude/skills/gaffer-cli/` need updating to match.
