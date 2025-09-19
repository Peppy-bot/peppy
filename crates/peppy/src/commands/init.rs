use std::{
    fs,
    path::{Path, PathBuf},
};

use super::Command;
use crate::{AppContext, Error, Result};
use config::{ConfigTemplateType, NodeConfigCreator};
use tracing::info;

pub struct InitCommand {
    pub node_name: String,
    pub in_dir: Option<PathBuf>,
}

impl Command for InitCommand {
    fn execute(self, ctx: &AppContext) -> Result<()> {
        let current_dir = if let Some(in_dir) = self.in_dir {
            in_dir
        } else {
            ctx.root_dir.clone()
        };
        init_root_node(&current_dir, &self.node_name)
            .map_err(|e| crate::Error::ExecutionFailed(e.to_string()))?;
        Ok(())
    }
}

pub fn init_root_node(path: impl AsRef<Path>, name: &str) -> Result<PathBuf> {
    let path = path.as_ref();
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;
    let peppy_config_path = path.join("peppy.json5");

    NodeConfigCreator::new(&ConfigTemplateType::RootNode, name, &config::Language::Rust)
        .write_to(&peppy_config_path)
        .map_err(Error::PeppyConfig)?;

    info!("Created root node at {}", peppy_config_path.display());
    Ok(peppy_config_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- init <node_name>
    #[test]
    fn test_init_root_node() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let non_existent_path = temp_dir.path().join("new_folder");

        assert!(!non_existent_path.exists());

        let peppy_yaml_path = init_root_node(&non_existent_path, "root_node").unwrap();

        assert!(non_existent_path.exists());
        assert!(peppy_yaml_path.exists());
        assert_eq!(peppy_yaml_path.file_name().unwrap(), "peppy.json5");
    }
}
