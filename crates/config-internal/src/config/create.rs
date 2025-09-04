use super::types::{
    ConfigTemplateType, Exposes, Logging, Name, Namespace, NodeConfig, QoSProfile, Resources, Topic,
};
use crate::{
    config::types::LogFormat,
    error::{Error, Result},
};

pub struct NodeConfigCreator;

impl NodeConfigCreator {
    /// Renders the template to a string
    pub fn from_template(
        template_type: &ConfigTemplateType,
        node_name: Option<&str>,
        node_namespace: Option<&str>,
    ) -> Result<NodeConfig> {
        // Build a NodeConfig directly instead of rendering+parsing YAML
        let name = node_name.ok_or_else(|| Error::ConfigParse("Missing node name".to_string()))?;
        let ns = node_namespace.unwrap_or("/");

        match template_type {
            ConfigTemplateType::RootNode => NodeConfigCreator::get_root_node_config(name),
            ConfigTemplateType::SimpleNode => NodeConfigCreator::get_simple_node_config(name, ns),
            ConfigTemplateType::FullNode => NodeConfigCreator::get_full_node_config(name, ns),
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

    #[test]
    fn test_root_node_content_validation() {
        let node_name = "root_node";
        let expected_content = format!(
            r#"node_config:
  name: {node_name}
  namespace: /
  version: 0.1.0
  respawn: true
  respawn_delay: 1.0
exposes:
  topics:
  - type: configuration/metadata
    name: /root_node/status
    qos_profile: standard
logging:
  min_level: info
  file_path: .pixi/envs/default/var/log/peppy/{node_name}_node.log
  max_file_size_mb: 10
  format: text
"#
        );
        let template =
            NodeConfigCreator::from_template(&ConfigTemplateType::RootNode, Some(node_name), None)
                .unwrap();

        // Convert NodeConfig to YAML string for comparison
        let yaml_output = serde_yaml::to_string(&template).unwrap();
        assert_eq!(yaml_output, expected_content);
    }

    #[test]
    fn test_simple_node_content_validation() {
        let node_name = "root_node";
        let namespace = "/ns";
        let expected_content = format!(
            r#"node_config:
  name: {node_name}
  namespace: {namespace}
  version: 0.1.0
logging:
  min_level: info
  format: text
"#
        );

        let template = NodeConfigCreator::from_template(
            &ConfigTemplateType::SimpleNode,
            Some(node_name),
            Some(namespace),
        )
        .unwrap();
        // Convert NodeConfig to YAML string for comparison
        let yaml_output = serde_yaml::to_string(&template).unwrap();
        assert_eq!(yaml_output, expected_content);
    }

    #[test]
    fn test_full_node_content_validation() {
        let node_name = "root_node";
        let namespace = "/ns";
        let expected_content = format!(
            r#"node_config:
  name: {node_name}
  namespace: {namespace}
  version: 0.1.0
  respawn: true
  respawn_delay: 1.0
resources:
  max_memory_mb: 1024
logging:
  min_level: info
  file_path: .pixi/envs/default/var/log/peppy/root_node_node.log
  max_file_size_mb: 10
  format: text
"#
        );
        let template = NodeConfigCreator::from_template(
            &ConfigTemplateType::FullNode,
            Some(node_name),
            Some(namespace),
        )
        .unwrap();
        // Convert NodeConfig to YAML string for comparison
        let yaml_output = serde_yaml::to_string(&template).unwrap();
        assert_eq!(yaml_output, expected_content);
    }
}
