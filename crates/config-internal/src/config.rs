mod builder;
mod create;
mod parse;

pub use builder::{ConfigTemplateType, Validator, YamlConfigBuilder};
pub use create::{create_peppy_node_config, init_root_node};
pub use parse::parse_yaml_config;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeConfig {
    pub node_config: NodeInfo,
    pub node_parameters: NodeParameters,
    pub exposes: Exposes,
    pub resources: Resources,
    pub logging: Logging,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeInfo {
    pub name: String,
    pub namespace: String,
    pub version: String,
    pub auto_start: bool,
    pub respawn: bool,
    pub respawn_delay: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct NodeParameters {
    // Dynamic parameters can be added here
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Exposes {
    pub topics: Vec<String>,
    pub services: Vec<String>,
    pub actions: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Resources {
    pub max_memory_mb: u32,
    pub cpu_affinity: Vec<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Logging {
    pub min_level: String,
    pub file_path: String,
    pub max_file_size_mb: u32,
    pub format: String,
}
