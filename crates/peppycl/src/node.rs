use crate::error::Error;
use crate::error::Result;
use config::NodeConfig;
use pmi::{Message, MessagingEngineContext, Messenger, MessengerBackend, PublisherQoS};
use std::path::PathBuf;

/// Sets up a node. If `config_file` is not provided, use current directory `peppy.json5`.
pub async fn setup_node(config_file: Option<PathBuf>) -> Result<()> {
    // Use provided config file, otherwise default to ./peppy.json5
    let configuration_file = match config_file {
        Some(path) => path,
        None => std::env::current_dir()?.join(config::consts::PEPPY_CONFIG_FILE),
    };

    let cfg = config::NodeConfigParser::from_path(&configuration_file)?;
    setup_node_from_config(cfg).await
}

pub async fn setup_node_from_config(node_config: NodeConfig) -> Result<()> {
    // 1. Try to reach out for the messaging
    let context = MessagingEngineContext {
        engine: "zenoh".to_string(),
        config_path: None,
    };
    let mut messenger = Messenger::new(context).map_err(Error::PeppyMessagingInterface)?;

    // 2. Emit node status if defined in config
    //if node_config.exposes
    let payload = "test".as_bytes();
    messenger
        .publish(Message::new("", payload), PublisherQoS::Standard)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {}
