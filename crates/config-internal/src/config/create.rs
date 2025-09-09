use super::types::{
    AnyValue, ConfigTemplateType, ExposedService, ExposedTopic, Exposes, Logging, MessageFormat,
    Name, Namespace, NodeConfig, QoSProfile, Resources, SubscribedService, SubscribedTopic,
};
use crate::format::prettify_json5;
use crate::{
    config::types::LogFormat,
    error::{Error, Result},
};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

impl NodeConfig {
    /// Builds and writes the configuration to a file
    pub fn write_to(self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let path = path.as_ref();

        // Serialize to JSON5, then pretty-format for readability
        let compact = serde_json5::to_string(&self).map_err(|e| Error::Serialize(e.to_string()))?;
        let json5 = prettify_json5(&compact);

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
        // Root node specific fields
        config.node_config.is_root = Some(true);
        config.node_config.name = Name::new(node_name)?;
        config.node_config.namespace = Namespace::new("/")?;
        config.node_config.auto_start = Some(true);
        config.node_config.respawn = Some(true);
        config.node_config.respawn_delay = Some(1.0);

        // node_parameters: { status: { frequency: "1Hz" } }
        let mut status = std::collections::BTreeMap::new();
        status.insert("frequency".to_string(), AnyValue::String("1Hz".to_string()));
        let mut parameters = std::collections::BTreeMap::new();
        parameters.insert(
            "status".to_string(),
            AnyValue::Object(status.into_iter().collect()),
        );
        config.node_parameters = Some(parameters);

        // subscribes_to: topics + services
        config.subscribes_to = Some(super::types::SubscribesTo {
            topics: Some(vec![SubscribedTopic {
                topic_type: "/peppy/status".to_string(),
                name: "{any}".to_string(),
                version: "{any}".to_string(),
                namespace: "/".to_string(),
                callback: "on_root_node_discovered".to_string(),
                optional: None,
            }]),
            services: Some(vec![SubscribedService {
                service_type: "/peppy/node".to_string(),
                name: "{any}".to_string(),
                version: "{any}".to_string(),
                namespace: "/".to_string(),
                callback: "on_payload_node_received".to_string(),
                optional: None,
            }]),
            actions: None,
        });

        // exposes: topics + services
        let mut topic_msg = MessageFormat::default();
        topic_msg
            .0
            .insert("name".to_string(), AnyValue::String("str".to_string()));
        let mut service_msg = MessageFormat::default();
        service_msg
            .0
            .insert("payload".to_string(), AnyValue::String("bytes".to_string()));
        config.exposes = Some(Exposes {
            topics: Some(vec![ExposedTopic {
                topic_type: "/peppy/status".to_string(),
                qos_profile: QoSProfile::Standard,
                message_format: Some(topic_msg),
                name: None,
            }]),
            services: Some(vec![ExposedService {
                service_type: "/peppy/node".to_string(),
                qos_profile: QoSProfile::Standard,
                message_format: Some(service_msg),
                name: None,
            }]),
            actions: Some(vec![]),
        });

        // resources
        config.resources = Some(Resources {
            max_memory_mb: Some(1024),
        });

        // logging
        config.logging = Some(Logging {
            min_level: String::from("info"),
            file_path: Some(
                std::path::Path::new(&crate::consts::env_root_dir())
                    .join("var")
                    .join("log")
                    .join("peppy")
                    .join("peppy_root.log")
                    .display()
                    .to_string(),
            ),
            max_file_size_mb: Some(100),
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
            file_path: Some(
                std::path::Path::new(&crate::consts::env_root_dir())
                    .join("var")
                    .join("log")
                    .join("peppy")
                    .join(format!("{}_node.log", config.node_config.name.as_str()))
                    .display()
                    .to_string(),
            ),
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

        let expected_json5 = r#"
        {
            node_config: {
                is_root: true,
                name: ""#
            .to_string()
            + node_name
            + r#"",
                namespace: "/",
                version: "0.1.0",
                auto_start: true,
                respawn: true,
                respawn_delay: 1.0
            },
            node_parameters: {
                status: {
                  frequency: "1Hz"
                }
            },
            subscribes_to: {
                topics: [
                  {
                    name: "{any}",
                    version: "{any}",
                    namespace: "/",
                    type: "/peppy/status",
                    callback: "on_root_node_discovered"
                  }
                ],
                services: [
                  {
                    name: "{any}",
                    version: "{any}",
                    namespace: "/",
                    type: "/peppy/node",
                    callback: "on_payload_node_received"
                  }
                ]
            },
            exposes: {
                topics: [
                  {
                    type: "/peppy/status",
                    qos_profile: "standard",
                    message_format: {
                      name: "str"
                    }
                  }
                ],
                services: [
                  {
                    type: "/peppy/node",
                    qos_profile: "standard",
                    message_format: {
                      payload: "bytes"
                    }
                  }
                ],
                actions: []
            },
            resources: {
                max_memory_mb: 1024
            },
            logging: {
                min_level: "info",
                file_path: ""#
            + crate::consts::env_root_dir()
            + r#"var/log/peppy/peppy_root.log",
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
                name: "a_node",
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
        let node_name = "a_node";
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
                name: "a_node",
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
                file_path: ".pixi/envs/default/var/log/peppy/a_node_node.log",
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
