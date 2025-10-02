use super::types::ConfigTemplateType;
use crate::error::{Error, Result};
use askama::Template;
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

#[derive(Template)]
#[template(path = "root_node.json5.j2")]
struct RootNodeTemplate<'a> {
    name: &'a str,
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
    template_type: ConfigTemplateType,
    name: String,
}

impl NodeConfigCreator {
    /// Creates a new NodeConfigCreator for a given template type and node metadata
    pub fn new(template_type: &ConfigTemplateType, node_name: &str) -> Self {
        Self {
            template_type: template_type.clone(),
            name: node_name.to_string(),
        }
    }

    /// Renders the chosen template and writes it to a file
    pub fn write_to(&self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();

        let rendered = match self.template_type {
            ConfigTemplateType::RootNode => {
                let tpl = RootNodeTemplate {
                    name: self.name.as_str(),
                    log_file_name: "peppy_root.log",
                };
                tpl.render().map_err(|e| Error::Serialize(e.to_string()))?
            }
            ConfigTemplateType::SimpleNode => {
                // Default command can be parameterized later
                let tpl = SimpleNodeTemplate {
                    name: self.name.as_str(),
                    launch_cmd: "[\"cargo\", \"run\", \"--release\"]",
                };
                tpl.render().map_err(|e| Error::Serialize(e.to_string()))?
            }
            ConfigTemplateType::FullNode => {
                let log_file_name = format!("{}_node.log", self.name);
                // Default command can be parameterized later
                let tpl = FullNodeTemplate {
                    name: self.name.as_str(),
                    launch_cmd: "[\"cargo\", \"run\", \"--release\"]",
                    log_file_name: &log_file_name,
                };
                tpl.render().map_err(|e| Error::Serialize(e.to_string()))?
            }
        };

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(rendered.as_bytes())?;

        Ok(path.to_path_buf())
    }
}

#[cfg(test)]
mod tests {
    use super::super::types::NodeConfig;
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[test]
    fn test_root_node_content_validation() {
        let node_name = "root_node";
        let template = NodeConfigCreator::new(&ConfigTemplateType::RootNode, node_name);

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        let expected_json5 = r#"
        {
            manifest: {
                name: ""#
            .to_string()
            + node_name
            + r#"",
                tag: "0.1.0",
                is_root_node: true
            },
            config: {
                auto_start: true,
                respawn: true,
                respawn_delay: 1.0
            },
            interfaces: {
              subscribes_to: {
                topics: [
                  {
                    node: "{any}",
                    tag: "{any}",
                    name: "peppy_node_status",
                    callback: "on_root_node_discovered"
                  }
                ],
                services: [
                  {
                    node: "{any}",
                    tag: "{any}",
                    name: "peppy_node_status",
                    callback: "on_payload_node_received"
                  }
                ]
              },
              exposes: {
                topics: [
                  {
                    name: "peppy_node_status",
                    qos_profile: "standard",
                    message_format: {
                      name: "str"
                    }
                  }
                ],
                services: [
                  {
                    name: "peppy_node_status",
                    qos_profile: "standard",
                    message_format: {
                      payload: "bytes"
                    }
                  }
                ],
                actions: []
              }
            },
            resources: {
                max_memory_mb: 1024
            },
            logging: {
                min_level: "info",
                // Will be stored in `.peppy/logs/` in dev mode and `/var/log/peppy/` (empty) in production
                file_name: "peppy_root.log",
                max_file_size_mb: 100,
                format: "text"
            }
        }"#;

        // Normalize by parsing both and comparing canonical JSON5 serialization
        let expected_cfg: NodeConfig = serde_json5::from_str(&expected_json5).unwrap();
        let actual_cfg: NodeConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }

    #[test]
    fn test_simple_node_content_validation() {
        let node_name = "a_node";
        let template = NodeConfigCreator::new(&ConfigTemplateType::SimpleNode, node_name);

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
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
        let template = NodeConfigCreator::new(&ConfigTemplateType::FullNode, node_name);

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
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
}
