use std::path::PathBuf;

use super::Command;
use crate::Result;
use config::init_root_node;

pub struct InitCommand {
    pub node_name: String,
    pub in_dir: Option<PathBuf>,
}

impl Command for InitCommand {
    fn execute(self) -> Result<()> {
        let current_dir = if let Some(in_dir) = self.in_dir {
            in_dir
        } else {
            std::env::current_dir()?
        };
        init_root_node(&current_dir, &self.node_name)
            .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))?;
        Ok(())
    }
}
