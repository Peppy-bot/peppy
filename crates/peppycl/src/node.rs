use config::NodeConfig;

use crate::error::Result;

pub async fn setup_node() -> Result<()> {
    // configuration_file is taken from the root path where this node is run
    let configuration_file = std::env::current_dir()?.join(config::consts::PEPPY_CONFIG_FILE);
    let cfg = config::NodeConfigParser::from_path(&configuration_file)?;
    setup_node_from_config(cfg).await
}

pub async fn setup_node_from_config(node_config: NodeConfig) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {}
