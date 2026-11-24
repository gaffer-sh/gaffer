//! `gaffer sync` — force sync pending uploads to the Gaffer dashboard.

use anyhow::{Context, Result};
use gaffer_core::types::GafferConfig;
use gaffer_core::GafferCore;

use crate::config::Config;
use crate::output::summary;

/// Force sync all pending uploads. `retry_exhausted` re-queues uploads that ran
/// out of retries first; `purge_exhausted` drops them and skips the sync.
pub fn run(config: &Config, retry_exhausted: bool, purge_exhausted: bool) -> Result<()> {
    let core = GafferCore::new(GafferConfig {
        token: config.token.clone(),
        api_url: config.api_url.clone(),
        project_root: config.project_root.to_string_lossy().to_string(),
    })
    .context("Failed to initialize gaffer")?;

    if purge_exhausted {
        let purged = core.purge_exhausted_uploads().context("Failed to purge uploads")?;
        eprintln!("Purged {} exhausted upload(s).", purged);
        return Ok(());
    }

    if config.token.is_none() {
        anyhow::bail!(
            "No token configured. Set GAFFER_PROJECT_TOKEN, add token to gaffer.toml, or on GitHub \
             Actions grant `permissions: id-token: write` for automatic authentication."
        );
    }

    if retry_exhausted {
        let reset = core.retry_exhausted_uploads().context("Failed to re-queue uploads")?;
        eprintln!("Re-queued {} exhausted upload(s).", reset);
    }

    let result = core.sync().context("Sync failed")?;
    summary::print_sync(&result);

    if result.synced == 0 && result.failed == 0 {
        eprintln!("No pending uploads to sync.");
    }

    Ok(())
}
