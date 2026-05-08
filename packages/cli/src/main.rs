//! Gaffer CLI — wraps test commands and parses artifacts for storage and analysis.
//!
//! Usage:
//!   gaffer test -- pnpm test
//!   gaffer test --config path/to/config.toml -- pnpm test
//!   gaffer sync
//!   gaffer init [--global]
//!   gaffer config list

mod commands;
mod config;
mod discovery;
mod framework;
mod git;
mod output;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand, ValueEnum};

use config::Config;

#[derive(Clone, ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
}

/// What types of failures should cause a non-zero exit code.
#[derive(Clone, ValueEnum)]
pub enum FailOn {
    /// Exit non-zero only for new failures (not pre-existing or flaky)
    New,
}

#[derive(Parser)]
#[command(name = "gaffer", about = "Test analytics and intelligence", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run a test command and analyze results
    Test {
        /// Authentication token (overrides GAFFER_TOKEN env var and config files)
        #[arg(long)]
        token: Option<String>,

        /// API URL for cloud sync (overrides GAFFER_API_URL env var)
        #[arg(long)]
        api_url: Option<String>,

        /// Path to a config file (overrides walk-up discovery)
        #[arg(long)]
        config: Option<PathBuf>,

        /// Report file path(s) to parse (can be specified multiple times)
        #[arg(long = "report", short = 'r')]
        reports: Vec<String>,

        /// Directory for local data storage (default: current directory)
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Output format: human (colored stderr) or json (stdout)
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        format: OutputFormat,

        /// Show full error messages and context files for failed tests
        #[arg(long)]
        show_errors: bool,

        /// Compare against the latest run on a branch (e.g. --compare=main)
        #[arg(long)]
        compare: Option<String>,

        /// Override exit code based on failure classification
        #[arg(long, value_enum)]
        fail_on: Option<FailOn>,

        /// The test command to run (everything after --)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Upload test reports or artifacts directly to the Gaffer dashboard.
    ///
    /// Routes small bundles (no file >90 MB AND total <200 MB) to a single
    /// multipart POST, and large files individually through R2 multipart
    /// upload (8-way concurrent part PUTs, automatic retry on 5xx/network).
    ///
    /// Examples:
    ///   gaffer upload ./test-results --token gfr_...
    ///   gaffer upload report.xml playwright-trace.zip --commit-sha $GITHUB_SHA --branch main
    ///   gaffer upload large-trace.zip --max-file-size-mb 5000 --debug
    Upload {
        /// File or directory paths to upload (one or more).
        #[arg(required = true)]
        paths: Vec<PathBuf>,

        /// Upload token (gfr_…). Falls back to GAFFER_UPLOAD_TOKEN, then GAFFER_TOKEN.
        #[arg(long)]
        token: Option<String>,

        /// Custom dashboard URL (e.g. https://preview.gaffer.sh). Falls back to GAFFER_API_URL.
        #[arg(long)]
        api_url: Option<String>,

        /// Git commit SHA recorded as a tag on the upload session.
        #[arg(long)]
        commit_sha: Option<String>,

        /// Git branch recorded as a tag on the upload session.
        #[arg(long)]
        branch: Option<String>,

        /// Test framework name (e.g. playwright, jest, vitest).
        #[arg(long)]
        test_framework: Option<String>,

        /// Optional test suite label.
        #[arg(long)]
        test_suite: Option<String>,

        /// Per-request HTTP timeout in seconds (default: 300).
        #[arg(long, default_value_t = 300)]
        timeout: u64,

        /// Per-file size limit in MB (default: 100). Files above this are rejected up front.
        #[arg(long, default_value_t = 100)]
        max_file_size_mb: u64,

        /// Print per-part throughput, session creation timing, and total wall time.
        #[arg(long)]
        debug: bool,
    },

    /// Force sync pending uploads to the Gaffer dashboard
    Sync {
        /// Authentication token (overrides GAFFER_TOKEN env var and config files)
        #[arg(long)]
        token: Option<String>,

        /// API URL for cloud sync (overrides GAFFER_API_URL env var)
        #[arg(long)]
        api_url: Option<String>,

        /// Path to a config file (overrides walk-up discovery)
        #[arg(long)]
        config: Option<PathBuf>,

        /// Directory for local data storage (default: current directory)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Interactive setup — detect framework, configure reporters, authenticate
    Init {
        /// API URL for cloud sync
        #[arg(long)]
        api_url: Option<String>,

        /// Set up global authentication (writes to ~/.config/gaffer/)
        #[arg(long)]
        global: bool,
    },

    /// Find test files affected by source file changes
    AffectedTests {
        /// Source files that changed (can be specified multiple times)
        #[arg(long = "files", required = true, num_args = 1..)]
        files: Vec<String>,

        /// Directory for local data storage (default: current directory)
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Output format: human or json
        #[arg(long, value_enum, default_value_t = OutputFormat::Json)]
        format: OutputFormat,
    },

    /// Diagnose common setup issues
    Doctor {
        /// Authentication token (overrides GAFFER_TOKEN env var and config files)
        #[arg(long)]
        token: Option<String>,

        /// API URL for cloud sync (overrides GAFFER_API_URL env var)
        #[arg(long)]
        api_url: Option<String>,

        /// Path to a config file (overrides walk-up discovery)
        #[arg(long)]
        config: Option<PathBuf>,

        /// Directory for local data storage (default: current directory)
        #[arg(long)]
        data_dir: Option<PathBuf>,
    },

    /// Query local test intelligence (health, flaky tests, durations, history)
    Query {
        /// Directory for local data storage (default: current directory)
        #[arg(long)]
        data_dir: Option<PathBuf>,

        /// Human-readable output (default: JSON)
        #[arg(long)]
        pretty: bool,

        #[command(subcommand)]
        command: QueryCommand,
    },

    /// Show resolved configuration and sources
    Config {
        /// Path to a config file (overrides walk-up discovery)
        #[arg(long)]
        config: Option<PathBuf>,

        #[command(subcommand)]
        command: ConfigCommand,
    },
}

#[derive(Subcommand)]
enum QueryCommand {
    /// Health score and trend
    Health,

    /// Flaky tests ranked by composite score
    Flaky,

    /// Top N slowest tests by duration
    Slowest {
        /// Number of tests to show
        #[arg(long, default_value_t = 10)]
        limit: u32,
    },

    /// Recent test runs with counts
    Runs {
        /// Number of runs to show
        #[arg(long, default_value_t = 20)]
        limit: u32,
    },

    /// Pass/fail history for a specific test
    History {
        /// Test name pattern to search for
        test: String,

        /// Number of entries to show
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },

    /// Search failures by error/name pattern
    Failures {
        /// Error or test name pattern to search for
        pattern: String,

        /// Number of results to show
        #[arg(long, default_value_t = 50)]
        limit: u32,
    },
}

#[derive(Subcommand)]
pub enum ConfigCommand {
    /// Show resolved configuration with sources
    List,
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Test {
            token,
            api_url,
            config,
            reports,
            data_dir,
            format,
            show_errors,
            compare,
            fail_on,
            command,
        } => {
            let project_root = resolve_data_dir(data_dir.as_ref());
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &reports,
                &project_root,
                config.as_deref(),
            );
            match commands::test::run(&config, &command, &reports, &format, show_errors, compare.as_deref(), fail_on.as_ref()) {
                Ok(exit_code) => process::exit(exit_code),
                Err(e) => {
                    eprintln!("[gaffer] Error: {:#}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Upload {
            paths,
            token,
            api_url,
            commit_sha,
            branch,
            test_framework,
            test_suite,
            timeout,
            max_file_size_mb,
            debug,
        } => {
            let code = commands::upload::run(commands::upload::UploadArgs {
                paths,
                token,
                api_url,
                commit_sha,
                branch,
                test_framework,
                test_suite,
                timeout_secs: timeout,
                max_file_size_mb,
                debug,
            });
            process::exit(code);
        }
        Commands::Sync {
            token,
            api_url,
            config,
            data_dir,
        } => {
            let project_root = resolve_data_dir(data_dir.as_ref());
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &[],
                &project_root,
                config.as_deref(),
            );
            if let Err(e) = commands::sync::run(&config) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::AffectedTests {
            files,
            data_dir,
            format,
        } => {
            let project_root = resolve_data_dir(data_dir.as_ref());
            if let Err(e) = commands::affected_tests::run(&project_root, &files, &format) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::Doctor {
            token,
            api_url,
            config,
            data_dir,
        } => {
            let project_root = resolve_data_dir(data_dir.as_ref());
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &[],
                &project_root,
                config.as_deref(),
            );
            if let Err(e) = commands::doctor::run(&config) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::Init { api_url, global } => {
            if global {
                if let Err(e) = commands::init::run_global(api_url.as_deref()) {
                    eprintln!("[gaffer] Error: {:#}", e);
                    process::exit(1);
                }
            } else {
                let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
                if let Err(e) = commands::init::run(&project_root, api_url.as_deref()) {
                    eprintln!("[gaffer] Error: {:#}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Query {
            data_dir,
            pretty,
            command,
        } => {
            let project_root = resolve_data_dir(data_dir.as_ref());
            if let Err(e) = commands::query::run(&project_root, command, pretty) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::Config { config, command } => {
            let project_root = resolve_data_dir(None);
            let config = Config::resolve(None, None, &[], &project_root, config.as_deref());
            if let Err(e) = commands::config::run(&config, command) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
    }
}

/// Resolve the data directory (where `.gaffer/data.db` lives).
/// Uses `--data-dir` if provided, otherwise the current working directory.
fn resolve_data_dir(data_dir: Option<&PathBuf>) -> PathBuf {
    let path = match data_dir {
        Some(dir) => {
            if dir.is_absolute() {
                dir.clone()
            } else {
                match std::env::current_dir() {
                    Ok(cwd) => cwd.join(dir),
                    Err(e) => {
                        eprintln!("[gaffer] Warning: could not determine current directory: {}", e);
                        return dir.clone();
                    }
                }
            }
        }
        None => match std::env::current_dir() {
            Ok(cwd) => cwd,
            Err(e) => {
                eprintln!("[gaffer] Warning: could not determine current directory: {}", e);
                return PathBuf::from(".");
            }
        },
    };
    // Canonicalize to clean up trailing /. and symlinks
    path.canonicalize().unwrap_or(path)
}
