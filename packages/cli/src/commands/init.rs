//! `gaffer init` — interactive setup: framework detection, reporter guidance, auth, config generation.
//! `gaffer init --global` — set up global authentication (writes to ~/.config/gaffer/).

use std::path::Path;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use colored::Colorize;
use dialoguer::Confirm;

use crate::config;
use crate::framework::{self, Framework, PatchResult};

const DEFAULT_API_URL: &str = "https://app.gaffer.sh";
const SESSION_POLL_INTERVAL: Duration = Duration::from_secs(2);
const SESSION_TIMEOUT: Duration = Duration::from_secs(300); // 5 minutes

/// Monorepo marker files to check in parent directories.
const MONOREPO_MARKERS: &[&str] = &[
    "pnpm-workspace.yaml",
    "turbo.json",
    "nx.json",
    "lerna.json",
];

pub fn run(project_root: &Path, api_url: Option<&str>) -> Result<()> {
    let api_base = api_url.unwrap_or(DEFAULT_API_URL);

    println!();

    // 1. Framework detection
    let frameworks = framework::detect_frameworks(project_root);
    if frameworks.is_empty() {
        println!(
            "  {} No test framework detected. Using default report patterns.",
            "Note:".bold()
        );
    } else {
        for fw in &frameworks {
            println!("  {} {}", "Detected:".bold(), fw);
        }
    }

    // 2. Reporter setup guidance (for each detected framework)
    for fw in &frameworks {
        show_reporter_status(fw);
    }

    println!();

    // 2b. Monorepo detection — show guidance if in a monorepo
    if is_in_monorepo(project_root) {
        println!(
            "  {} Monorepo detected. This config applies to tests run from",
            "Note:".bold()
        );
        println!("  this directory and below. For other packages, run {} in each,",
            "gaffer init".bold()
        );
        println!("  or use {} for a default token.", "gaffer init --global".bold());
        println!();
    }

    // 3. Cloud auth (optional)
    let token = if prompt_cloud_connect()? {
        match authenticate(api_base) {
            Ok(t) => Some(t),
            Err(e) => {
                eprintln!(
                    "  {} Authentication failed: {}",
                    "Error:".red().bold(),
                    e
                );
                let continue_local = Confirm::new()
                    .with_prompt("  Continue in local-only mode? (no cloud sync)")
                    .default(false)
                    .interact()
                    .context("Failed to read input")?;
                if continue_local {
                    None
                } else {
                    anyhow::bail!("Authentication failed. Please check your network and try again.");
                }
            }
        }
    } else {
        None
    };

    // 4. Update .gitignore (before writing config, since config may contain a token)
    ensure_gitignore(project_root);

    // 5. Write .gaffer/config.toml
    let persist_api_url = if api_base != DEFAULT_API_URL { Some(api_base) } else { None };
    let patterns: Vec<String> = crate::config::DEFAULT_REPORT_PATTERNS.iter().map(|s| s.to_string()).collect();
    let config_path = project_root.join(".gaffer/config.toml");
    config::write_config(project_root, token.as_deref(), persist_api_url, &patterns)
        .context("Failed to write .gaffer/config.toml")?;
    println!("  {} {}", "Created:".green().bold(), config_path.display());

    // 6. Done
    println!();
    if token.is_some() {
        println!(
            "  {} Run: {} to capture and sync test results.",
            "Done!".green().bold(),
            "gaffer test -- <your test command>".bold()
        );
    } else {
        println!(
            "  {} Run: {} to capture test results locally.",
            "Done!".green().bold(),
            "gaffer test -- <your test command>".bold()
        );
        println!(
            "  To enable cloud sync later, run {} again.",
            "gaffer init".bold()
        );
    }
    println!();

    Ok(())
}

/// `gaffer init --global` — authenticate and write to the global config directory.
/// Skips framework detection, reporter guidance, report patterns, and .gitignore.
pub fn run_global(api_url: Option<&str>) -> Result<()> {
    let api_base = api_url.unwrap_or(DEFAULT_API_URL);

    println!();
    println!("  {} Setting up global Gaffer authentication", "Global:".bold());
    println!("  This token will be used as a fallback when no local .gaffer/config.toml exists.");
    println!();

    // Auth flow
    let token = match authenticate(api_base) {
        Ok(t) => t,
        Err(e) => {
            anyhow::bail!("Authentication failed: {}. Please check your network and try again.", e);
        }
    };

    // Write global config (auth only — no report patterns)
    let persist_api_url = if api_base != DEFAULT_API_URL { Some(api_base) } else { None };
    config::write_global_config(Some(&token), persist_api_url)
        .context("Failed to write global config")?;

    let global_path = config::global_config_path()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| "~/.config/gaffer/config.toml".to_string());
    println!("  {} {}", "Created:".green().bold(), global_path);

    println!();
    println!(
        "  {} {} now works from any directory.",
        "Done!".green().bold(),
        "gaffer test".bold()
    );
    println!(
        "  Local .gaffer/config.toml files will take priority over this global config."
    );
    println!();

    Ok(())
}

/// Check if the project is inside a monorepo by looking for workspace marker files
/// in the current directory or parent directories.
fn is_in_monorepo(project_root: &Path) -> bool {
    let mut dir = project_root.to_path_buf();

    loop {
        // Check simple marker files
        for marker in MONOREPO_MARKERS {
            if dir.join(marker).exists() {
                return true;
            }
        }

        // Check package.json for "workspaces" field
        let pkg_json = dir.join("package.json");
        if pkg_json.exists() {
            if let Ok(content) = std::fs::read_to_string(&pkg_json) {
                if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                    if json.get("workspaces").is_some() {
                        return true;
                    }
                }
            }
        }

        // Check Cargo.toml for [workspace] section
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            if let Ok(content) = std::fs::read_to_string(&cargo_toml) {
                if content.contains("[workspace]") {
                    return true;
                }
            }
        }

        // Check go.work
        if dir.join("go.work").exists() {
            return true;
        }

        if !dir.pop() {
            break;
        }
    }

    false
}

