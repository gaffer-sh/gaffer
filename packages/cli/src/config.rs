//! Configuration resolution with per-field merging across multiple sources.
//!
//! Resolution order (per field): CLI flags > env vars > local config > global config > defaults.
//!
//! ```text
//! LOCAL CONFIG DISCOVERY (walk up from CWD, like git):
//!   1. Check `<dir>/.gaffer/config.toml`
//!   2. Check `<dir>/gaffer.toml`
//!   3. Move to parent directory and repeat
//!   4. Stop at filesystem root
//!
//! GLOBAL CONFIG (auth fallback):
//!   $XDG_CONFIG_HOME/gaffer/config.toml   (if XDG_CONFIG_HOME set)
//!   ~/.config/gaffer/config.toml           (fallback)
//!
//! PER-FIELD MERGE:
//!   token:    CLI > env > local > global > None
//!   api_url:  CLI > env > local > global > None
//!   patterns: CLI > local > defaults (global never provides patterns)
//! ```
//!
//! The directory containing the local config becomes the project root (where
//! `.gaffer/data.db` lives). If no local config is found, CWD is the project root.

use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::oidc;

/// Where a config value came from, in priority order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigSource {
    CliFlag,
    EnvVar,
    ExplicitConfig,
    LocalConfig,
    GlobalConfig,
    /// Token only: no explicit token was configured anywhere, so it was
    /// obtained by exchanging the GitHub Actions runner's OIDC identity
    /// token (see `oidc::try_exchange`).
    Oidc,
    Default,
}

impl std::fmt::Display for ConfigSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigSource::CliFlag => write!(f, "CLI flag"),
            ConfigSource::EnvVar => write!(f, "env var"),
            ConfigSource::ExplicitConfig => write!(f, "explicit config"),
            ConfigSource::LocalConfig => write!(f, "local config"),
            ConfigSource::GlobalConfig => write!(f, "global config"),
            ConfigSource::Oidc => write!(f, "GitHub Actions OIDC"),
            ConfigSource::Default => write!(f, "default"),
        }
    }
}

/// Tracks which source provided each resolved config value.
#[derive(Debug)]
pub struct ConfigSources {
    pub token: ConfigSource,
    pub api_url: ConfigSource,
    pub report_patterns: ConfigSource,
    /// Path to the local config file, if one was found.
    pub local_config_path: Option<PathBuf>,
    /// Path to the global config file, if one exists.
    pub global_config_path: Option<PathBuf>,
    /// All directories checked during local walk-up (for `config list`).
    pub checked_paths: Vec<(PathBuf, bool)>,
}

#[derive(Debug)]
pub struct Config {
    pub token: Option<String>,
    pub api_url: Option<String>,
    pub project_root: PathBuf,
    pub report_patterns: Vec<String>,
    pub sources: ConfigSources,
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
    "**/target/nextest/**/*.xml",
    "**/ctrf/**/*.json",
    "**/ctrf-report.json",
    "**/coverage/lcov.info",
    "**/lcov.info",
];

/// Return the global config directory, respecting XDG Base Directory spec.
///
/// Checks `$XDG_CONFIG_HOME/gaffer/` first, then falls back to `$HOME/.config/gaffer/`.
/// Returns `None` if neither environment variable is set.
pub fn global_config_dir() -> Option<PathBuf> {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        if !xdg.is_empty() {
            return Some(PathBuf::from(xdg).join("gaffer"));
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() {
            return Some(PathBuf::from(home).join(".config").join("gaffer"));
        }
    }
    None
}

/// Return the full path to the global config file, if the directory can be determined.
pub fn global_config_path() -> Option<PathBuf> {
    global_config_dir().map(|d| d.join("config.toml"))
}

