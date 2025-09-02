use crate::error::{Error, Result};
use askama::Template;
use saphyr::{LoadableYamlNode, Yaml};
use serde::{Deserialize, Serialize};
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

/// Configuration parameters for node creation
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeParameters {
    // Dynamic parameters can be added here
}

/// Builder for creating YAML configuration files
pub struct NodeConfigBuilder {
    name: String, // TODO: Create a newtype pattern to validate the name
    namespace: String,
    version: String,
    auto_start: Option<bool>,
    respawn: Option<bool>,
    respawn_delay: Option<f64>,
    max_memory_mb: Option<u32>,
    cpu_affinity: Option<Vec<u32>>,
    logging_level: String,
    logging_file_path: Option<String>,
    validators: Vec<Box<dyn Validator>>,
    template_type: ConfigTemplateType,
}

impl Default for NodeConfigBuilder {
    fn default() -> Self {
        Self {
            name: String::new(),
            namespace: "/".to_string(),
            version: "0.1.0".to_string(),
            auto_start: None,
            respawn: None,
            respawn_delay: None,
            max_memory_mb: None,
            cpu_affinity: None,
            logging_level: "info".to_string(),
            logging_file_path: None,
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
        Self {
            name: name.into(),
            respawn: Some(true),
            respawn_delay: Some(1.0),
            logging_file_path: Some(format!("/var/log/peppy/{}_root.log", name)),
            validators: vec![Box::new(SyntaxValidator)],
            template_type: ConfigTemplateType::RootNode,
            ..Default::default()
        }
    }

    /// Creates a builder for simple node configuration
    pub fn simple_node(name: &str) -> Self {
        Self {
            name: name.into(),
            validators: vec![Box::new(SyntaxValidator)],
            template_type: ConfigTemplateType::SimpleNode,
            ..Default::default()
        }
    }

    /// Creates a builder for full node configuration
    pub fn full_node(name: &str) -> Self {
        Self {
            name: name.into(),
            respawn: Some(true),
            respawn_delay: Some(2.0),
            max_memory_mb: Some(1024),
            logging_file_path: Some(format!("/var/log/peppy/{}_node.log", name)),
            validators: vec![Box::new(SyntaxValidator)],
            template_type: ConfigTemplateType::FullNode,
            ..Default::default()
        }
    }

    /// Sets the node name
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Sets the namespace
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.namespace = namespace.into();
        self
    }

    /// Sets the version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Sets the logging level
    pub fn with_logging_level(mut self, level: impl Into<String>) -> Self {
        self.logging_level = level.into();
        self
    }

    /// Sets the logging file path
    pub fn with_logging_file_path(mut self, path: impl Into<String>) -> Self {
        self.logging_file_path = Some(path.into());
        self
    }

    /// Sets memory limit in MB
    pub fn with_memory_limit(mut self, limit_mb: u32) -> Self {
        self.max_memory_mb = Some(limit_mb);
        self
    }

    /// Sets CPU affinity
    pub fn with_cpu_affinity(mut self, cores: Vec<u32>) -> Self {
        self.cpu_affinity = Some(cores);
        self
    }

    /// Sets auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = Some(auto_start);
        self
    }

    /// Sets respawn
    pub fn with_respawn(mut self, respawn: bool, delay: f64) -> Self {
        self.respawn = Some(respawn);
        self.respawn_delay = Some(delay);
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
                let template = RootNodeTemplate { name: &self.name };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::SimpleNode => {
                let template = SimpleNodeTemplate {
                    name: &self.name,
                    namespace: &self.namespace,
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::FullNode => {
                let template = FullNodeTemplate {
                    name: &self.name,
                    namespace: &self.namespace,
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
