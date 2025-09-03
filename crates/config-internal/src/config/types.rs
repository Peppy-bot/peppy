use crate::error::Error;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeConfig {
    pub node_config: NodeInfo,
    #[serde(default)]
    pub node_parameters: NodeParameters,
    #[serde(default)]
    pub exposes: Exposes,
    #[serde(default)]
    pub resources: Resources,
    #[serde(default)]
    pub logging: Logging,
}

/// Validated node name. Lowercase letters, digits, '_' and '-' only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Name(String);

impl Name {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, Error> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-'
    }
}

impl TryFrom<String> for Name {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(Error::InvalidName("Name cannot be empty".to_string()));
        }
        if value.chars().all(Name::is_valid_char) {
            return Ok(Name(value));
        }
        Err(Error::InvalidName(value))
    }
}

impl From<Name> for String {
    fn from(v: Name) -> Self {
        v.0
    }
}

/// Validated namespace. Same as Name but allows '/'.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

impl Namespace {
    pub fn new<S: Into<String>>(s: S) -> Result<Self, Error> {
        Self::try_from(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    fn is_valid_char(c: char) -> bool {
        c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-' || c == '/'
    }
}

impl TryFrom<String> for Namespace {
    type Error = Error;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if value.is_empty() {
            return Err(Error::InvalidNamespace(
                "Namespace cannot be empty".to_string(),
            ));
        }
        if value.chars().all(Namespace::is_valid_char) {
            return Ok(Namespace(value));
        }
        Err(Error::InvalidNamespace(value))
    }
}

impl From<Namespace> for String {
    fn from(v: Namespace) -> Self {
        v.0
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: Name,
    pub namespace: Namespace,
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_false")]
    pub auto_start: bool,
    #[serde(default = "default_false")]
    pub respawn: bool,
    #[serde(default = "default_respawn_delay")]
    pub respawn_delay: f64,
}

impl Default for NodeInfo {
    fn default() -> Self {
        Self {
            // Default values must be non-empty to comply with validation
            name: Name::new("node").expect("default name is valid"),
            namespace: Namespace::new("/").expect("default namespace is valid"),
            version: "0.1.0".to_string(),
            auto_start: false,
            respawn: false,
            respawn_delay: 1.0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeParameters {
    // Dynamic parameters can be added here
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Exposes {
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub services: Vec<String>,
    #[serde(default)]
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    #[serde(default = "default_max_memory_mb")]
    pub max_memory_mb: u32,
    #[serde(default)]
    pub cpu_affinity: Vec<u32>,
}

impl Default for Resources {
    fn default() -> Self {
        Self {
            max_memory_mb: default_max_memory_mb(),
            cpu_affinity: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logging {
    #[serde(default = "default_log_level")]
    pub min_level: String,
    #[serde(default = "default_log_file_path")]
    pub file_path: String,
    #[serde(default = "default_max_file_size_mb")]
    pub max_file_size_mb: u32,
    #[serde(default = "default_log_format")]
    pub format: String,
}

impl Default for Logging {
    fn default() -> Self {
        Self {
            min_level: default_log_level(),
            file_path: default_log_file_path(),
            max_file_size_mb: default_max_file_size_mb(),
            format: default_log_format(),
        }
    }
}

// Default value functions
fn default_version() -> String {
    "0.1.0".to_string()
}

fn default_false() -> bool {
    false
}

fn default_respawn_delay() -> f64 {
    1.0
}

fn default_max_memory_mb() -> u32 {
    512
}

fn default_log_level() -> String {
    "info".to_string()
}

fn default_log_file_path() -> String {
    "".to_string()
}

fn default_max_file_size_mb() -> u32 {
    10
}

fn default_log_format() -> String {
    "text".to_string()
}

/// Supported template types
#[derive(Debug, Clone, Default)]
pub enum ConfigTemplateType {
    RootNode,
    #[default]
    SimpleNode,
    FullNode,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_validation() {
        assert!(Name::new("node").is_ok());
        assert!(Name::new("my_node-1").is_ok());

        assert!(Name::new("").is_err()); // empty not permitted
        assert!(Name::new("Node").is_err()); // capital
        assert!(Name::new("node/").is_err()); // slash not allowed
        assert!(Name::new("node@!").is_err()); // specials not allowed
    }

    #[test]
    fn namespace_validation() {
        assert!(Namespace::new("/").is_ok());
        assert!(Namespace::new("/robot").is_ok());
        assert!(Namespace::new("/robot/camera_v1").is_ok());

        assert!(Namespace::new("").is_err()); // empty not permitted
        assert!(Namespace::new("/Robot").is_err()); // capital
        assert!(Namespace::new("/robot$cam").is_err()); // special
    }
}