impl Config {
    /// Resolve configuration from all sources with per-field merging.
    ///
    /// Each field resolves independently through the priority chain:
    /// CLI flags > env vars > explicit config > local walk-up > global config > defaults.
    ///
    /// When `explicit_config` is provided, it takes priority over local walk-up.
    /// The `start_dir` always determines `project_root` (where `.gaffer/data.db` lives) —
    /// neither explicit config nor local walk-up changes it.
    pub fn resolve(
        cli_token: Option<&str>,
        cli_api_url: Option<&str>,
        cli_reports: &[String],
        start_dir: &Path,
        explicit_config: Option<&Path>,
    ) -> Self {
        // Load explicit config if provided
        let explicit = explicit_config.and_then(load_explicit_config);

        // Load local + global config layers
        let (local_result, checked_paths) = find_local_config(start_dir);
        let global = load_global_config();

        // project_root is always start_dir — explicit config and walk-up don't change it
        let project_root = if explicit_config.is_some() {
            // When using explicit config, project_root = start_dir (CWD or --data-dir)
            start_dir.to_path_buf()
        } else {
            // When using walk-up, project_root = directory where local config was found
            local_result
                .as_ref()
                .map(|(_, root)| root.clone())
                .unwrap_or_else(|| start_dir.to_path_buf())
        };

        let local_config = local_result.map(|(cfg, _)| cfg);

        // API URL: CLI > env > explicit > local > global. Resolved before the
        // token so a resolved custom endpoint can be used as the OIDC
        // exchange origin below.
        let (api_url, api_url_source) = resolve_field_5(
            cli_api_url.map(|s| s.to_string()),
            || std::env::var("GAFFER_API_URL").ok(),
            || explicit.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.api_url.clone())),
            || local_config.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.api_url.clone())),
            || global.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.api_url.clone())),
        );

        // Token: CLI > env > explicit > local > global > GitHub Actions OIDC.
        // `gaffer upload` reaches the same OIDC fallback through its own
        // `resolve_token` (commands/upload.rs); both bottom out in
        // `oidc::try_exchange`, so `gaffer test`/`sync`/`doctor` (all of
        // which resolve their token through here) get identical behavior.
        let (token, token_source) = resolve_field_5(
            cli_token.map(|s| s.to_string()),
            || std::env::var("GAFFER_TOKEN").ok(),
            || explicit.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.token.clone())),
            || local_config.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.token.clone())),
            || global.as_ref().and_then(|c| c.project.as_ref().and_then(|p| p.token.clone())),
        );
        let (token, token_source) = if token.is_none() {
            match oidc::try_exchange(api_url.as_deref()) {
                Some(Ok(exchanged)) => {
                    oidc::print_auth_success(&exchanged);
                    (Some(exchanged.access_token), ConfigSource::Oidc)
                }
                Some(Err(e)) => {
                    eprintln!(
                        "[gaffer] Warning: GitHub Actions OIDC token exchange failed: {}. \
                         Pass --token, set GAFFER_TOKEN, or check that this job was granted \
                         `permissions: id-token: write`.",
                        e
                    );
                    (token, token_source)
                }
                None => (token, token_source),
            }
        } else {
            (token, token_source)
        };

        // Report patterns: CLI > explicit > local > defaults (global never provides patterns)
        let (report_patterns, patterns_source) = if !cli_reports.is_empty() {
            (cli_reports.to_vec(), ConfigSource::CliFlag)
        } else if let Some(patterns) = explicit.as_ref().and_then(|c| c.test.as_ref().and_then(|t| t.report_patterns.clone())) {
            (patterns, ConfigSource::ExplicitConfig)
        } else if let Some(patterns) = local_config.as_ref().and_then(|c| c.test.as_ref().and_then(|t| t.report_patterns.clone())) {
            (patterns, ConfigSource::LocalConfig)
        } else {
            (DEFAULT_REPORT_PATTERNS.iter().map(|s| s.to_string()).collect(), ConfigSource::Default)
        };

        let local_config_path = if explicit_config.is_some() {
            explicit_config.map(|p| p.to_path_buf())
        } else {
            checked_paths.iter().find(|(_, found)| *found).map(|(p, _)| p.clone())
        };
        let global_path = global_config_path().filter(|p| p.exists());

        Config {
            token,
            api_url,
            project_root,
            report_patterns,
            sources: ConfigSources {
                token: token_source,
                api_url: api_url_source,
                report_patterns: patterns_source,
                local_config_path,
                global_config_path: global_path,
                checked_paths,
            },
        }
    }
}

