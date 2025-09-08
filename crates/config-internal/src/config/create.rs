use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use super::types::{
    ConfigTemplateType, Exposes, Logging, Name, Namespace, NodeConfig, QoSProfile, Resources, Topic,
};
use crate::{
    config::types::LogFormat,
    error::{Error, Result},
};

impl NodeConfig {
    /// Builds and writes the configuration to a file
    pub fn write_to(self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();

        // Serialize the populated config back to JSON5
        let json5 = serde_json5::to_string(&self).map_err(|e| Error::Serialize(e.to_string()))?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(json5.as_bytes())?;

        Ok(path.to_path_buf())
    }
}

pub struct NodeConfigCreator;

impl NodeConfigCreator {
    /// Renders the template to a string
    pub fn from_template(
        template_type: &ConfigTemplateType,
        node_name: &str,
        node_namespace: Option<&str>,
    ) -> Result<NodeConfig> {
        // Build a NodeConfig directly instead of rendering+parsing YAML
        let ns = node_namespace.unwrap_or("/");

        match template_type {
            ConfigTemplateType::RootNode => NodeConfigCreator::get_root_node_config(node_name),
            ConfigTemplateType::SimpleNode => {
                NodeConfigCreator::get_simple_node_config(node_name, ns)
            }
            ConfigTemplateType::FullNode => NodeConfigCreator::get_full_node_config(node_name, ns),
        }
    }

    fn get_root_node_config(node_name: &str) -> Result<NodeConfig> {
        let mut config = NodeConfig::default();
        config.node_config.name = Name::new(node_name)?;
        // Root node always lives in '/'
        config.node_config.namespace = Namespace::new("/")?;
        config.node_config.respawn = Some(true);
        config.node_config.respawn_delay = Some(1.0);

        // TODO: config.node_parameters What to do here? config.node_parameters can have an arbitrary structure
        // Example:
        // node_parameters:
        //   # Publishes its status
        //   status:
        //     frequency: 1Hz

        config.exposes = Some(Exposes {
            topics: Some(vec![Topic {
                topic_type: String::from("configuration/metadata"),
                name: String::from("/root_node/status"),
                qos_profile: QoSProfile::Standard,
            }]),
            services: None,
            actions: None,
        });

        config.logging = Some(Logging {
            min_level: String::from("info"),
            file_path: Some(format!(
                ".pixi/envs/default/var/log/peppy/{}_node.log",
                &config.node_config.name.as_str()
            )),
            max_file_size_mb: Some(10),
            format: LogFormat::default(),
        });
        Ok(config)
    }

    fn get_simple_node_config(node_name: &str, namespace: &str) -> Result<NodeConfig> {
        let mut config = NodeConfig::default();
        config.node_config.name = Name::new(node_name)?;
        config.node_config.namespace = Namespace::new(namespace.to_string())?;

        config.logging = Some(Logging {
            min_level: String::from("info"),
            file_path: None,
            max_file_size_mb: None,
            format: LogFormat::default(),
        });
        Ok(config)
    }

    fn get_full_node_config(node_name: &str, namespace: &str) -> Result<NodeConfig> {
        let mut config = NodeConfig::default();
        config.node_config.name = Name::new(node_name)?;
        config.node_config.namespace = Namespace::new(namespace)?;
        config.node_config.respawn = Some(true);
        config.node_config.respawn_delay = Some(1.0);

        config.resources = Some(Resources {
            max_memory_mb: Some(1024),
        });

        config.logging = Some(Logging {
            min_level: String::from("info"),
            file_path: Some(format!(
                ".pixi/envs/default/var/log/peppy/{}_node.log",
                &config.node_config.name.as_str()
            )),
            max_file_size_mb: Some(10),
            format: LogFormat::default(),
        });
        Ok(config)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::NamedTempFile;

    #[test]
    fn test_root_node_content_validation() {
        let node_name = "root_node";
        let template =
            NodeConfigCreator::from_template(&ConfigTemplateType::RootNode, node_name, None)
                .unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth (human-friendly JSON5, not strict JSON)
        let expected_json5 = r#"{
            node_config: {
                name: "root_node",
                namespace: "/",
                version: "0.1.0",
                respawn: true,
                respawn_delay: 1,
            },
            exposes: {
                topics: [
                    { type: "configuration/metadata", name: "/root_node/status", qos_profile: "standard" },
                ],
            },
            logging: {
                min_level: "info",
                file_path: ".pixi/envs/default/var/log/peppy/root_node_node.log",
                max_file_size_mb: 10,
                format: "text",
            },
        }"#;

        // Normalize by parsing both and comparing canonical JSON5 serialization
        let expected_cfg: NodeConfig = serde_json5::from_str(expected_json5).unwrap();
        let actual_cfg: NodeConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }

    #[test]
    fn test_simple_node_content_validation() {
        let node_name = "root_node";
        let namespace = "/ns";
        let template = NodeConfigCreator::from_template(
            &ConfigTemplateType::SimpleNode,
            node_name,
            Some(namespace),
        )
        .unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
            node_config: {
                name: "root_node",
                namespace: "/ns",
                version: "0.1.0",
            },
            logging: {
                min_level: "info",
                format: "text",
            },
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
        let node_name = "root_node";
        let namespace = "/ns";
        let template = NodeConfigCreator::from_template(
            &ConfigTemplateType::FullNode,
            node_name,
            Some(namespace),
        )
        .unwrap();

        // Write to a temporary file and read back the content
        let temp_file = NamedTempFile::new().unwrap();
        let temp_path = temp_file.path().to_path_buf();
        template.write_to(&temp_path).unwrap();
        let output = fs::read_to_string(&temp_path).unwrap();

        // JSON5 ground truth with human-friendly syntax
        let expected_json5 = r#"{
            node_config: {
                name: "root_node",
                namespace: "/ns",
                version: "0.1.0",
                respawn: true,
                respawn_delay: 1,
            },
            resources: {
                max_memory_mb: 1024,
            },
            logging: {
                min_level: "info",
                file_path: ".pixi/envs/default/var/log/peppy/root_node_node.log",
                max_file_size_mb: 10,
                format: "text",
            },
        }"#;

        // Normalize and compare canonical JSON5
        let expected_cfg: NodeConfig = serde_json5::from_str(expected_json5).unwrap();
        let actual_cfg: NodeConfig = serde_json5::from_str(&output).unwrap();
        let expected_min = serde_json5::to_string(&expected_cfg).unwrap();
        let actual_min = serde_json5::to_string(&actual_cfg).unwrap();
        assert_eq!(actual_min, expected_min);
    }
}
