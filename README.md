# Gaffer CLI

Test analytics and intelligence for your CI pipeline. Run tests, detect flaky tests, track health trends, and sync results to [gaffer.sh](https://gaffer.sh).

## Install

Homebrew (macOS, Linux):

```sh
brew install gaffer-sh/tap/gaffer
```

Or via the install script (macOS, Linux):

```sh
curl -fsSL https://app.gaffer.sh/install.sh | sh
```

## Quick Start

```sh
# Set up authentication and configure your project
gaffer init

# Run tests with analytics
gaffer test -- npm test

# Or point at existing report files
gaffer test --report results.xml -- npm test
```

## Commands

### `gaffer test -- <command>`

Wraps your test command and analyzes the results. Discovers report files (JUnit XML, Jest/Vitest JSON, Playwright JSON, CTRF, TRX) and coverage files (LCOV, Cobertura, JaCoCo, Clover), then provides:

- Pass/fail/skip/flaky counts
- Failure clustering (groups related failures by root cause)
- Health score trending
- Coverage summary
- Automatic sync to [gaffer.sh](https://gaffer.sh) dashboard

**Flags:**

| Flag | Description |
|------|-------------|
| `--report <path>` | Explicit report file path (repeatable) |
| `--format json` | JSON output to stdout (default: human-readable stderr) |
| `--show-errors` | Show full error messages and context for failed tests |
| `--compare <branch>` | Compare against the latest run on a branch (e.g. `--compare=main`) |
| `--fail-on <mode>` | Override exit code by failure classification (`new` exits 0 when only pre-existing or flaky failures exist) |
| `--affected` | Derive the wrapped command from `affected-tests`. Use with `--files`. Trailing `-- <cmd>` is ignored when set |
| `--files <paths>` | Changed source files. Only meaningful with `--affected` (repeatable) |
| `--no-graph` | With `--affected`, disable the import-graph strategy |
| `--no-cache` | With `--affected`, force an in-memory graph build instead of using `.gaffer/graph.db` |
| `--on-empty <auto\|skip\|fail>` | With `--affected`, exit-code policy when no tests are affected. `auto` (default) exits 0 only when all detection signals were available; `skip` always 0; `fail` always non-zero |
| `--token <token>` | Auth token (overrides `GAFFER_TOKEN` env var and config) |
| `--root <path>` | Project root directory (default: `.`) |

**Run only affected tests:**

```sh
# Map changed source files to affected tests and run them in one invocation
gaffer test --affected --files src/auth.ts src/api.ts
```

### `gaffer query <subcommand>`

Query local test intelligence from the `.gaffer/data.db` database. Returns JSON by default, or human-readable output with `--pretty`.

| Subcommand | Description |
|------------|-------------|
| `health` | Health score and trend |
| `flaky` | Flaky tests ranked by composite score |
| `slowest` | Top N slowest tests (`--limit`, default 10) |
| `runs` | Recent test runs with counts (`--limit`, default 20) |
| `history <test>` | Pass/fail history for a specific test (`--limit`, default 50) |
| `failures <pattern>` | Search failures by error/name pattern (`--limit`, default 50) |

### `gaffer affected-tests`

Map changed source files to relevant test specs. Returns affected test files and a suggested run command. Runs three strategies by default: naming convention, directory proximity, and static import-graph reverse-reachability.

```sh
gaffer affected-tests --files src/auth.ts src/api.ts
```

| Flag | Description |
|------|-------------|
| `--files <paths>` | Changed source files (required, repeatable) |
| `--format <human\|json>` | Output format (default: `json`) |
| `--pretty` | Human-readable output. Equivalent to `--format human` |
| `--no-graph` | Disable the import-graph strategy; use naming + proximity only |
| `--no-cache` | Force in-memory graph build instead of using `.gaffer/graph.db` |
| `--print-cmd` | Print only the bare `run_command` string to stdout; exit 1 if none is available |
| `--root <path>` | Project root directory (default: `.`) |

The JSON output includes a `signals` object listing which detection sources were attempted and which were unavailable, so callers can distinguish "no affected tests" from "ran in degraded mode."

### `gaffer init`

Interactive setup: detects your test framework, walks you through reporter configuration, and authenticates via browser.

### `gaffer sync`

Force-syncs any pending uploads that haven't been sent yet (e.g., if a previous run was interrupted).

## Configuration

`gaffer init` creates `.gaffer/config.toml` in your project root:

```toml
[auth]
token = "gaf_..."

[sync]
api_url = "https://app.gaffer.sh"
```

The token can also be set via `GAFFER_TOKEN` environment variable or `--token` flag.

## Supported Formats

### Test Reports
- JUnit XML
- Jest / Vitest JSON
- Playwright JSON
- CTRF JSON
- TRX (.NET)

### Coverage Reports
- LCOV
- Cobertura XML
- JaCoCo XML
- Clover XML

## Building from Source

```sh
git clone https://github.com/gaffer-sh/gaffer.git
cd gaffer
cargo build --release -p gaffer
```

The binary is at `target/release/gaffer`.

## License

MIT
