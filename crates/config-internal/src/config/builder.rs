use crate::error::{Error, Result};
use askama::Template;
use saphyr::{LoadableYamlNode, Yaml};
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Template for root node configuration
#[derive(Template)]
#[template(path = "init.yaml.j2")]
struct RootNodeTemplate<'a> {
    namespace: &'a str,
    max_memory_mb: u32,
    cpu_affinity: &'a Vec<u32>,
    logging_level: &'a str,
    logging_file_path: &'a str,
}

/// Template for standard node configuration
#[derive(Template)]
#[template(path = "peppy_new_node_simple.yaml.j2")]
struct StandardNodeTemplate<'a> {
    name: &'a str,
    namespace: &'a str,
    logging_level: &'a str,
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
pub struct YamlConfigBuilder {
    name: String,
    namespace: String,
    version: String,
    auto_start: bool,
    respawn: bool,
    respawn_delay: f64,
    max_memory_mb: u32,
    cpu_affinity: Vec<u32>,
    logging_level: String,
    logging_file_path: String,
    validators: Vec<Box<dyn Validator>>,
    template_type: ConfigTemplateType,
}

/// Supported template types
#[derive(Debug, Clone)]
pub enum ConfigTemplateType {
    RootNode,
    StandardNode,
    Custom(String), // Path to custom template
}

impl Default for YamlConfigBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl YamlConfigBuilder {
    /// Creates a new builder with default settings
    pub fn new() -> Self {
        Self {
            name: String::new(),
            namespace: "/".to_string(),
            version: "0.1.0".to_string(),
            auto_start: true,
            respawn: false,
            respawn_delay: 2.0,
            max_memory_mb: 512,
            cpu_affinity: Vec::new(),
            logging_level: "info".to_string(),
            logging_file_path: String::new(),
            validators: vec![Box::new(SyntaxValidator)],
            template_type: ConfigTemplateType::StandardNode,
        }
    }

    /// Creates a builder for root node configuration
    pub fn root_node() -> Self {
        let mut builder = Self::new();
        builder.template_type = ConfigTemplateType::RootNode;
        builder.name = "<root_node>".to_string();
        builder
    }

    /// Creates a builder for standard node configuration
    pub fn standard_node(name: impl Into<String>) -> Self {
        let mut builder = Self::new();
        builder.name = name.into();
        builder
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
        self.logging_file_path = path.into();
        self
    }

    /// Sets memory limit in MB
    pub fn with_memory_limit(mut self, limit_mb: u32) -> Self {
        self.max_memory_mb = limit_mb;
        self
    }

    /// Sets CPU affinity
    pub fn with_cpu_affinity(mut self, cores: Vec<u32>) -> Self {
        self.cpu_affinity = cores;
        self
    }

    /// Sets auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.auto_start = auto_start;
        self
    }

    /// Sets respawn
    pub fn with_respawn(mut self, respawn: bool, delay: f64) -> Self {
        self.respawn = respawn;
        self.respawn_delay = delay;
        self
    }

    /// Adds a custom validator
    pub fn add_validator(mut self, validator: Box<dyn Validator>) -> Self {
        self.validators.push(validator);
        self
    }

    /// Uses a custom template
    pub fn with_custom_template(mut self, template_path: impl Into<String>) -> Self {
        self.template_type = ConfigTemplateType::Custom(template_path.into());
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
                    namespace: &self.namespace,
                    max_memory_mb: self.max_memory_mb,
                    cpu_affinity: &self.cpu_affinity,
                    logging_level: &self.logging_level,
                    logging_file_path: if self.logging_file_path.is_empty() {
                        "/var/log/peppy/peppy_root.log"
                    } else {
                        &self.logging_file_path
                    },
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::StandardNode => {
                let template = StandardNodeTemplate {
                    name: &self.name,
                    namespace: &self.namespace,
                    logging_level: &self.logging_level,
                };
                template
                    .render()
                    .map_err(|e| Error::AskamaError(e.to_string()))
            }
            ConfigTemplateType::Custom(template_path) => {
                // For custom templates, we'd need to handle dynamic template loading
                // This is a placeholder for now
                Err(Error::ConfigParse(format!(
                    "Custom template loading not yet implemented: {}",
                    template_path
                )))
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
    use tempfile::TempDir;

    #[test]
    fn test_builder_standard_node() {
        let content = YamlConfigBuilder::standard_node("test_node")
            .with_namespace("/robot")
            .with_logging_level("debug")
            .build()
            .unwrap();

        assert!(content.contains("test_node"));
        assert!(content.contains("/robot"));
        assert!(content.contains("debug"));
    }

    #[test]
    fn test_builder_root_node() {
        let content = YamlConfigBuilder::root_node()
            .with_namespace("/system")
            .with_memory_limit(1024)
            .with_cpu_affinity(vec![0, 1, 2])
            .build()
            .unwrap();

        assert!(content.contains("<root_node>"));
        assert!(content.contains("/system"));
        assert!(content.contains("1024"));
        assert!(content.contains("[0, 1, 2]"));
    }

    #[test]
    fn test_builder_write_to_file() {
        let temp_dir = TempDir::new().unwrap();
        let config_path = temp_dir.path().join("test.yaml");

        let result = YamlConfigBuilder::standard_node("file_test")
            .with_namespace("/app")
            .write_to(&config_path);

        assert!(result.is_ok());
        assert!(config_path.exists());

        let content = fs::read_to_string(&config_path).unwrap();
        assert!(content.contains("file_test"));
        assert!(content.contains("/app"));
    }

    #[test]
    fn test_fluent_api_chaining() {
        let content = YamlConfigBuilder::new()
            .with_name("chained_node")
            .with_namespace("/app")
            .with_logging_level("warn")
            .with_memory_limit(256)
            .build()
            .unwrap();

        assert!(content.contains("chained_node"));
        assert!(content.contains("/app"));
        assert!(content.contains("warn"));
    }
}
