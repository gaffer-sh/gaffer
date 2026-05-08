//! `gaffer config list` — show resolved configuration with sources.

use anyhow::Result;
use colored::Colorize;

use crate::config::{mask_token, Config, ConfigSource};
use crate::ConfigCommand;

pub fn run(config: &Config, command: ConfigCommand) -> Result<()> {
    match command {
        ConfigCommand::List => list(config),
    }
}

fn list(config: &Config) -> Result<()> {
    println!();
    println!("  {}", "Resolved Configuration".bold());
    println!();

    // Token
    let token_display = match &config.token {
        Some(t) => mask_token(t),
        None => "(not set)".to_string(),
    };
    println!(
        "  {}  {}  ({})",
        "token:".bold(),
        token_display,
        source_label(&config.sources.token),
    );

    // API URL
    let api_url_display = config
        .api_url
        .as_deref()
        .unwrap_or("https://app.gaffer.sh (default)");
    println!(
        "  {}  {}  ({})",
        "api_url:".bold(),
        api_url_display,
        source_label(&config.sources.api_url),
    );

    // Report patterns
    println!(
        "  {}  {} patterns  ({})",
        "patterns:".bold(),
        config.report_patterns.len(),
        source_label(&config.sources.report_patterns),
    );
    for pattern in &config.report_patterns {
        println!("    {}", pattern);
    }

    // Project root
    println!();
    println!(
        "  {}  {}",
        "project_root:".bold(),
        config.project_root.display(),
    );

    // Config files checked
    println!();
    println!("  {}", "Config files:".bold());

    if let Some(ref path) = config.sources.local_config_path {
        println!("    {} {}  (local)", "✓".green(), path.display());
    }

    // Show checked-but-not-found paths (limit to avoid noise from deep walk-ups)
    let not_found: Vec<_> = config.sources.checked_paths.iter().filter(|(_, found)| !*found).collect();
    for (path, _) in not_found.iter().take(4) {
        println!("    {} {}  (not found)", "✗".dimmed(), path.display());
    }
    if not_found.len() > 4 {
        println!("    {} ... and {} more checked", "✗".dimmed(), not_found.len() - 4);
    }

    match &config.sources.global_config_path {
        Some(path) => println!("    {} {}  (global)", "✓".green(), path.display()),
        None => {
            if let Some(path) = crate::config::global_config_path() {
                println!("    {} {}  (global, not found)", "✗".dimmed(), path.display());
            } else {
                println!("    {} global config  (cannot determine home directory)", "✗".dimmed());
            }
        }
    }

    println!();

    Ok(())
}

fn source_label(source: &ConfigSource) -> String {
    match source {
        ConfigSource::CliFlag => "from --flag".to_string(),
        ConfigSource::EnvVar => "from env var".to_string(),
        ConfigSource::ExplicitConfig => "from --config".to_string(),
        ConfigSource::LocalConfig => "from local config".to_string(),
        ConfigSource::GlobalConfig => "from global config".to_string(),
        ConfigSource::Default => "default".to_string(),
    }
}
