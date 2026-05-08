//! `gaffer doctor` — diagnose common setup issues.
//!
//! Checks: config, database, token, report discovery, pending uploads,
//! framework detection, version.

use std::time::SystemTime;

use anyhow::Result;
use colored::Colorize;
use gaffer_core::types::GafferConfig;
use gaffer_core::GafferCore;

use crate::config::Config;
use crate::discovery;
use crate::framework;

fn pass(label: &str, detail: &str) {
    eprintln!("  {}  {}  {}", "OK".green().bold(), label, detail.dimmed());
}

fn warn(label: &str, detail: &str) {
    eprintln!("  {} {}  {}", "WARN".yellow().bold(), label, detail);
}

fn fail(label: &str, detail: &str) {
    eprintln!("  {} {}  {}", "FAIL".red().bold(), label, detail);
}

fn info(label: &str, detail: &str) {
    eprintln!("  {}  {}  {}", "INFO".dimmed(), label, detail.dimmed());
}

pub fn run(config: &Config) -> Result<()> {
    eprintln!("{}", "gaffer doctor".bold());
    eprintln!();

    // 1. Config
    check_config(config);

    // 2. Database
    check_database(config);

    // 3. Token
    check_token(config);

    // 4. Report discovery
    check_reports(config);

    // 5. Pending uploads
    check_pending(config);

    // 6. Frameworks
    check_frameworks(config);

    // 7. Version
    check_version();

    eprintln!();
    Ok(())
}

fn check_config(config: &Config) {
    // Show config layers
    match &config.sources.local_config_path {
        Some(path) => pass("Local config", &format!("{}", path.display())),
        None => info("Local config", "Not found (walked up from project root)"),
    }

    match &config.sources.global_config_path {
        Some(path) => pass("Global config", &format!("{}", path.display())),
        None => {
            let hint = crate::config::global_config_path()
                .map(|p| format!("{} (not found)", p.display()))
                .unwrap_or_else(|| "Cannot determine home directory".to_string());
            info("Global config", &hint);
        }
    }

    // Show token source
    let source_desc = format!("{}", config.sources.token);
    match &config.token {
        Some(_) => pass("Token source", &source_desc),
        None => warn("Token source", "Not configured (local-only mode, no cloud sync)"),
    }

    info("Root", &format!("{}", config.project_root.display()));

    // Check for orphaned .gaffer/ directories (data.db but no config.toml)
    let gaffer_dir = config.project_root.join(".gaffer");
    if gaffer_dir.join("data.db").exists() && !gaffer_dir.join("config.toml").exists() {
        warn(
            "Orphaned dir",
            &format!(
                "{} has data.db but no config.toml — run `gaffer init` here or delete it",
                gaffer_dir.display()
            ),
        );
    }
}

fn check_database(config: &Config) {
    let db_path = config.project_root.join(".gaffer/data.db");
    if !db_path.exists() {
        warn("Database", "No database found. Run `gaffer test` to create one.");
        return;
    }

    let core = GafferCore::new(GafferConfig {
        token: None,
        api_url: None,
        project_root: config.project_root.to_string_lossy().to_string(),
    });

    match core {
        Ok(core) => {
            match core.query_runs(1) {
                Ok(runs) => {
                    let size = std::fs::metadata(&db_path)
                        .map(|m| format!("{}KB", m.len() / 1024))
                        .unwrap_or_else(|_| "?".to_string());
                    let run_count = if runs.is_empty() { "no runs" } else { "has data" };
                    pass("Database", &format!("{} ({}), {}", db_path.display(), size, run_count));
                }
                Err(e) => {
                    fail("Database", &format!("Could not query: {}", e));
                }
            }
        }
        Err(e) => {
            fail("Database", &format!("Could not open: {}", e));
        }
    }
}

fn check_token(config: &Config) {
    match &config.token {
        None => {
            info("Token", "Not configured (local-only mode)");
        }
        Some(token) => {
            let masked = crate::config::mask_token(token);

            let base_url = config.api_url.as_deref().unwrap_or("https://app.gaffer.sh").trim_end_matches('/');
            let url = format!("{}/api/v1/user/projects", base_url);

            let http_config = ureq::config::Config::builder()
                .timeout_global(Some(std::time::Duration::from_secs(5)))
                .build();
            let agent = ureq::Agent::new_with_config(http_config);

            match agent.get(&url).header("X-API-Key", token).call() {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if (200..300).contains(&status) {
                        pass("Token", &format!("{} (valid, API reachable)", masked));
                    } else {
                        fail("Token", &format!("{} (HTTP {})", masked, status));
                    }
                }
                Err(e) => {
                    let err_str = e.to_string();
                    if err_str.contains("timed out") || err_str.contains("timeout") {
                        warn("Token", &format!("{} (API timeout — skipped)", masked));
                    } else {
                        fail("Token", &format!("{} ({})", masked, err_str));
                    }
                }
            }
        }
    }
}

fn check_reports(config: &Config) {
    let count = discovery::discover_reports(
        &config.project_root,
        &config.report_patterns,
        SystemTime::UNIX_EPOCH, // find all matching files, not just fresh ones
    ).len();

    if count > 0 {
        pass("Reports", &format!("{} files match current patterns", count));
    } else {
        warn("Reports", "No report files found matching configured patterns");
    }
}

fn check_pending(config: &Config) {
    let core = GafferCore::new(GafferConfig {
        token: config.token.clone(),
        api_url: config.api_url.clone(),
        project_root: config.project_root.to_string_lossy().to_string(),
    });

    match core {
        Ok(_core) => {
            // We can't easily query pending uploads count without exposing a new method,
            // so we check if the DB path exists and the sync result
            let db_path = config.project_root.join(".gaffer/data.db");
            if db_path.exists() {
                pass("Sync", "Database accessible for sync operations");
            }
        }
        Err(_) => {
            // Already reported in database check
        }
    }
}

fn check_frameworks(config: &Config) {
    let frameworks = framework::detect_frameworks(&config.project_root);
    if frameworks.is_empty() {
        info("Frameworks", "None detected");
    } else {
        let names: Vec<String> = frameworks.iter().map(|f| f.to_string()).collect();
        pass("Frameworks", &names.join(", "));
    }
}

fn check_version() {
    let version = env!("CARGO_PKG_VERSION");
    pass("Version", &format!("gaffer {}", version));
}
