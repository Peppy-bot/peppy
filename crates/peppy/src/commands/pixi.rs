use super::Command;
use crate::pixi;
use crate::{Error, Result};

pub struct PixiCommand {
    pub args: Vec<String>,
}

impl Command for PixiCommand {
    fn execute(self) -> Result<()> {
        // Use current directory as working directory
        let current_dir = std::env::current_dir()
            .map_err(|e| Error::PixiError(format!("Failed to get current directory: {}", e)))?;

        let facade =
            pixi::PixiFacade::new(current_dir).map_err(|e| Error::PixiError(e.to_string()))?;

        // Use execute_with_status to preserve original exit code behavior
        let status = facade
            .execute_with_status(&self.args)
            .map_err(|e| Error::PixiError(e.to_string()))?;

        if !status.success() {
            std::process::exit(status.code().unwrap_or(1));
        }

        Ok(())
    }
}
