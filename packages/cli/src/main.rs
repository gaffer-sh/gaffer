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
}

fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Test {
            token,
            api_url,
            reports,
            root,
            command,
        } => {
            let project_root = resolve_root(&root);
            let config = Config::resolve(
                token.as_deref(),
                api_url.as_deref(),
                &reports,
                &project_root,
            );
            match commands::test::run(&config, &command, &reports) {
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
