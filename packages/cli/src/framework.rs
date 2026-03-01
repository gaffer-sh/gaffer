//! Framework detection and reporter configuration.
//!
//! Detects test frameworks by scanning for config files, checks if a reporter
//! is already configured, and provides copy-paste setup instructions.

use std::path::{Path, PathBuf};

use colored::Colorize;

const GAFFER_JUNIT_PATH: &str = ".gaffer/reports/junit.xml";
const GAFFER_VITEST_JSON_PATH: &str = ".gaffer/reports/vitest-results.json";
const GAFFER_PLAYWRIGHT_JSON_PATH: &str = ".gaffer/reports/playwright-results.json";

#[derive(Debug, Clone, PartialEq)]
pub enum Framework {
    Vitest(PathBuf),
    Playwright(PathBuf),
    Pytest(PytestConfig),
    Jest(PathBuf),
    Go,
    Rspec,
}

#[derive(Debug, Clone, PartialEq)]
pub enum PytestConfig {
    PyprojectToml(PathBuf),
    PytestIni(PathBuf),
    SetupCfg(PathBuf),
}

impl PytestConfig {
    pub fn path(&self) -> &Path {
        match self {
            PytestConfig::PyprojectToml(p) => p,
            PytestConfig::PytestIni(p) => p,
            PytestConfig::SetupCfg(p) => p,
        }
    }
}

impl std::fmt::Display for Framework {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Framework::Vitest(p) => write!(f, "vitest ({})", p.file_name().unwrap_or_default().to_string_lossy()),
            Framework::Playwright(p) => write!(f, "playwright ({})", p.file_name().unwrap_or_default().to_string_lossy()),
            Framework::Pytest(cfg) => write!(f, "pytest ({})", cfg.path().file_name().unwrap_or_default().to_string_lossy()),
            Framework::Jest(p) => write!(f, "jest ({})", p.file_name().unwrap_or_default().to_string_lossy()),
            Framework::Go => write!(f, "go test"),
            Framework::Rspec => write!(f, "rspec"),
        }
    }
}

