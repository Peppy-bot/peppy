use std::path::PathBuf;
use tracing::info;

use super::Command;
use crate::{Error, Result};

pub struct SyncCommand {
    pub file: PathBuf,
}

impl Command for SyncCommand {
    fn execute(self) -> Result<()> {
        let current_dir = std::env::current_dir()
            .map_err(|e| Error::SyncError(format!("Failed to get current directory: {}", e)))?;

        let full_path = if self.file.is_relative() {
            current_dir.join(self.file.strip_prefix("./").unwrap_or(&self.file))
        } else {
            self.file
        };

        info!("Syncing file: {}", full_path.display());
        // TODO: Implement the actual sync logic here
        Ok(())
    }
}
