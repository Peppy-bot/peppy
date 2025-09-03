use super::parse::NodeConfigParser;
use super::types::{ConfigTemplateType, Name, Namespace, NodeConfig};
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
    fn validate(&self, content: String) -> Result<String> {
        for validator in &self.validators {
            validator.validate(&content)?;
        }

        Ok(content)
    }

    /// Builds the configuration and returns it as a string
    pub fn build(&self) -> Result<NodeConfig> {
        match &self.config_source {
            ConfigSource::Template(t) => NodeConfigCreator::render(
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    use askama::Template;

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

    #[test]
    fn test_root_node_content_validation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("peppy.yaml");
        let node_name = "root_node";

        // Write using write_to
        let builder =
            NodeConfigBuilder::from_template(ConfigTemplateType::RootNode).with_name(node_name);
        builder.write_to(&file_path).unwrap();
        let written_content = fs::read_to_string(&file_path).unwrap();

        let template = RootNodeTemplate { name: node_name };
        let expected_content = template
            .render()
            .map_err(|e| Error::AskamaError(e.to_string()))
            .unwrap();

        // The written content should match the serialized config
        assert_eq!(written_content, expected_content);
    }

    #[test]
    fn test_simple_node_content_validation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("peppy.yaml");

        let node_name = "simple_node";
        let namespace = "/";

        // Write using write_to
        let builder = NodeConfigBuilder::from_template(ConfigTemplateType::SimpleNode)
            .with_name(node_name)
            .with_namespace(namespace);
        builder.write_to(&file_path).unwrap();
        let written_content = fs::read_to_string(&file_path).unwrap();

        let template = SimpleNodeTemplate {
            name: node_name,
            namespace: namespace,
        };
        let expected_content = template
            .render()
            .map_err(|e| Error::AskamaError(e.to_string()))
            .unwrap();

        // The written content should match the serialized config
        assert_eq!(written_content, expected_content);
    }

    #[test]
    fn test_full_node_content_validation() {
        let dir = tempdir().unwrap();
        let file_path = dir.path().join("peppy.yaml");

        let node_name = "full_node";
        let namespace = "/";

        // Write using write_to
        let builder = NodeConfigBuilder::from_template(ConfigTemplateType::FullNode)
            .with_name(node_name)
            .with_namespace(namespace);
        builder.write_to(&file_path).unwrap();
        let written_content = fs::read_to_string(&file_path).unwrap();

        let template = FullNodeTemplate {
            name: node_name,
            namespace: namespace,
        };
        let expected_content = template
            .render()
            .map_err(|e| Error::AskamaError(e.to_string()))
            .unwrap();

        // The written content should match the serialized config
        assert_eq!(written_content, expected_content);
    }

    #[test]
    fn test_parse_simple_node_yaml() {
        let yaml_content = r#"
node_config:
  name: "camera_node"
  namespace: "/sensors"
  version: "1.0.0"
  auto_start: true
exposes:
  topics:
    - "/camera/video_feed"
  services:
    - "/camera/enable"
"#;

        let builder = NodeConfigBuilder::from_yaml(yaml_content);
        let config = builder.build().unwrap();

        assert_eq!(config.node_config.name.as_str(), "camera_node");
        assert_eq!(config.exposes.topics.len(), 1);
        assert_eq!(config.exposes.topics[0], "/camera/video_feed");
        assert_eq!(config.exposes.services.len(), 1);
        assert_eq!(config.exposes.services[0], "/camera/enable");
    }

    #[test]
    fn test_parse_root_node_yaml() {
        let yaml_content = r#"
node_config:
  is_root: true
  name: "my_robot_1"
  namespace: "/"
  version: "0.1.0"
  respawn: true
  respawn_delay: 1.0
"#;

        let builder = NodeConfigBuilder::from_yaml(yaml_content);
        let config = builder.build().unwrap();

        assert_eq!(config.node_config.name.as_str(), "my_robot_1");
        assert!(config.node_config.respawn);
    }

    #[test]
    fn test_parse_peppy_yaml_config() {
        // Write a test configuration based on the example
        let yaml_content = r#"
node_config:
  name: "root_node"
  namespace: "/"
  version: "0.1.0"
  auto_start: true
  respawn: false
  respawn_delay: 2.0

node_parameters:

exposes:
  topics: []
  services: []
  actions: []

resources:
  max_memory_mb: 512
  cpu_affinity: []

logging:
  min_level: "info"
  file_path: "/var/log/peppy/peppy_root.log"
  max_file_size_mb: 100
  format: "text"
"#;

        // Parse the configuration
        let config = NodeConfigBuilder::from_yaml(yaml_content).build().unwrap();

        // Verify the parsed values
        assert_eq!(config.node_config.name.as_str(), "root_node");
        assert_eq!(config.node_config.namespace.as_str(), "/");
        assert_eq!(config.node_config.version, "0.1.0");
        assert!(config.node_config.auto_start);
        assert!(!config.node_config.respawn);
        assert_eq!(config.node_config.respawn_delay, 2.0);
        assert_eq!(config.resources.max_memory_mb, 512);
        assert_eq!(config.logging.min_level, "info");
        assert_eq!(config.logging.file_path, "/var/log/peppy/peppy_root.log");
        assert_eq!(config.logging.max_file_size_mb, 100);
        assert_eq!(config.logging.format, "text");
    }
}
