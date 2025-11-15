use std::{
    fs,
    path::{Path, PathBuf},
};

use super::Command;
use crate::{AppContext, Error, Result};
use config::node::NodeConfigCreator;
use tracing::info;

pub struct InstallCommand {
    pub node_name: String,
    pub in_dir: Option<PathBuf>,
}

impl Command for InstallCommand {
    fn execute(self, ctx: &AppContext) -> Result<()> {
        let current_dir = if let Some(in_dir) = self.in_dir {
            in_dir
        } else {
            ctx.root_dir.clone()
        };
        install_peppy_daemon(&current_dir)
            .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))?;
        Ok(())
    }
}

pub fn install_peppy_daemon(path: impl AsRef<Path>) -> Result<PathBuf> {
    let path = path.as_ref();
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;
    let peppy_config_path = path.join("peppy_launcher.json5");

    NodeConfigCreator::peppy_config("peppyd")
        .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))?
        .write_to(&peppy_config_path)
        .map_err(Error::PeppyConfig)?;

    info!("Created peppy config at {}", peppy_config_path.display());
    Ok(peppy_config_path)
}
