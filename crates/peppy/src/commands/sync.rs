use std::path::PathBuf;

use super::{Command, CommandError};

pub struct SyncCommand {
    pub file: PathBuf,
}

impl Command for SyncCommand {
    fn execute(self) -> Result<(), CommandError> {
        let current_dir = std::env::current_dir()?;

        let full_path = if self.file.is_relative() {
            current_dir.join(self.file.strip_prefix("./").unwrap_or(&self.file))
        } else {
            self.file
        };

        println!("Syncing file: {}", full_path.display());
        // TODO: Implement the actual sync logic here
        Ok(())
    }
}
