# Gaffer CLI

Test analytics and intelligence for your CI pipeline. Run tests, detect flaky tests, track health trends, and sync results to [gaffer.sh](https://gaffer.sh).

## Install

```sh
# curl
curl -fsSL https://app.gaffer.sh/install.sh | sh

# Homebrew
brew install gaffer-sh/tap/gaffer

# From source
cargo install --path packages/cli
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
