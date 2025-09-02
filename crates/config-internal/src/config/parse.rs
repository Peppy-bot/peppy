use super::builder::NodeConfigBuilder;
use super::types::NodeConfig;
use crate::error::Result;
use std::path::PathBuf;

/// Parses a YAML configuration file into a NodeConfig using the builder pattern
pub fn parse_yaml_config(config_file: PathBuf) -> Result<NodeConfig> {
    NodeConfigBuilder::from_yaml(config_file)?.build_config()
}

pub fn get_nodes_startup_order() {
    todo!(
        "This function should return the order in which the nodes should be started, those nodes should also have a `need_repo_pull` field"
    )
}
