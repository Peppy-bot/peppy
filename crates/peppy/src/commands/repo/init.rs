use std::sync::Arc;

use core_node::{InitOutcome, ensure_default_repos, repositories_list_path};
use daemon_config::consts::PeppyDirs;
use tracing::info;

use crate::context::AppContext;
use crate::error::{Error, Result};

/// `peppy repo init`: sync the user's `repositories.json5` with the bundled
/// default template by appending any missing default entries. Operates
/// directly on the local config file (no daemon connection required) so it
/// can be run after upgrading peppy without restarting the daemon.
pub(super) fn repo_init(_ctx: &Arc<AppContext>) -> Result<()> {
    let peppy_dirs = PeppyDirs::default();
    repo_init_with_dirs(&peppy_dirs)
}

pub fn repo_init_with_dirs(peppy_dirs: &PeppyDirs) -> Result<()> {
    match ensure_default_repos(peppy_dirs)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to sync default repositories: {e}")))?
    {
        InitOutcome::Created => {
            info!(
                "Created repositories.json5 with default entries at {}",
                repositories_list_path(peppy_dirs).display()
            );
        }
        InitOutcome::Updated { added: 0 } => {
            info!("repositories.json5 is already in sync with the default template.");
        }
        InitOutcome::Updated { added } => {
            info!(
                "Added {} missing default repositor{} to repositories.json5.",
                added,
                if added == 1 { "y" } else { "ies" }
            );
        }
    }
    Ok(())
}
