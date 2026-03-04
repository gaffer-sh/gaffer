# Gaffer CLI

Test analytics and intelligence for your CI pipeline. Run tests, detect flaky tests, track health trends, and sync results to [gaffer.sh](https://gaffer.sh).

## Install

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
| `--token <token>` | Auth token (overrides `GAFFER_TOKEN` env var and config) |
| `--root <path>` | Project root directory (default: `.`) |

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
