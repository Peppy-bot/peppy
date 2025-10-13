use crate::error::{Error, Result};
use askama::Template;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Template)]
#[template(path = "peppy_config.json5.j2")]
struct PeppyConfigTemplate<'a> {
    log_file_name: &'a str,
}

#[derive(Template)]
#[template(path = "simple_node.json5.j2")]
struct SimpleNodeTemplate<'a> {
    name: &'a str,
    launch_cmd: &'a str,
}

#[derive(Template)]
#[template(path = "full_node.json5.j2")]
struct FullNodeTemplate<'a> {
    name: &'a str,
    launch_cmd: &'a str,
    log_file_name: &'a str,
}

// Note: writing is handled by NodeConfigCreator using Askama templates

#[derive(Debug, Clone)]
pub struct NodeConfigCreator {
    redered_template: String,
}

impl NodeConfigCreator {
    pub fn simple_node(node_name: &str) -> Result<Self> {
        let tpl = SimpleNodeTemplate {
            name: node_name,
            launch_cmd: "[\"cargo\", \"run\", \"--release\"]",
        };
        let redered_template = tpl.render().map_err(|e| Error::Serialize(e.to_string()))?;

        Ok(Self { redered_template })
    }

    pub fn full_node(node_name: &str) -> Result<Self> {
        let log_file_name = format!("{}_node.log", node_name);
        // Default command can be parameterized later
        let tpl = FullNodeTemplate {
            name: node_name,
            launch_cmd: "[\"cargo\", \"run\", \"--release\"]",
            log_file_name: &log_file_name,
        };
        let redered_template = tpl.render().map_err(|e| Error::Serialize(e.to_string()))?;

        Ok(Self { redered_template })
    }

    pub fn peppy_config(log_file_name: &str) -> Result<Self> {
        let log_file_name = format!("{}.log", log_file_name);
        // Default command can be parameterized later
        let tpl = PeppyConfigTemplate {
            log_file_name: &log_file_name,
        };
        let redered_template = tpl.render().map_err(|e| Error::Serialize(e.to_string()))?;

        Ok(Self { redered_template })
    }

    /// Renders the chosen template and writes it to a file
    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(self.redered_template.as_bytes())?;

        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::NodeConfig;
    use super::*;
    use crate::config::PeppyConfig;
    use std::fs;
    use tempfile::NamedTempFile;

    #[test]
    fn test_simple_node_content_validation() {
        let node_name = "a_node";
        let template = NodeConfigCreator::simple_node(node_name).unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "a_node",
                tag: "0.1.0",
                launch_cmd: ["cargo", "run", "--release"],
            },
            logging: {
                min_level: "info",
                format: "text"
            }
        }"#;

        // Normalize and compare canonical JSON5
        let expected_cfg: NodeConfig = serde_json5::from_str(expected_json5).unwrap();
        let actual_cfg: NodeConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }

    #[test]
    fn test_full_node_content_validation() {
        let node_name = "a_node";
        let template = NodeConfigCreator::full_node(&node_name).unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "a_node",
                tag: "0.1.0",
                launch_cmd: ["cargo", "run", "--release"]
            },
            config: {
                respawn: true,
                respawn_delay: 1
            },
            resources: {
                max_memory_mb: 1024
            },
            logging: {
                min_level: "info",
                file_name: "a_node_node.log",
                max_file_size_mb: 10,
                format: "text"
            }
        }"#;

        // Normalize and compare canonical JSON5
        let expected_cfg: NodeConfig = serde_json5::from_str(expected_json5).unwrap();
        let actual_cfg: NodeConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }

    #[test]
    fn test_peppy_config_content_validation() {
        let log_file_name = "root_log";
        let template = NodeConfigCreator::peppy_config(log_file_name).unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
            deployments: [],
            logging: {
                min_level: "info",
                file_name: "root_log.log",
                max_file_size_mb: 20,
                format: "text"
            }
        }"#;

        // Normalize and compare canonical JSON5
        let expected_cfg: PeppyConfig = serde_json5::from_str(expected_json5).unwrap();
        let actual_cfg: PeppyConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }
}