fn show_reporter_status(framework: &Framework) {
    let result = framework::check_reporter(framework);
    match result {
        PatchResult::AlreadyConfigured { file, format } => {
            println!(
                "  {} {} — {} reporter already configured",
                "Ready:".green().bold(),
                file.file_name().unwrap_or_default().to_string_lossy(),
                format
            );
        }
        PatchResult::Instructions(instructions) => {
            println!();
            println!("{}", instructions);
        }
    }
}

fn prompt_cloud_connect() -> Result<bool> {
    let connect = Confirm::new()
        .with_prompt("  Connect to Gaffer Cloud? (enables team sync, dashboard, notifications)")
        .default(true)
        .interact()
        .context("Failed to read input")?;
    Ok(connect)
}

fn authenticate(api_base: &str) -> Result<String> {
    // 1. Create session
    let setup_url = format!("{}/api/v1/cli/setup", api_base);
    let response = ureq::post(&setup_url)
        .send_empty()
        .context("Failed to create CLI session")?;
    let session: serde_json::Value =
        serde_json::from_reader(response.into_body().as_reader()).context("Invalid session response")?;

    let code = session["code"]
        .as_str()
        .context("Missing session code")?
        .to_string();
    let browser_url = session["url"]
        .as_str()
        .context("Missing session URL")?
        .to_string();

    // 2. Show verify code (last 6 chars — must match apps/dashboard/app/pages/cli/setup.vue)
    let verify_code = if code.len() >= 6 {
        &code[code.len() - 6..]
    } else {
        &code
    };
    println!();
    println!("  {} {}", "Verify code:".bold(), verify_code);
    println!("  Opening browser to authenticate...");

    // 3. Open browser
    if let Err(e) = open::that(&browser_url) {
        eprintln!(
            "  {} Could not open browser: {}\n  Open this URL manually: {}",
            "Warning:".yellow().bold(),
            e,
            browser_url
        );
    }

    // 4. Poll for token
    let poll_url = format!("{}/api/v1/cli/token?code={}", api_base, code);
    let start = std::time::Instant::now();
    let mut consecutive_errors: u32 = 0;
    const MAX_CONSECUTIVE_ERRORS: u32 = 5;

    println!("  Waiting for authentication...");

    loop {
        if start.elapsed() > SESSION_TIMEOUT {
            anyhow::bail!(
                "Authentication timed out. You can copy your token from {}/settings and set GAFFER_TOKEN.",
                api_base
            );
        }

        thread::sleep(SESSION_POLL_INTERVAL);

        let body: serde_json::Value = match ureq::get(&poll_url).call() {
            Ok(resp) => {
                consecutive_errors = 0;
                match serde_json::from_reader(resp.into_body().as_reader()) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!(
                            "  {} Unexpected response from server: {}",
                            "Warning:".yellow().bold(),
                            e
                        );
                        continue;
                    }
                }
            }
            Err(ureq::Error::StatusCode(status)) => {
                if status == 404 {
                    anyhow::bail!(
                        "Session expired or not found. Please re-run `gaffer init`."
                    );
                }
                if status == 429 {
                    thread::sleep(Duration::from_secs(5));
                    continue;
                }
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    anyhow::bail!(
                        "Server returned HTTP {} after {} consecutive failures. Check your network and try again.",
                        status, consecutive_errors
                    );
                }
                continue;
            }
            Err(e) => {
                consecutive_errors += 1;
                if consecutive_errors >= MAX_CONSECUTIVE_ERRORS {
                    anyhow::bail!(
                        "Cannot reach Gaffer server after {} attempts: {}. Check your network and try again.",
                        consecutive_errors, e
                    );
                }
                continue;
            }
        };

        if body["status"].as_str() == Some("ready") {
            if let Some(token) = body["token"].as_str() {
                println!("  {} Authenticated!", "Done:".green().bold());
                return Ok(token.to_string());
            }
        }
        // status == "pending" → keep polling
    }
}

/// Append `.gaffer/` to `.gitignore` if it exists and doesn't already contain it.
fn ensure_gitignore(project_root: &Path) {
    let gitignore_path = project_root.join(".gitignore");
    if !gitignore_path.exists() {
        return;
    }

    let content = match std::fs::read_to_string(&gitignore_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "  {} Could not read .gitignore: {}\n  Please manually add '.gaffer/' to your .gitignore.",
                "Warning:".yellow().bold(),
                e
            );
            return;
        }
    };

    // Check if .gaffer/ is already ignored
    if content.lines().any(|line| {
        let trimmed = line.trim();
        trimmed == ".gaffer/" || trimmed == ".gaffer" || trimmed == "/.gaffer/" || trimmed == "/.gaffer"
    }) {
        return;
    }

    // Append .gaffer/ to .gitignore
    let mut new_content = content;
    if !new_content.ends_with('\n') {
        new_content.push('\n');
    }
    new_content.push_str(".gaffer/\n");

    match std::fs::write(&gitignore_path, new_content) {
        Ok(()) => println!("  {} .gaffer/ to .gitignore", "Added:".green().bold()),
        Err(e) => eprintln!(
            "  {} Could not update .gitignore: {}",
            "Warning:".yellow().bold(),
            e
        ),
    }
}
