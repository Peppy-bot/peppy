use super::parse::NodeConfigParser;
use super::types::{ConfigTemplateType, Logging, Name, Namespace, NodeConfig, Resources};
use crate::config::create::NodeConfigCreator;
use crate::error::{Error, Result};
use saphyr::{LoadableYamlNode, Yaml};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

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

enum ConfigSource {
    Template(ConfigTemplateType),
    Yaml(String),
}

/// Builder for creating YAML configuration files
pub struct NodeConfigBuilder {
    pub config: NodeConfig,
    validators: Vec<Box<dyn Validator>>,
    config_source: ConfigSource,
}

impl NodeConfigBuilder {
    /// Creates a new node based on a predefined template
    pub fn from_template(template_type: ConfigTemplateType) -> Self {
        Self {
            config: NodeConfig::default(),
            validators: Vec::new(),
            config_source: ConfigSource::Template(template_type),
        }
    }

    /// Creates a builder from a YAML string
    pub fn from_yaml(content: &str) -> Self {
        Self {
            config: NodeConfig::default(),
            validators: vec![Box::new(SyntaxValidator)],
            config_source: ConfigSource::Yaml(content.into()),
        }
    }

    /// Sets the node name
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.config.node_config.name =
            Name::new(name.into()).expect("invalid node name passed to builder");
        self
    }

    /// Sets the namespace
    pub fn with_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.config.node_config.namespace =
            Namespace::new(namespace.into()).expect("invalid namespace passed to builder");
        self
    }

    /// Sets the max_memory in resources
    pub fn with_max_memory_mb(mut self, max_memory_mb: u32) -> Self {
        if self.config.resources.is_none() {
            self.config.resources = Some(Resources::default());
        }
        if let Some(ref mut resources) = self.config.resources {
            resources.max_memory_mb = Some(max_memory_mb);
        }
        self
    }

    /// Sets the version
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.config.node_config.version = version.into();
        self
    }

    /// Sets the logging level
    pub fn with_logging_level(mut self, level: impl Into<String>) -> Self {
        if self.config.logging.is_none() {
            self.config.logging = Some(Logging::default());
        }
        if let Some(ref mut logging) = self.config.logging {
            logging.min_level = level.into();
        }
        self
    }

    /// Sets the logging file path
    pub fn with_logging_file_path(mut self, path: impl Into<String>) -> Self {
        if self.config.logging.is_none() {
            self.config.logging = Some(Logging::default());
        }
        if let Some(ref mut logging) = self.config.logging {
            logging.file_path = Some(path.into());
        }
        self
    }

    /// Sets memory limit in MB
    pub fn with_memory_limit(mut self, limit_mb: u32) -> Self {
        if self.config.resources.is_none() {
            self.config.resources = Some(Resources::default());
        }
        if let Some(ref mut resources) = self.config.resources {
            resources.max_memory_mb = Some(limit_mb);
        }
        self
    }

    /// Sets auto-start
    pub fn with_auto_start(mut self, auto_start: bool) -> Self {
        self.config.node_config.auto_start = Some(auto_start);
        self
    }

    /// Sets respawn
    pub fn with_respawn(mut self, respawn: bool, delay: f64) -> Self {
        self.config.node_config.respawn = Some(respawn);
        self.config.node_config.respawn_delay = Some(delay);
        self
    }

    /// Adds a custom validator
    pub fn add_validator(mut self, validator: Box<dyn Validator>) -> Self {
        self.validators.push(validator);
        self
    }

    /// Validates the configuration without writing
    fn validate(&self, content: String) -> Result<String> {
        for validator in &self.validators {
            validator.validate(&content)?;
        }

        Ok(content)
    }

    /// Builds the configuration and returns it as a string
    pub fn build(&self) -> Result<NodeConfig> {
        match &self.config_source {
            ConfigSource::Template(t) => NodeConfigCreator::from_template(
                t,
                Some(self.config.node_config.name.as_str()),
                Some(self.config.node_config.namespace.as_str()),
            ),
            ConfigSource::Yaml(content) => {
                // Validation only useful for provided Yaml
                self.validate(content.into())?;
                NodeConfigParser::from_content(content)
            }
        }
    }

    /// Builds and writes the configuration to a file
    pub fn write_to(self, path: impl AsRef<Path>) -> Result<PathBuf> {
        let content = self.build()?;
        let path = path.as_ref();

        // Serialize the populated config back to YAML
        // FIXME: Replace deprecated serde_yaml when a good alternative pops up, like `saphyr-serde`
        let yaml = serde_yaml::to_string(&content)
            .map_err(|e| Error::ConfigParse(format!("Failed to serialize YAML: {}", e)))?;

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }

        let mut file = fs::File::create(path)?;
        file.write_all(yaml.as_bytes())?;

        Ok(path.to_path_buf())
    }
}