#[derive(Debug)]
pub enum PatchResult {
    /// A supported reporter is already configured — no changes needed.
    AlreadyConfigured { file: PathBuf, format: &'static str },
    /// Print instructions for the user to configure manually.
    Instructions(String),
}

/// Detect all test frameworks in the project directory.
pub fn detect_frameworks(project_root: &Path) -> Vec<Framework> {
    let mut frameworks = Vec::new();

    let checks: Vec<(&[&str], Box<dyn Fn(PathBuf) -> Framework>)> = vec![
        // Vitest
        (
            &["vitest.config.ts", "vitest.config.js", "vitest.config.mts", "vitest.config.mjs"],
            Box::new(|p| Framework::Vitest(p)),
        ),
        // Playwright
        (
            &["playwright.config.ts", "playwright.config.js"],
            Box::new(|p| Framework::Playwright(p)),
        ),
        // Jest
        (
            &["jest.config.ts", "jest.config.js", "jest.config.mjs", "jest.config.cjs"],
            Box::new(|p| Framework::Jest(p)),
        ),
    ];

    for (filenames, constructor) in &checks {
        for filename in *filenames {
            let path = project_root.join(filename);
            if path.exists() {
                frameworks.push(constructor(path));
                break; // Only first matching extension per framework
            }
        }
    }

    // pytest — check pyproject.toml for [tool.pytest], then pytest.ini, then setup.cfg
    let pyproject = project_root.join("pyproject.toml");
    if pyproject.exists() {
        match std::fs::read_to_string(&pyproject) {
            Ok(content) => {
                if content.contains("[tool.pytest") {
                    frameworks.push(Framework::Pytest(PytestConfig::PyprojectToml(pyproject)));
                }
            }
            Err(e) => {
                eprintln!("  {} Could not read {}: {}", "Warning:".yellow().bold(), pyproject.display(), e);
            }
        }
    } else {
        let pytest_ini = project_root.join("pytest.ini");
        if pytest_ini.exists() {
            frameworks.push(Framework::Pytest(PytestConfig::PytestIni(pytest_ini)));
        } else {
            let setup_cfg = project_root.join("setup.cfg");
            if setup_cfg.exists() {
                match std::fs::read_to_string(&setup_cfg) {
                    Ok(content) => {
                        if content.contains("[tool:pytest]") {
                            frameworks.push(Framework::Pytest(PytestConfig::SetupCfg(setup_cfg)));
                        }
                    }
                    Err(e) => {
                        eprintln!("  {} Could not read {}: {}", "Warning:".yellow().bold(), setup_cfg.display(), e);
                    }
                }
            }
        }
    }

    // Go
    let go_mod = project_root.join("go.mod");
    if go_mod.exists() {
        frameworks.push(Framework::Go);
    }

    // RSpec
    let rspec = project_root.join(".rspec");
    let gemfile = project_root.join("Gemfile");
    if rspec.exists() {
        frameworks.push(Framework::Rspec);
    } else if gemfile.exists() {
        match std::fs::read_to_string(&gemfile) {
            Ok(content) => {
                if content.contains("rspec") {
                    frameworks.push(Framework::Rspec);
                }
            }
            Err(e) => {
                eprintln!("  {} Could not read {}: {}", "Warning:".yellow().bold(), gemfile.display(), e);
            }
        }
    }

    frameworks
}

/// Check if a framework already has a reporter configured, or provide
/// setup instructions for the user to add one manually.
pub fn check_reporter(framework: &Framework) -> PatchResult {
    match framework {
        Framework::Vitest(config_path) => check_or_instruct_js(config_path, vitest_instructions),
        Framework::Playwright(config_path) => check_or_instruct_js(config_path, playwright_instructions),
        Framework::Pytest(pytest_config) => check_or_instruct_pytest(pytest_config),
        Framework::Jest(config_path) => PatchResult::Instructions(jest_instructions(config_path)),
        Framework::Go => PatchResult::Instructions(go_instructions()),
        Framework::Rspec => PatchResult::Instructions(rspec_instructions()),
    }
}

// --- Shared helpers ---

/// Check if a JS/TS config file already has a supported reporter configured.
/// Returns the format name (e.g. "JUnit", "CTRF") if found, or None.
fn detect_js_reporter(content: &str) -> Option<&'static str> {
    if content.contains("'junit'") || content.contains("\"junit\"") {
        return Some("JUnit");
    }
    if content.contains("ctrf") {
        return Some("CTRF");
    }
    if (content.contains("'json'") || content.contains("\"json\"")) && content.contains(".gaffer/") {
        return Some("JSON");
    }
    None
}

// --- JS/TS frameworks (Vitest, Playwright) ---

/// Check if a JS/TS config already has a reporter; if not, return instructions.
fn check_or_instruct_js(config_path: &Path, instructions_fn: fn(&Path) -> String) -> PatchResult {
    match std::fs::read_to_string(config_path) {
        Ok(content) => {
            if let Some(format) = detect_js_reporter(&content) {
                return PatchResult::AlreadyConfigured {
                    file: config_path.to_path_buf(),
                    format,
                };
            }
        }
        Err(e) => {
            eprintln!(
                "  {} Could not read {}: {}",
                "Warning:".yellow().bold(),
                config_path.display(),
                e
            );
        }
    }

    PatchResult::Instructions(instructions_fn(config_path))
}

fn vitest_instructions(config_path: &Path) -> String {
    let filename = config_path.file_name().unwrap_or_default().to_string_lossy();
    format!(
        "Vitest — add a JSON reporter to {filename}:\n\n\
         {sp}reporters: ['default', 'json'],\n\
         {sp}outputFile: {{\n\
         {sp}{sp}json: '{path}',\n\
         {sp}}},\n",
        sp = "  ",
        path = GAFFER_VITEST_JSON_PATH
    )
}

