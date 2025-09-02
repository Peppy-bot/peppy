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
    /// Creates a builder from an existing YAML file
    pub fn from_yaml(path: impl AsRef<Path>) -> Result<Self> {
        let content = fs::read_to_string(path.as_ref())?;
        Self::from_yaml_str(&content)
    }

    /// Creates a builder from a YAML string
    pub fn from_yaml_str(content: &str) -> Result<Self> {
        let docs = Yaml::load_from_str(content)
            .map_err(|e| Error::ConfigParse(format!("Failed to parse YAML: {}", e)))?;

        if docs.is_empty() {
            return Err(Error::ConfigParse("Empty YAML document".to_string()));
        }

        let doc = &docs[0];
        let mut builder = Self::default();

        // Parse sections into builder.config
        Self::parse_node_config_section(doc, &mut builder)?;
        Self::parse_exposes_section(doc, &mut builder)?;
        Self::parse_resources_section(doc, &mut builder)?;
        Self::parse_logging_section(doc, &mut builder)?;

        Ok(builder)
    }

    fn parse_node_config_section(doc: &Yaml, builder: &mut NodeConfigBuilder) -> Result<()> {
        // Parse node_config section
        if let Some(node_config) = doc.as_mapping_get("node_config") {
            // Required/optional string fields
            if let Some(n) = Self::get_str(node_config, "name")? {
                builder.config.node_config.name = n.to_string();
            }

            if let Some(ns) = Self::get_str(node_config, "namespace")? {
                builder.config.node_config.namespace = ns.to_string();
            }

            // Optional fields
            if let Some(v) = Self::get_str(node_config, "version")? {
                builder.config.node_config.version = v.to_string();
            }

            if let Some(v) = Self::get_bool(node_config, "auto_start")? {
                builder.config.node_config.auto_start = v;
            }

            if let Some(v) = Self::get_bool(node_config, "respawn")? {
                builder.config.node_config.respawn = v;
            }

            if let Some(v) = Self::get_f64(node_config, "respawn_delay")? {
                builder.config.node_config.respawn_delay = v;
            }

            if let Some(is_root) = node_config.as_mapping_get("is_root") {
                if let Some(true) = is_root.as_bool() {
                    builder.template_type = ConfigTemplateType::RootNode;
                }
            }
        }
        Ok(())
    }

    fn parse_exposes_section(doc: &Yaml, builder: &mut NodeConfigBuilder) -> Result<()> {
        // Parse exposes section
        if let Some(exposes) = doc.as_mapping_get("exposes") {
            if let Some(topics) = exposes.as_mapping_get("topics") {
                if let Some(vec) = topics.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'topics[{}]'",
                                i
                            )));
                        }
                    }
                    builder.config.exposes.topics = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'topics'".to_string(),
                    ));
                }
            }

            if let Some(services) = exposes.as_mapping_get("services") {
                if let Some(vec) = services.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'services[{}]'",
                                i
                            )));
                        }
                    }
                    builder.config.exposes.services = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'services'".to_string(),
                    ));
                }
            }

            if let Some(actions) = exposes.as_mapping_get("actions") {
                if let Some(vec) = actions.as_vec() {
                    let mut out = Vec::with_capacity(vec.len());
                    for (i, item) in vec.iter().enumerate() {
                        if let Some(s) = item.as_str() {
                            out.push(s.to_string());
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected string for key 'actions[{}]'",
                                i
                            )));
                        }
                    }
                    builder.config.exposes.actions = out;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'actions'".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_resources_section(doc: &Yaml, builder: &mut NodeConfigBuilder) -> Result<()> {
        if let Some(resources) = doc.as_mapping_get("resources") {
            if let Some(v) = Self::get_u32(resources, "max_memory_mb")? {
                builder.config.resources.max_memory_mb = v;
            }

            if let Some(cpu) = resources.as_mapping_get("cpu_affinity") {
                if let Some(cpu_vec) = cpu.as_vec() {
                    let mut parsed = Vec::with_capacity(cpu_vec.len());
                    for (i, item) in cpu_vec.iter().enumerate() {
                        if let Some(v) = Self::yaml_to_u32(item) {
                            parsed.push(v);
                        } else {
                            return Err(Error::ConfigParse(format!(
                                "Expected number for key 'cpu_affinity[{}]'",
                                i
                            )));
                        }
                    }
                    builder.config.resources.cpu_affinity = parsed;
                } else {
                    return Err(Error::ConfigParse(
                        "Expected array for key 'cpu_affinity'".to_string(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn parse_logging_section(doc: &Yaml, builder: &mut NodeConfigBuilder) -> Result<()> {
        if let Some(logging) = doc.as_mapping_get("logging") {
            if let Some(v) = Self::get_str(logging, "min_level")? {
                builder.config.logging.min_level = v.to_string();
            }

            if let Some(v) = Self::get_str(logging, "file_path")? {
                builder.config.logging.file_path = v.to_string();
            }

            if let Some(v) = Self::get_u32(logging, "max_file_size_mb")? {
                builder.config.logging.max_file_size_mb = v;
            }

            if let Some(v) = Self::get_str(logging, "format")? {
                builder.config.logging.format = v.to_string();
            }
        }
        Ok(())
    }

    fn get_str<'a>(map: &'a Yaml, key: &str) -> Result<Option<&'a str>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(s) = value.as_str() {
                return Ok(Some(s));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected string for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_u32(map: &Yaml, key: &str) -> Result<Option<u32>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(v) = Self::yaml_to_u32(value) {
                return Ok(Some(v));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected number for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_bool(map: &Yaml, key: &str) -> Result<Option<bool>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(b) = value.as_bool() {
                return Ok(Some(b));
            } else {
                return Err(Error::ConfigParse(format!(
                    "Expected boolean for key '{}'",
                    key
                )));
            }
        }
        Ok(None)
    }

    fn get_f64(map: &Yaml, key: &str) -> Result<Option<f64>> {
        if let Some(value) = map.as_mapping_get(key) {
            if let Some(v) = value.as_floating_point() {
                return Ok(Some(v));
            }
            if let Some(s) = value.as_str() {
                if let Ok(v) = s.parse::<f64>() {
                    return Ok(Some(v));
                }
            }
            return Err(Error::ConfigParse(format!(
                "Expected number for key '{}'",
                key
            )));
        }
        Ok(None)
    }

    fn yaml_to_u32(node: &Yaml) -> Option<u32> {
        // Accept both float-like and integer-like values
        if let Some(v) = node.as_floating_point() {
            return Some(v.max(0.0) as u32);
        }
        if let Some(s) = node.as_str() {
            if let Ok(v) = s.parse::<f64>() {
                return Some(v.max(0.0) as u32);
            }
        }
        // Last-resort: parse from debug representation (handles Integer(100), Real(2.0))
        let dbg = format!("{:?}", node);
        let num: String = dbg
            .chars()
            .filter(|c| c.is_ascii_digit() || *c == '.' || *c == '-')
            .collect();
        if num.is_empty() {
            return None;
        }
        num.parse::<f64>().ok().map(|v| v.max(0.0) as u32)
    }

    /// Builds the configuration returning the NodeConfig struct
    pub fn build_config(self) -> Result<NodeConfig> {
        Ok(self.config)
    }

    /// Creates a builder for root node configuration
    pub fn root_node(name: &str) -> Self {
        let mut builder = Self::default();
        builder.config.node_config.name = name.into();
        builder.config.node_config.respawn = true;
        builder.config.node_config.respawn_delay = 1.0;
        builder.config.logging.file_path =
            format!(".pixi/envs/default/var/log/peppy/{}_root.log", name);
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
        builder.config.logging.file_path =
            format!(".pixi/envs/default/var/log/peppy/{}_node.log", name);
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

        let builder = NodeConfigBuilder::from_yaml_str(yaml_content).unwrap();
        let config = builder.build_config().unwrap();

        assert_eq!(config.node_config.name, "camera_node");
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

        let builder = NodeConfigBuilder::from_yaml_str(yaml_content).unwrap();
        let config = builder.build_config().unwrap();

        assert_eq!(config.node_config.name, "my_robot_1");
        assert_eq!(config.node_config.respawn, true);
    }
}