/// Resolve a single field through the 5-layer priority chain:
/// CLI flag > env var > explicit config > local config > global config.
///
/// Returns the resolved value and which source provided it.
fn resolve_field_5(
    cli_value: Option<String>,
    env_fn: impl FnOnce() -> Option<String>,
    explicit_fn: impl FnOnce() -> Option<String>,
    local_fn: impl FnOnce() -> Option<String>,
    global_fn: impl FnOnce() -> Option<String>,
) -> (Option<String>, ConfigSource) {
    if let Some(v) = cli_value {
        return (Some(v), ConfigSource::CliFlag);
    }
    if let Some(v) = env_fn() {
        return (Some(v), ConfigSource::EnvVar);
    }
    if let Some(v) = explicit_fn() {
        return (Some(v), ConfigSource::ExplicitConfig);
    }
    if let Some(v) = local_fn() {
        return (Some(v), ConfigSource::LocalConfig);
    }
    if let Some(v) = global_fn() {
        return (Some(v), ConfigSource::GlobalConfig);
    }
    (None, ConfigSource::Default)
}

/// Load a config file from an explicit path (--config flag).
fn load_explicit_config(path: &Path) -> Option<TomlConfig> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!(
                "[gaffer] Error: could not read config file {}: {}",
                path.display(),
                e
            );
            return None;
        }
    };
    match toml::from_str(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            eprintln!(
                "[gaffer] Error: failed to parse config file {}: {}",
                path.display(),
                e
            );
            None
        }
    }
}

/// Walk up from `start_dir` looking for `.gaffer/config.toml` or `gaffer.toml`.
///
/// `(found config and its project root, every path probed and whether it existed)`.
/// The probe list feeds `gaffer config` / `gaffer doctor`, which show the search path.
type LocalConfigLookup = (Option<(TomlConfig, PathBuf)>, Vec<(PathBuf, bool)>);

/// Returns the parsed config + project root if found, plus all paths checked.
fn find_local_config(start_dir: &Path) -> LocalConfigLookup {
    let mut dir = start_dir.to_path_buf();
    let mut checked = Vec::new();

    loop {
        for filename in &[".gaffer/config.toml", "gaffer.toml"] {
            let toml_path = dir.join(filename);
            match std::fs::read_to_string(&toml_path) {
                Ok(content) => {
                    checked.push((toml_path.clone(), true));
                    return match toml::from_str(&content) {
                        Ok(config) => (Some((config, dir)), checked),
                        Err(e) => {
                            eprintln!(
                                "[gaffer] Warning: failed to parse {}: {}",
                                toml_path.display(),
                                e
                            );
                            (Some((TomlConfig::default(), dir)), checked)
                        }
                    };
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    checked.push((toml_path, false));
                }
                Err(e) => {
                    eprintln!(
                        "[gaffer] Warning: could not read {}: {}",
                        toml_path.display(),
                        e
                    );
                    checked.push((toml_path, false));
                }
            }
        }

        if !dir.pop() {
            break;
        }
    }

    (None, checked)
}

/// Load the global config file if it exists.
fn load_global_config() -> Option<TomlConfig> {
    let path = global_config_path()?;
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            eprintln!(
                "[gaffer] Warning: could not read global config {}: {}",
                path.display(),
                e
            );
            return None;
        }
    };
    match toml::from_str(&content) {
        Ok(config) => Some(config),
        Err(e) => {
            eprintln!(
                "[gaffer] Warning: failed to parse global config {}: {}",
                path.display(),
                e
            );
            None
        }
    }
}

/// Escape a string for use as a TOML basic string value.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('\t', "\\t")
}

/// Write a project-local config file to `.gaffer/config.toml`.
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

    set_restrictive_permissions(&config_path);

    Ok(())
}