fn playwright_instructions(config_path: &Path) -> String {
    let filename = config_path.file_name().unwrap_or_default().to_string_lossy();
    format!(
        "Playwright — add a JSON reporter to {filename}:\n\n\
         {sp}reporter: [\n\
         {sp}{sp}['html'],\n\
         {sp}{sp}['json', {{ outputFile: '{path}' }}],\n\
         {sp}],\n",
        sp = "  ",
        path = GAFFER_PLAYWRIGHT_JSON_PATH
    )
}

// --- Pytest ---

/// Check if pytest already has --junitxml configured; if not, show instructions.
fn check_or_instruct_pytest(config: &PytestConfig) -> PatchResult {
    match std::fs::read_to_string(config.path()) {
        Ok(content) => {
            if content.contains("--junitxml") {
                return PatchResult::AlreadyConfigured {
                    file: config.path().to_path_buf(),
                    format: "JUnit",
                };
            }
        }
        Err(e) => {
            eprintln!(
                "  {} Could not read {}: {}",
                "Warning:".yellow().bold(),
                config.path().display(),
                e
            );
        }
    }

    PatchResult::Instructions(pytest_instructions(config))
}

fn pytest_instructions(config: &PytestConfig) -> String {
    let filename = config.path().file_name().unwrap_or_default().to_string_lossy();
    match config {
        PytestConfig::PyprojectToml(_) => format!(
            "Pytest — add JUnit output to {filename}:\n\n\
             {sp}[tool.pytest.ini_options]\n\
             {sp}addopts = \"--junitxml={path}\"\n",
            sp = "  ",
            path = GAFFER_JUNIT_PATH
        ),
        _ => format!(
            "Pytest — add JUnit output to {filename}:\n\n\
             {sp}[pytest]\n\
             {sp}addopts = --junitxml={path}\n",
            sp = "  ",
            path = GAFFER_JUNIT_PATH
        ),
    }
}

// --- Instruction-only frameworks ---

fn jest_instructions(config_path: &Path) -> String {
    let filename = config_path.file_name().unwrap_or_default().to_string_lossy();
    format!(
        "Jest requires a reporter package for structured output.\n\n\
         Option 1 — JUnit:\n\
         {sp}1. Install: pnpm add -D jest-junit\n\
         {sp}2. Add to {filename} (or package.json):\n\n\
         {sp}{sp}reporters: ['default', ['jest-junit', {{\n\
         {sp}{sp}{sp}outputDirectory: '.gaffer/reports',\n\
         {sp}{sp}{sp}outputName: 'junit.xml',\n\
         {sp}{sp}}}]],\n\n\
         Option 2 — CTRF:\n\
         {sp}1. Install: pnpm add -D jest-ctrf-json-reporter\n\
         {sp}2. Add to {filename} (or package.json):\n\n\
         {sp}{sp}reporters: ['default', ['jest-ctrf-json-reporter', {{\n\
         {sp}{sp}{sp}outputDir: '.gaffer/reports',\n\
         {sp}{sp}}}]],\n",
        sp = "  ",
    )
}

fn go_instructions() -> String {
    format!(
        "Go test requires gotestsum for structured output.\n\n\
         Option 1 — JUnit:\n\
         {sp}1. Install: go install gotest.tools/gotestsum@latest\n\
         {sp}2. Run: gotestsum --junitfile {path} -- ./...\n\n\
         Option 2 — CTRF:\n\
         {sp}1. Install: go install github.com/ctrf-io/go-ctrf-json-reporter@latest\n\
         {sp}2. Run: go test -json ./... | go-ctrf-json-reporter -output .gaffer/reports/ctrf-report.json\n\n\
         Or wrap with gaffer:\n\
         {sp}gaffer test -- gotestsum --junitfile {path} -- ./...\n",
        path = GAFFER_JUNIT_PATH,
        sp = "   ",
    )
}

