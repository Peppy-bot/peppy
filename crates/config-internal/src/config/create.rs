use super::types::{ConfigTemplateType, Name, Namespace, NodeConfig};
use crate::error::Error;
use crate::error::Result;

pub struct NodeConfigCreator;

impl NodeConfigCreator {
    /// Renders the template to a string
    pub fn render(
        template_type: &ConfigTemplateType,
        node_name: Option<&str>,
        node_namespace: Option<&str>,
    ) -> Result<NodeConfig> {
        // Build a NodeConfig directly instead of rendering+parsing YAML
        let name = node_name.ok_or_else(|| Error::ConfigParse("Missing node name".to_string()))?;
        let ns = node_namespace.unwrap_or("/");

        let mut config = NodeConfig::default();

        match template_type {
            ConfigTemplateType::RootNode => {
                config.node_config.name = Name::new(name.to_string())?;
                // Root node always lives in '/'
                config.node_config.namespace = Namespace::new("/")?;
                config.node_config.respawn = true;
                config.node_config.respawn_delay = 1.0;
                // logging and other sections keep defaults
            }
            ConfigTemplateType::SimpleNode => {
                config.node_config.name = Name::new(name.to_string())?;
                config.node_config.namespace = Namespace::new(ns.to_string())?;
                // Defaults for version and others are already set
                config.logging.min_level = "info".to_string();
            }
            ConfigTemplateType::FullNode => {
                config.node_config.name = Name::new(name.to_string())?;
                config.node_config.namespace = Namespace::new(ns.to_string())?;
                config.node_config.respawn = true;
                config.node_config.respawn_delay = 2.0;
                config.resources.max_memory_mb = 1024;
                config.logging.min_level = "info".to_string();
                config.logging.file_path =
                    format!(".pixi/envs/default/var/log/peppy/{}_node.log", name);
            }
        }

        Ok(config)
    }
}