/// Write a global config file (auth only, no report patterns).
/// Creates the global config directory if it doesn't exist.
/// Sets restrictive file permissions (0o600) since the file contains a token.
pub fn write_global_config(
    token: Option<&str>,
    api_url: Option<&str>,
) -> std::io::Result<()> {
    let dir = global_config_dir().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "Cannot determine home directory. Set $HOME or $XDG_CONFIG_HOME.",
        )
    })?;
    std::fs::create_dir_all(&dir)?;

    let mut content = String::new();
    content.push_str("[project]\n");
    if let Some(token) = token {
        content.push_str(&format!("token = \"{}\"\n", toml_escape(token)));
    }
    if let Some(url) = api_url {
        content.push_str(&format!("api_url = \"{}\"\n", toml_escape(url)));
    }

    let config_path = dir.join("config.toml");
    std::fs::write(&config_path, content)?;

    set_restrictive_permissions(&config_path);

    Ok(())
}

/// Set file permissions to 0o600 (owner read/write only) on Unix.
/// Warns on failure since config files may contain tokens.
fn set_restrictive_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = std::fs::Permissions::from_mode(0o600);
        if let Err(e) = std::fs::set_permissions(path, perms) {
            eprintln!(
                "[gaffer] Warning: could not set restrictive permissions on {}: {}",
                path.display(),
                e
            );
        }
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Mask a token for display, showing only a prefix and suffix.
/// Tokens shorter than 12 chars are fully masked to avoid leaking overlapping characters.
pub fn mask_token(token: &str) -> String {
    if token.len() < 12 {
        return "****".to_string();
    }
    format!("{}***{}", &token[..4], &token[token.len() - 4..])
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // Tests that modify env vars must be serialized to avoid race conditions.
    // Any test here that calls `Config::resolve` with no token configured
    // reaches `oidc::try_exchange`, which reads process-wide env vars the
    // oidc/upload test suites also mutate, so this reuses their mutex
    // rather than defining a separate one that wouldn't serialize against it.
    use crate::oidc::ENV_MUTEX;

    // --- global_config_dir tests ---

    #[test]
    fn global_config_dir_uses_xdg_when_set() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let xdg_path = tmp.path().to_str().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", xdg_path);
        let result = global_config_dir();
        std::env::remove_var("XDG_CONFIG_HOME");

        assert_eq!(result, Some(tmp.path().join("gaffer")));
    }

    #[test]
    fn global_config_dir_ignores_empty_xdg() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", "");
        let result = global_config_dir();
        // Restore
        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        // Should fall back to HOME-based path (if HOME is set)
        assert!(result.is_none() || result.unwrap().to_string_lossy().contains(".config/gaffer"));
    }

    // --- find_local_config tests ---

    #[test]
    fn find_local_config_finds_gaffer_dir_config() {
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[project]\ntoken = \"test_token\"\n",
        )
        .unwrap();

        let (result, checked) = find_local_config(tmp.path());
        assert!(result.is_some());
        let (config, root) = result.unwrap();
        assert_eq!(config.project.unwrap().token.unwrap(), "test_token");
        assert_eq!(root, tmp.path());
        assert!(checked.iter().any(|(_, found)| *found));
    }

    #[test]
    fn find_local_config_finds_gaffer_toml() {
        let tmp = TempDir::new().unwrap();
        fs::write(
            tmp.path().join("gaffer.toml"),
            "[project]\ntoken = \"flat_token\"\n",
        )
        .unwrap();

        let (result, _) = find_local_config(tmp.path());
        assert!(result.is_some());
        let (config, _) = result.unwrap();
        assert_eq!(config.project.unwrap().token.unwrap(), "flat_token");
    }

    #[test]
    fn find_local_config_walks_up() {
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[project]\ntoken = \"parent_token\"\n",
        )
        .unwrap();

        // Create a child directory and search from there
        let child = tmp.path().join("subdir");
        fs::create_dir_all(&child).unwrap();

        let (result, _) = find_local_config(&child);
        assert!(result.is_some());
        let (config, root) = result.unwrap();
        assert_eq!(config.project.unwrap().token.unwrap(), "parent_token");
        assert_eq!(root, tmp.path());
    }

    #[test]
    fn find_local_config_returns_none_when_nothing_found() {
        let tmp = TempDir::new().unwrap();
        let (result, checked) = find_local_config(tmp.path());
        assert!(result.is_none());
        // Should have checked at least the tmp dir
        assert!(!checked.is_empty());
        assert!(checked.iter().all(|(_, found)| !found));
    }

    #[test]
    fn find_local_config_prefers_gaffer_dir_over_flat_file() {
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[project]\ntoken = \"dir_token\"\n",
        )
        .unwrap();
        fs::write(
            tmp.path().join("gaffer.toml"),
            "[project]\ntoken = \"flat_token\"\n",
        )
        .unwrap();

        let (result, _) = find_local_config(tmp.path());
        let (config, _) = result.unwrap();
        assert_eq!(config.project.unwrap().token.unwrap(), "dir_token");
    }

    // --- Config::resolve per-field merging tests ---

    #[test]
    fn resolve_cli_flag_wins_over_all() {
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[project]\ntoken = \"local_token\"\n",
        )
        .unwrap();

        let config = Config::resolve(
            Some("cli_token"),
            None,
            &[],
            tmp.path(),
            None,
        );
        assert_eq!(config.token.as_deref(), Some("cli_token"));
        assert_eq!(config.sources.token, ConfigSource::CliFlag);
    }

    #[test]
    fn resolve_local_config_provides_patterns_global_provides_token() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        // Local config with patterns but no token
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[test]\nreport_patterns = [\"custom/**/*.xml\"]\n",
        )
        .unwrap();

        // Global config with token
        let global_dir = TempDir::new().unwrap();
        let global_gaffer = global_dir.path().join("gaffer");
        fs::create_dir_all(&global_gaffer).unwrap();
        fs::write(
            global_gaffer.join("config.toml"),
            "[project]\ntoken = \"global_token\"\n",
        )
        .unwrap();

        // Point XDG to our temp global dir
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", global_dir.path());

        let config = Config::resolve(None, None, &[], tmp.path(), None);

        // Restore env
        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        // Token from global, patterns from local
        assert_eq!(config.token.as_deref(), Some("global_token"));
        assert_eq!(config.sources.token, ConfigSource::GlobalConfig);
        assert_eq!(config.report_patterns, vec!["custom/**/*.xml"]);
        assert_eq!(config.sources.report_patterns, ConfigSource::LocalConfig);
    }

    #[test]
    fn resolve_local_token_wins_over_global() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(
            gaffer_dir.join("config.toml"),
            "[project]\ntoken = \"local_token\"\n",
        )
        .unwrap();

        // Global config with different token
        let global_dir = TempDir::new().unwrap();
        let global_gaffer = global_dir.path().join("gaffer");
        fs::create_dir_all(&global_gaffer).unwrap();
        fs::write(
            global_gaffer.join("config.toml"),
            "[project]\ntoken = \"global_token\"\n",
        )
        .unwrap();

        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", global_dir.path());

        let config = Config::resolve(None, None, &[], tmp.path(), None);

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert_eq!(config.token.as_deref(), Some("local_token"));
        assert_eq!(config.sources.token, ConfigSource::LocalConfig);
    }

    #[test]
    fn resolve_defaults_when_no_config() {
        let _lock = ENV_MUTEX.lock().unwrap();
        // Clear the OIDC env vars too. A real GitHub Actions runner sets
        // these ambiently, and if present they'd send this test down the
        // OIDC exchange path (a real network call) instead of the "no
        // token" default this test is asserting.
        let original_request_url = std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL").ok();
        let original_request_token = std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN").ok();
        std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL");
        std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN");

        let tmp = TempDir::new().unwrap();
        // Ensure no global config interferes
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let empty_dir = TempDir::new().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", empty_dir.path());

        let config = Config::resolve(None, None, &[], tmp.path(), None);

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }
        match original_request_url {
            Some(v) => std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_URL", v),
            None => std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_URL"),
        }
        match original_request_token {
            Some(v) => std::env::set_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN", v),
            None => std::env::remove_var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
        }

        assert!(config.token.is_none());
        assert_eq!(config.sources.token, ConfigSource::Default);
        assert_eq!(config.report_patterns.len(), DEFAULT_REPORT_PATTERNS.len());
        assert_eq!(config.sources.report_patterns, ConfigSource::Default);
    }

    #[test]
    fn resolve_project_root_from_local_config() {
        // No token anywhere in this config, so resolution reaches the OIDC
        // fallback; serialize against oidc/upload's env var mutation.
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join(".gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        fs::write(gaffer_dir.join("config.toml"), "[project]\n").unwrap();

        let child = tmp.path().join("deep").join("nested");
        fs::create_dir_all(&child).unwrap();

        let config = Config::resolve(None, None, &[], &child, None);
        assert_eq!(config.project_root, tmp.path());
    }

    #[test]
    fn resolve_project_root_is_cwd_when_no_local() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        let empty_dir = TempDir::new().unwrap();
        std::env::set_var("XDG_CONFIG_HOME", empty_dir.path());

        let config = Config::resolve(None, None, &[], tmp.path(), None);

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert_eq!(config.project_root, tmp.path());
    }

    // --- write_global_config tests ---

    #[test]
    fn write_global_config_creates_dir_and_file() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        let result = write_global_config(Some("test_token"), Some("https://test.gaffer.sh"));

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        assert!(result.is_ok());
        let config_path = tmp.path().join("gaffer").join("config.toml");
        assert!(config_path.exists());

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("token = \"test_token\""));
        assert!(content.contains("api_url = \"https://test.gaffer.sh\""));
        // Should NOT contain [test] section
        assert!(!content.contains("[test]"));
        assert!(!content.contains("report_patterns"));
    }

    #[test]
    fn write_global_config_overwrites_existing() {
        let _lock = ENV_MUTEX.lock().unwrap();
        let tmp = TempDir::new().unwrap();
        let gaffer_dir = tmp.path().join("gaffer");
        fs::create_dir_all(&gaffer_dir).unwrap();
        let config_path = gaffer_dir.join("config.toml");

        // Write first config
        fs::write(&config_path, "[project]\ntoken = \"old_token\"\n").unwrap();
        assert!(fs::read_to_string(&config_path).unwrap().contains("old_token"));

        // Now use the global write function
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        write_global_config(Some("new_token"), None).unwrap();

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("new_token"));
        assert!(!content.contains("old_token"));
    }

    #[cfg(unix)]
    #[test]
    fn write_global_config_sets_restrictive_permissions() {
        let _lock = ENV_MUTEX.lock().unwrap();
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let original_xdg = std::env::var("XDG_CONFIG_HOME").ok();
        std::env::set_var("XDG_CONFIG_HOME", tmp.path());

        write_global_config(Some("secret"), None).unwrap();

        match original_xdg {
            Some(v) => std::env::set_var("XDG_CONFIG_HOME", v),
            None => std::env::remove_var("XDG_CONFIG_HOME"),
        }

        let metadata = fs::metadata(tmp.path().join("gaffer/config.toml")).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }

    // --- mask_token tests ---

    #[test]
    fn mask_token_shows_prefix_and_suffix() {
        assert_eq!(mask_token("gfr_abcdef123456"), "gfr_***3456");
    }

    #[test]
    fn mask_token_short_token() {
        assert_eq!(mask_token("abc"), "****");
        // Tokens under 12 chars are fully masked to avoid leaking overlapping chars
        assert_eq!(mask_token("abcdefg"), "****");
        assert_eq!(mask_token("abcdefghijk"), "****");
    }

    // --- write_config tests (existing function) ---

    #[test]
    fn write_config_creates_gaffer_dir_and_file() {
        let tmp = TempDir::new().unwrap();
        let patterns = vec!["**/*.xml".to_string()];
        write_config(tmp.path(), Some("tok"), Some("https://api"), &patterns).unwrap();

        let content = fs::read_to_string(tmp.path().join(".gaffer/config.toml")).unwrap();
        assert!(content.contains("token = \"tok\""));
        assert!(content.contains("api_url = \"https://api\""));
        assert!(content.contains("[test]"));
        assert!(content.contains("**/*.xml"));
    }
}
