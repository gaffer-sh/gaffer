//! Configuration resolution: CLI flags > env vars > config file > defaults.
//!
//! Config file discovery walks up from the working directory (like git):
//!   1. Check `<dir>/.gaffer/config.toml`
//!   2. Check `<dir>/gaffer.toml`
//!   3. Move to parent directory and repeat
//!   4. Stop at filesystem root
//!
//! The directory containing the config becomes the project root (where `.gaffer/data.db` lives).

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug)]
pub struct Config {
    pub token: Option<String>,
    pub api_url: Option<String>,
    pub project_root: PathBuf,
    pub report_patterns: Vec<String>,
}

#[derive(Deserialize, Default)]
struct TomlConfig {
    project: Option<TomlProject>,
    test: Option<TomlTest>,
}

#[derive(Deserialize, Default)]
struct TomlProject {
    token: Option<String>,
    api_url: Option<String>,
}

#[derive(Deserialize, Default)]
struct TomlTest {
    report_patterns: Option<Vec<String>>,
}

/// Default glob patterns for auto-discovering report files.
pub const DEFAULT_REPORT_PATTERNS: &[&str] = &[
    "**/.gaffer/reports/**/*.xml",
    "**/.gaffer/reports/**/*.json",
    "**/junit*.xml",
    "**/test-results/**/*.xml",
    "**/test-reports/**/*.xml",
    "**/ctrf/**/*.json",
    "**/ctrf-report.json",
    "**/coverage/lcov.info",
    "**/lcov.info",
];

impl Config {
    /// Resolve configuration from all sources.
    ///
    /// Walks up from `start_dir` to find `.gaffer/config.toml` or `gaffer.toml`.
    /// The directory containing the config becomes `project_root`.
    /// If no config is found, `start_dir` is used as the project root.
    pub fn resolve(
        cli_token: Option<&str>,
        cli_api_url: Option<&str>,
        cli_reports: &[String],
        start_dir: &Path,
    ) -> Self {
        // Walk up to find config file
        let (toml_config, project_root) = find_config(start_dir);

        // Token: CLI > env > toml
        let token = cli_token
            .map(|s| s.to_string())
            .or_else(|| std::env::var("GAFFER_TOKEN").ok())
            .or_else(|| toml_config.project.as_ref().and_then(|p| p.token.clone()));

        // API URL: CLI > env > toml
        let api_url = cli_api_url
            .map(|s| s.to_string())
            .or_else(|| std::env::var("GAFFER_API_URL").ok())
            .or_else(|| toml_config.project.as_ref().and_then(|p| p.api_url.clone()));

        // Report patterns: CLI > toml > defaults
        let report_patterns = if !cli_reports.is_empty() {
            cli_reports.to_vec()
        } else if let Some(patterns) = toml_config.test.as_ref().and_then(|t| t.report_patterns.clone()) {
            patterns
        } else {
            DEFAULT_REPORT_PATTERNS.iter().map(|s| s.to_string()).collect()
        };

        Config {
            token,
            api_url,
            project_root,
            report_patterns,
        }
    }
}

/// Walk up from `start_dir` looking for `.gaffer/config.toml` or `gaffer.toml`.
/// Returns the parsed config and the directory it was found in (project root).
/// If nothing found, returns default config and `start_dir` as the root.
fn find_config(start_dir: &Path) -> (TomlConfig, PathBuf) {
    let mut dir = start_dir.to_path_buf();

    loop {
        // Try .gaffer/config.toml then gaffer.toml in this directory
        for filename in &[".gaffer/config.toml", "gaffer.toml"] {
            let toml_path = dir.join(filename);
            match std::fs::read_to_string(&toml_path) {
                Ok(content) => {
                    return match toml::from_str(&content) {
                        Ok(config) => (config, dir),
                        Err(e) => {
                            eprintln!(
                                "[gaffer] Warning: failed to parse {}: {}",
                                toml_path.display(),
                                e
                            );
                            (TomlConfig::default(), dir)
                        }
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    eprintln!(
                        "[gaffer] Warning: could not read {}: {}",
                        toml_path.display(),
                        e
                    );
                    continue;
                }
            }
        }

        // Move to parent directory
        if !dir.pop() {
            break;
        }
    }

    (TomlConfig::default(), start_dir.to_path_buf())
}

/// Escape a string for use as a TOML quoted value.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// Write a config file to `.gaffer/config.toml`.
/// Sets restrictive file permissions (0o600) on Unix since the file may contain a token.
pub fn write_config(
    project_root: &Path,
    token: Option<&str>,
    api_url: Option<&str>,
    report_patterns: &[String],
) -> std::io::Result<()> {
    let gaffer_dir = project_root.join(".gaffer");
    std::fs::create_dir_all(&gaffer_dir)?;

    let mut content = String::new();
    content.push_str("[project]\n");
    if let Some(token) = token {
        content.push_str(&format!("token = \"{}\"\n", toml_escape(token)));
    }
    if let Some(url) = api_url {
        content.push_str(&format!("api_url = \"{}\"\n", toml_escape(url)));
    }
    content.push('\n');
    content.push_str("[test]\n");
    content.push_str("report_patterns = [\n");
    for pattern in report_patterns {
        content.push_str(&format!("    \"{}\",\n", toml_escape(pattern)));
    }
    content.push_str("]\n");

    let config_path = gaffer_dir.join("config.toml");
    std::fs::write(&config_path, content)?;

    // Set restrictive permissions since this file may contain a token
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        let _ = std::fs::set_permissions(&config_path, perms);
    }

    Ok(())
}
