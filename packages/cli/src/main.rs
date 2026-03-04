//! Gaffer CLI — wraps test commands and parses artifacts for storage and analysis.
//!
//! Usage:
//!   gaffer test -- pnpm test
//!   gaffer test --report results/junit.xml -- pnpm test
//!   gaffer sync

mod commands;
mod config;
mod discovery;
mod framework;
mod git;
mod output;

use std::path::PathBuf;
use std::process;

use clap::{Parser, Subcommand};

use config::Config;

#[derive(Clone, clap::ValueEnum)]
pub enum OutputFormat {
    Human,
    Json,
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
        /// Authentication token (overrides GAFFER_TOKEN and gaffer.toml)
        #[arg(long, env = "GAFFER_TOKEN")]
        token: Option<String>,

        /// API URL for cloud sync
        #[arg(long, env = "GAFFER_API_URL")]
        api_url: Option<String>,

        /// Report file path(s) to parse (can be specified multiple times)
        #[arg(long = "report", short = 'r')]
        reports: Vec<String>,

        /// Project root directory (default: current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Output format: human (colored stderr) or json (stdout)
        #[arg(long, value_enum, default_value_t = OutputFormat::Human)]
        format: OutputFormat,

        /// Show full error messages and context files for failed tests
        #[arg(long)]
        show_errors: bool,

        /// Compare against the latest run on a branch (e.g. --compare=main)
        #[arg(long)]
        compare: Option<String>,

        /// The test command to run (everything after --)
        #[arg(trailing_var_arg = true, required = true)]
        command: Vec<String>,
    },

    /// Force sync pending uploads to the Gaffer dashboard
    Sync {
        /// Authentication token
        #[arg(long, env = "GAFFER_TOKEN")]
        token: Option<String>,

        /// API URL for cloud sync
        #[arg(long, env = "GAFFER_API_URL")]
        api_url: Option<String>,

        /// Project root directory (default: current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,
    },

    /// Interactive setup — detect framework, configure reporters, authenticate
    Init {
        /// API URL for cloud sync
        #[arg(long, env = "GAFFER_API_URL")]
        api_url: Option<String>,
    },

    /// Query local test intelligence (health, flaky tests, durations, history)
    Query {
        /// Project root directory (default: current directory)
        #[arg(long, default_value = ".")]
        root: PathBuf,

        /// Human-readable output (default: JSON)
        #[arg(long)]
        pretty: bool,

        #[command(subcommand)]
        command: QueryCommand,
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

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Test {
            token,
            api_url,
            reports,
            root,
            format,
            show_errors,
            compare,
            command,
        } => {
            let project_root = resolve_root(&root);
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &reports,
                &project_root,
            );
            match commands::test::run(&config, &command, &reports, &format, show_errors, compare.as_deref()) {
                Ok(exit_code) => process::exit(exit_code),
                Err(e) => {
                    eprintln!("[gaffer] Error: {:#}", e);
                    process::exit(1);
                }
            }
        }
        Commands::Sync {
            token,
            api_url,
            root,
        } => {
            let project_root = resolve_root(&root);
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &[],
                &project_root,
            );
            if let Err(e) = commands::sync::run(&config) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::Init { api_url } => {
            let project_root = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
            if let Err(e) = commands::init::run(&project_root, api_url.as_deref()) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
        Commands::Query {
            root,
            pretty,
            command,
        } => {
            let project_root = resolve_root(&root);
            if let Err(e) = commands::query::run(&project_root, command, pretty) {
                eprintln!("[gaffer] Error: {:#}", e);
                process::exit(1);
            }
        }
    }
}

fn resolve_root(root: &PathBuf) -> PathBuf {
    if root.is_absolute() {
        root.clone()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(root),
            Err(e) => {
                eprintln!("[gaffer] Warning: could not determine current directory: {}", e);
                root.clone()
            }
        }
    }
}
