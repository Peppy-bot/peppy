use super::types::{Exposes, Logging, NodeConfig, NodeInfo, NodeParameters, Resources};
use crate::error::{Error, Result};
use askama::Template;
use saphyr::{LoadableYamlNode, Yaml};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Template for root node configuration
#[derive(Template)]
#[template(path = "root_node.yaml.j2")]
struct RootNodeTemplate<'a> {
    name: &'a str,
}

/// Template for simple node configuration
#[derive(Template)]
#[template(path = "peppy_new_node_simple.yaml.j2")]
struct SimpleNodeTemplate<'a> {
    name: &'a str,
    namespace: &'a str,
}

/// Template for full node configuration
#[derive(Template)]
#[template(path = "peppy_new_node_full.yaml.j2")]
struct FullNodeTemplate<'a> {
    name: &'a str,
    namespace: &'a str,
}

/// Trait for validating YAML configurations
pub trait Validator: Send + Sync {
    fn validate(&self, content: &str) -> Result<()>;
}

/// Validates YAML syntax
pub struct SyntaxValidator;

impl Validator for SyntaxValidator {
    fn validate(&self, content: &str) -> Result<()> {
        Yaml::load_from_str(content)
            .map_err(|e| Error::ConfigParse(format!("YAML syntax validation failed: {}", e)))?;
        Ok(())
    }
}

/// Builder for creating YAML configuration files
pub struct NodeConfigBuilder {
    config: NodeConfig,
    validators: Vec<Box<dyn Validator>>,
    template_type: ConfigTemplateType,
}

impl Default for NodeConfigBuilder {
    fn default() -> Self {
        Self {
            config: NodeConfig {
                node_config: NodeInfo {
                    name: String::new(),
                    namespace: "/".to_string(),
                    version: "0.1.0".to_string(),
                    auto_start: false,
                    respawn: false,
                    respawn_delay: 1.0,
                },
                node_parameters: NodeParameters::default(),
                exposes: Exposes::default(),
                resources: Resources::default(),
                logging: Logging::default(),
            },
            validators: Vec::new(),
            template_type: ConfigTemplateType::default(),
        }
    }
}

impl Default for ConfigTemplateType {
    fn default() -> Self {
        ConfigTemplateType::SimpleNode
    }
}

/// Supported template types
#[derive(Debug, Clone)]
pub enum ConfigTemplateType {
    RootNode,
    SimpleNode,
    FullNode,
}

impl NodeConfigBuilder {
    /// Creates a builder for root node configuration
    pub fn root_node(name: &str) -> Self {
        let mut builder = Self::default();
        builder.config.node_config.name = name.into();
        builder.config.node_config.respawn = true;
        builder.config.node_config.respawn_delay = 1.0;
        builder.config.logging.file_path = format!("/var/log/peppy/{}_root.log", name);
        builder.validators = vec![Box::new(SyntaxValidator)];
        builder.template_type = ConfigTemplateType::RootNode;
        builder
    }

    /// Creates a builder for simple node configuration
    pub fn simple_node(name: &str) -> Self {
        let mut builder = Self::default();
        builder.config.node_config.name = name.into();
        builder.validators = vec![Box::new(SyntaxValidator)];
        builder.template_type = ConfigTemplateType::SimpleNode;
        builder
    }

    /// Creates a builder for full node configuration
    pub fn full_node(name: &str) -> Self {
        let mut builder = Self::default();
        builder.config.node_config.name = name.into();
        builder.config.node_config.respawn = true;
        builder.config.node_config.respawn_delay = 2.0;
        builder.config.resources.max_memory_mb = 1024;
        builder.config.logging.file_path = format!("/var/log/peppy/{}_node.log", name);
        builder.validators = vec![Box::new(SyntaxValidator)];
        builder.template_type = ConfigTemplateType::FullNode;
        builder
    }

    /// Sets the node name
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.config.node_config.name = name.into();
        self
    }

    /// Sets the namespace
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.config.node_config.namespace = namespace.into();
        self
    }

    /// Sets the version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.config.node_config.version = version.into();
        self
    }

    /// Sets the logging level
    pub fn with_logging_level(mut self, level: impl Into<String>) -> Self {
        self.config.logging.min_level = level.into();
        self
    }

    /// Sets the logging file path
    pub fn with_logging_file_path(mut self, path: impl Into<String>) -> Self {
        self.config.logging.file_path = path.into();
        self
    }

    /// Sets memory limit in MB
    pub fn with_memory_limit(mut self, limit_mb: u32) -> Self {
        self.config.resources.max_memory_mb = limit_mb;
        self
    }

    /// Sets CPU affinity
    pub fn with_cpu_affinity(mut self, cores: Vec<u32>) -> Self {
        self.config.resources.cpu_affinity = cores;
        self
    }

    /// Sets auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.config.node_config.auto_start = auto_start;
        self
    }

    /// Sets respawn
    pub fn with_respawn(mut self, respawn: bool, delay: f64) -> Self {
        self.config.node_config.respawn = respawn;
        self.config.node_config.respawn_delay = delay;
        self
    }

    /// Adds a custom validator
    pub fn add_validator(mut self, validator: Box<dyn Validator>) -> Self {
        self.validators.push(validator);
        self
    }

    /// Validates the configuration without writing
    pub fn validate(&self) -> Result<String> {
        let content = self.render()?;

        for validator in &self.validators {
            validator.validate(&content)?;
        }

        Ok(content)
    }

    /// Renders the template to a string
    fn render(&self) -> Result<String> {
        match &self.template_type {
            ConfigTemplateType::RootNode => {
                let template = RootNodeTemplate {
                    name: &self.config.node_config.name,
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::SimpleNode => {
                let template = SimpleNodeTemplate {
                    name: &self.config.node_config.name,
                    namespace: &self.config.node_config.namespace,
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::FullNode => {
                let template = FullNodeTemplate {
                    name: &self.config.node_config.name,
                    namespace: &self.config.node_config.namespace,
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
        }
    }

    /// Builds and writes the configuration to a file
    pub fn write_to(self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let content = self.validate()?;
        let path = path.as_ref();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(content.as_bytes())?;

        Ok(path.to_path_buf())
    }

    /// Builds the configuration and returns it as a string
    pub fn build(self) -> Result<String> {
        self.validate()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_root_node_content_validation() {
        let content = NodeConfigBuilder::root_node("test_root").build().unwrap();

        assert!(content.contains("test_root"));
        assert!(content.contains("is_root: true"));
        assert!(content.contains("namespace: \"/\""));

        // Validate YAML syntax
        let docs = Yaml::load_from_str(&content);
        assert!(docs.is_ok(), "Root node config should be valid YAML");
    }

    #[test]
    fn test_simple_node_content_validation() {
        let content = NodeConfigBuilder::simple_node("test_simple")
            .with_namespace("/robot")
            .with_logging_level("info")
            .build()
            .unwrap();

        assert!(content.contains("test_simple"));
        assert!(content.contains("/robot"));

        // Validate YAML syntax
        let docs = Yaml::load_from_str(&content);
        assert!(docs.is_ok(), "Simple node config should be valid YAML");
    }

    #[test]
    fn test_full_node_content_validation() {
        let content = NodeConfigBuilder::full_node("test_full")
            .with_namespace("/system")
            .build()
            .unwrap();

        assert!(content.contains("test_full"));
        assert!(content.contains("/system"));
        assert!(content.contains("respawn"));

        // Validate YAML syntax
        let docs = Yaml::load_from_str(&content);
        assert!(docs.is_ok(), "Full node config should be valid YAML");
    }
}
