use tracing::info;

use crate::config::YamlConfigBuilder;
use crate::error::Result;
use std::path::PathBuf;
use std::{fs, path::Path};

pub fn create_peppy_node_config(node_path: &Path, node_name: &str) -> Result<()> {
    let peppy_yaml_path = node_path.join("peppy.yaml");

    // Use the new builder pattern
    YamlConfigBuilder::standard_node(node_name)
        .with_namespace("/")
        .with_logging_level("info")
        .write_to(peppy_yaml_path)?;

    Ok(())
}

pub fn init_root_node(path: &Path) -> Result<PathBuf> {
    // Create the directory if it doesn't exist
    fs::create_dir_all(path)?;

    let peppy_yaml_path = path.join("peppy.yaml");

    // Use the new builder pattern for root node
    YamlConfigBuilder::root_node()
        .with_namespace("/")
        .with_memory_limit(512)
        .write_to(&peppy_yaml_path)?;

    // TODO: Must also install the systemd service in the OS if it's not already the case"
    info!("Created root node at {}", peppy_yaml_path.display());
    Ok(peppy_yaml_path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use saphyr::{LoadableYamlNode, Yaml};

    #[test]
    fn test_init_root_node() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let non_existent_path = temp_dir.path().join("new_folder");

        assert!(!non_existent_path.exists());

        let peppy_yaml_path = init_root_node(&non_existent_path).unwrap();

        assert!(non_existent_path.exists());
        assert!(peppy_yaml_path.exists());
        assert_eq!(peppy_yaml_path.file_name().unwrap(), "peppy.yaml");

        let content = fs::read_to_string(&peppy_yaml_path).unwrap();
        assert!(content.contains("node_config:"));
        assert!(content.contains("namespace: \"/\""));
        assert!(content.contains("<root_node>"));

        // Validate that the generated file is valid YAML syntax
        let docs = Yaml::load_from_str(&content);
        assert!(
            docs.is_ok(),
            "Generated peppy.yaml file should be valid YAML syntax"
        );
    }

    // Can be run from the command line with:
    // cargo run --manifest-path <path_to_root_Cargo.toml> -- node create my_project
    #[test]
    fn test_create_peppy_config() {
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let node_name = "test_node";

        let result = create_peppy_node_config(temp_dir.path(), &node_name);
        assert!(result.is_ok());

        let peppy_path = temp_dir.path().join("peppy.yaml");
        assert!(peppy_path.exists());

        let content = fs::read_to_string(&peppy_path).unwrap();
        assert!(content.contains(node_name));

        // Validate that the generated file is valid YAML syntax
        let docs = Yaml::load_from_str(&content);
        assert!(docs.is_ok(), "Failed to parse peppy.yaml as valid YAML");
    }
}
