//! `gaffer query <subcommand>` — query local test intelligence on-demand.

use std::path::Path;

use anyhow::{Context, Result};
use gaffer_core::types::GafferConfig;
use gaffer_core::GafferCore;
use serde::Serialize;

use crate::output::summary;
use crate::QueryCommand;

fn print_json(value: &impl Serialize, label: &str) -> Result<()> {
    let json =
        serde_json::to_string(value).with_context(|| format!("Failed to serialize {} to JSON", label))?;
    println!("{}", json);
    Ok(())
}

/// Run a query subcommand. Opens the local DB read-only and delegates to the
/// appropriate GafferCore query method.
pub fn run(project_root: &Path, command: QueryCommand, pretty: bool) -> Result<()> {
    let db_path = project_root.join(".gaffer").join("data.db");
    if !db_path.exists() {
        anyhow::bail!(
            "No test data found. Run `gaffer test` first.\n\
             Expected database at: {}",
            db_path.display()
        );
    }

    let core = GafferCore::new(GafferConfig {
        token: None,
        api_url: None,
        project_root: project_root.to_string_lossy().to_string(),
    })
    .context("Failed to open database")?;

    match command {
        QueryCommand::Health => {
            let health = core.query_health().context("Failed to compute health score")?;
            if pretty {
                summary::print_health(&health, None);
            } else {
                print_json(&health, "health score")?;
            }
        }
        QueryCommand::Flaky => {
            let flaky = core.query_flaky().context("Failed to detect flaky tests")?;
            if pretty {
                summary::print_flaky_list(&flaky);
            } else {
                print_json(&flaky, "flaky tests")?;
            }
        }
        QueryCommand::Slowest { limit } => {
            let analysis = core
                .query_slowest(limit)
                .context("Failed to analyze durations")?;
            if pretty {
                summary::print_slowest(&analysis);
            } else {
                print_json(&analysis, "duration analysis")?;
            }
        }
        QueryCommand::Runs { limit } => {
            let runs = core.query_runs(limit).context("Failed to list runs")?;
            if pretty {
                summary::print_runs(&runs);
            } else {
                print_json(&runs, "runs")?;
            }
        }
        QueryCommand::History { test, limit } => {
            let history = core
                .query_history(&test, limit)
                .context("Failed to query test history")?;
            if pretty {
                summary::print_history(&history);
            } else {
                print_json(&history, "test history")?;
            }
        }
        QueryCommand::Failures { pattern, limit } => {
            let failures = core
                .query_failures(&pattern, limit)
                .context("Failed to search failures")?;
            if pretty {
                summary::print_failures_search(&failures);
            } else {
                print_json(&failures, "failure results")?;
            }
        }
    }

    Ok(())
}
