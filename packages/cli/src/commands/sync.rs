//! `gaffer sync` — force sync pending uploads to the Gaffer dashboard.

use anyhow::{Context, Result};
use gaffer_core::types::GafferConfig;
use gaffer_core::GafferCore;

use crate::config::Config;
use crate::output::summary;

/// Force sync all pending uploads.
pub fn run(config: &Config) -> Result<()> {
    let core = GafferCore::new(GafferConfig {
        token: config.token.clone(),
        api_url: config.api_url.clone(),
        project_root: config.project_root.to_string_lossy().to_string(),
    })
    .context("Failed to initialize gaffer")?;

    if config.token.is_none() {
        anyhow::bail!("No token configured. Set GAFFER_TOKEN or add token to gaffer.toml");
    }

    let result = core.sync().context("Sync failed")?;
    summary::print_sync(&result);

    if result.synced == 0 && result.failed == 0 {
        eprintln!("No pending uploads to sync.");
    }

    Ok(())
}
