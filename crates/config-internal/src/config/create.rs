use tracing::info;

use super::builder::NodeConfigBuilder;
use crate::error::Result;
use std::path::PathBuf;
use std::{fs, path::Path};

pub fn create_peppy_node_config(node_path: &Path, node_name: &str, full: bool) -> Result<PathBuf> {
    let peppy_yaml_path = node_path.join("peppy.yaml");

    let builder = if full {
        NodeConfigBuilder::full_node(node_name)
    } else {
        NodeConfigBuilder::simple_node(node_name)
    };

    builder
        .with_namespace("/")
        .with_logging_level("info")
        .write_to(&peppy_yaml_path)?;

    info!(
        "Created {} node in {}",
        &node_name,
        peppy_yaml_path.display()
    );
    Ok(peppy_yaml_path)
}

pub fn init_root_node(path: &Path, name: &str) -> Result<PathBuf> {
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;
    let peppy_yaml_path = path.join("peppy.yaml");

    NodeConfigBuilder::root_node(name)
        .with_namespace("/")
        .write_to(&peppy_yaml_path)?;

    info!("Created root node at {}", peppy_yaml_path.display());
    Ok(peppy_yaml_path)
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
        assert_eq!(peppy_yaml_path.file_name().unwrap(), "peppy.yaml");
    }

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- node create my_node
    #[test]
    fn test_create_peppy_config() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";

        let result = create_peppy_node_config(temp_dir.path(), &node_name, false);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.yaml");
        assert!(peppy_path.exists());
    }
}
