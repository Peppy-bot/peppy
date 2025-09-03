use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl Default for NodeConfig {
    fn default() -> Self {
        Self {
            node_config: NodeInfo::default(),
            node_parameters: NodeParameters::default(),
            exposes: Exposes::default(),
            resources: Resources::default(),
            logging: Logging::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub namespace: String,
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
            name: String::new(),
            namespace: "/".to_string(),
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
#[derive(Debug, Clone)]
pub enum ConfigTemplateType {
    RootNode,
    SimpleNode,
    FullNode,
}

impl Default for ConfigTemplateType {
    fn default() -> Self {
        ConfigTemplateType::SimpleNode
    }
}