fn rspec_instructions() -> String {
    format!(
        "RSpec requires a reporter gem for structured output.\n\n\
         Option 1 — JUnit:\n\
         {sp}1. Install: gem install rspec_junit_formatter\n\
         {sp}2. Add to .rspec:\n\
         {sp}{sp}--format RspecJunitFormatter\n\
         {sp}{sp}--out {path}\n\n\
         {sp}Or keep default output too:\n\
         {sp}{sp}--format documentation\n\
         {sp}{sp}--format RspecJunitFormatter\n\
         {sp}{sp}--out {path}\n\n\
         Option 2 — CTRF:\n\
         {sp}1. Install: gem install rspec-ctrf-json-formatter\n\
         {sp}2. Add to .rspec:\n\
         {sp}{sp}--format CtrfJsonFormatter\n\
         {sp}{sp}--out .gaffer/reports/ctrf-report.json\n",
        path = GAFFER_JUNIT_PATH,
        sp = "   ",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn detect_vitest() {
        let dir = temp_dir();
        fs::write(dir.path().join("vitest.config.ts"), "export default {}").unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 1);
        assert!(matches!(fws[0], Framework::Vitest(_)));
    }

    #[test]
    fn detect_playwright() {
        let dir = temp_dir();
        fs::write(dir.path().join("playwright.config.ts"), "export default {}").unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 1);
        assert!(matches!(fws[0], Framework::Playwright(_)));
    }

    #[test]
    fn detect_pytest_pyproject() {
        let dir = temp_dir();
        fs::write(
            dir.path().join("pyproject.toml"),
            "[tool.pytest.ini_options]\naddopts = \"-v\"",
        )
        .unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 1);
        assert!(matches!(fws[0], Framework::Pytest(PytestConfig::PyprojectToml(_))));
    }

    #[test]
    fn detect_go() {
        let dir = temp_dir();
        fs::write(dir.path().join("go.mod"), "module example.com/foo").unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 1);
        assert!(matches!(fws[0], Framework::Go));
    }

    #[test]
    fn detect_none() {
        let dir = temp_dir();
        assert!(detect_frameworks(dir.path()).is_empty());
    }

    #[test]
    fn detects_vitest_and_playwright() {
        let dir = temp_dir();
        fs::write(dir.path().join("vitest.config.ts"), "").unwrap();
        fs::write(dir.path().join("playwright.config.ts"), "").unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 2);
        assert!(matches!(fws[0], Framework::Vitest(_)));
        assert!(matches!(fws[1], Framework::Playwright(_)));
    }

    #[test]
    fn detects_vitest_and_jest_separately() {
        let dir = temp_dir();
        fs::write(dir.path().join("vitest.config.ts"), "").unwrap();
        fs::write(dir.path().join("jest.config.js"), "").unwrap();
        let fws = detect_frameworks(dir.path());
        assert_eq!(fws.len(), 2);
        assert!(matches!(fws[0], Framework::Vitest(_)));
        assert!(matches!(fws[1], Framework::Jest(_)));
    }

    #[test]
    fn vitest_returns_instructions() {
        let dir = temp_dir();
        let config = dir.path().join("vitest.config.ts");
        fs::write(
            &config,
            "import { defineConfig } from 'vitest/config'\n\nexport default defineConfig({\n  test: {\n    globals: true,\n  },\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Vitest(config.clone()));
        match result {
            PatchResult::Instructions(msg) => {
                assert!(msg.contains("vitest-results.json"));
                assert!(msg.contains("'json'"));
            }
            other => panic!("Expected Instructions, got {:?}", other),
        }
    }

    #[test]
    fn vitest_already_configured_junit() {
        let dir = temp_dir();
        let config = dir.path().join("vitest.config.ts");
        fs::write(
            &config,
            "export default defineConfig({\n  reporters: ['default', 'junit'],\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Vitest(config.clone()));
        assert!(matches!(result, PatchResult::AlreadyConfigured { .. }));
    }

    #[test]
    fn vitest_already_configured_json() {
        let dir = temp_dir();
        let config = dir.path().join("vitest.config.ts");
        fs::write(
            &config,
            "export default defineConfig({\n  reporters: ['default', 'json'],\n  outputFile: {\n    json: '.gaffer/reports/vitest-results.json',\n  },\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Vitest(config.clone()));
        match result {
            PatchResult::AlreadyConfigured { format, .. } => assert_eq!(format, "JSON"),
            other => panic!("Expected AlreadyConfigured, got {:?}", other),
        }
    }

    #[test]
    fn playwright_returns_instructions() {
        let dir = temp_dir();
        let config = dir.path().join("playwright.config.ts");
        fs::write(
            &config,
            "import { defineConfig } from '@playwright/test'\n\nexport default defineConfig({\n  testDir: './e2e',\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Playwright(config.clone()));
        match result {
            PatchResult::Instructions(msg) => {
                assert!(msg.contains("playwright-results.json"));
                assert!(msg.contains("'json'"));
            }
            other => panic!("Expected Instructions, got {:?}", other),
        }
    }

    #[test]
    fn playwright_already_configured_json() {
        let dir = temp_dir();
        let config = dir.path().join("playwright.config.ts");
        fs::write(
            &config,
            "import { defineConfig } from '@playwright/test'\n\nexport default defineConfig({\n  reporter: [['html'], ['json', { outputFile: '.gaffer/reports/playwright-results.json' }]],\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Playwright(config.clone()));
        match result {
            PatchResult::AlreadyConfigured { format, .. } => assert_eq!(format, "JSON"),
            other => panic!("Expected AlreadyConfigured, got {:?}", other),
        }
    }

    #[test]
    fn pytest_returns_instructions() {
        let dir = temp_dir();
        let config = dir.path().join("pyproject.toml");
        fs::write(
            &config,
            "[tool.pytest.ini_options]\naddopts = \"-v\"\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Pytest(PytestConfig::PyprojectToml(config.clone())));
        match result {
            PatchResult::Instructions(msg) => {
                assert!(msg.contains("--junitxml"));
                assert!(msg.contains("pyproject.toml"));
            }
            other => panic!("Expected Instructions, got {:?}", other),
        }
    }

    #[test]
    fn pytest_already_configured() {
        let dir = temp_dir();
        let config = dir.path().join("pyproject.toml");
        fs::write(
            &config,
            "[tool.pytest.ini_options]\naddopts = \"--junitxml=results.xml\"\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Pytest(PytestConfig::PyprojectToml(config.clone())));
        assert!(matches!(result, PatchResult::AlreadyConfigured { .. }));
    }

    #[test]
    fn jest_returns_instructions() {
        let dir = temp_dir();
        let config = dir.path().join("jest.config.js");
        fs::write(&config, "module.exports = {}").unwrap();
        let result = check_reporter(&Framework::Jest(config));
        assert!(matches!(result, PatchResult::Instructions(_)));
    }

    #[test]
    fn go_returns_instructions() {
        let result = check_reporter(&Framework::Go);
        assert!(matches!(result, PatchResult::Instructions(_)));
    }

    #[test]
    fn vitest_ctrf_already_configured() {
        let dir = temp_dir();
        let config = dir.path().join("vitest.config.ts");
        fs::write(
            &config,
            "import { defineConfig } from 'vitest/config'\n\nexport default defineConfig({\n  reporters: ['default', 'vitest-ctrf-json-reporter'],\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Vitest(config.clone()));
        match result {
            PatchResult::AlreadyConfigured { format, .. } => assert_eq!(format, "CTRF"),
            other => panic!("Expected AlreadyConfigured, got {:?}", other),
        }
    }

    #[test]
    fn playwright_ctrf_already_configured() {
        let dir = temp_dir();
        let config = dir.path().join("playwright.config.ts");
        fs::write(
            &config,
            "import { defineConfig } from '@playwright/test'\n\nexport default defineConfig({\n  reporter: [['html'], ['playwright-ctrf-json-reporter']],\n})\n",
        )
        .unwrap();
        let result = check_reporter(&Framework::Playwright(config.clone()));
        match result {
            PatchResult::AlreadyConfigured { format, .. } => assert_eq!(format, "CTRF"),
            other => panic!("Expected AlreadyConfigured, got {:?}", other),
        }
    }
}
