use crate::error::Error;
use crate::error::Result;
use config::node::{NodeConfig, NodeConfigParser};
#[cfg(feature = "zenoh")]
use pmi::ZenohAdapter;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::path::PathBuf;

// TODO: We actually need an `on_node_start` and `on_node_initialize(config)` instead of letting the
// user start the node with `main()`

/// Sets up a node. If `config_file` is not provided, use current directory `peppy.json5`.
pub async fn setup_node(config_file: Option<PathBuf>) -> Result<()> {
    // Use provided config file, otherwise default to ./peppy.json5
    let configuration_file = match config_file {
        Some(path) => path,
        None => std::env::current_dir()?.join(config::consts::PEPPY_NODE_CONFIG_FILE),
    };

    let cfg = NodeConfigParser::from_path(&configuration_file)?;
    setup_node_from_config(cfg).await
}

pub async fn setup_node_from_config(node_config: NodeConfig) -> Result<()> {
    // In tests, only use the mock engine to avoid external binaries.
    // Outside tests, prefer zenoh and fall back to mock when unavailable.
    #[cfg(test)]
    let engines: &[&str] = &["mock"];
    #[cfg(all(not(test), feature = "zenoh"))]
    let engines: &[&str] = &["zenoh", "mock"]; // ordered preference in non-test builds
    #[cfg(all(not(test), not(feature = "zenoh")))]
    let engines: &[&str] = &["mock"];

    tracing::info!(
        node.name = node_config.manifest.name.as_str(),
        node.tag = %node_config.manifest.tag,
        "initializing peppy node from configuration"
    );

    let mut last_err: Option<pmi::PeppyMessagingInterfaceError> = None;
    for engine in engines {
        match build_messenger(engine) {
            Ok(mut messenger) => match messenger.start_session().await {
                Ok(()) => {
                    // TODO: parse `node_config` and expose interfaces via peppygen
                    return Ok(());
                }
                Err(e) => last_err = Some(e),
            },
            Err(e) => last_err = Some(e),
        }
    }

    Err(Error::PeppyMessagingInterface(last_err.unwrap_or(
        pmi::PeppyMessagingInterfaceError::UnsupportedEngine,
    )))
}

#[cfg(test)]
mod tests {}

fn build_messenger(
    engine: &str,
) -> core::result::Result<Messenger, pmi::PeppyMessagingInterfaceError> {
    match engine {
        #[cfg(feature = "zenoh")]
        "zenoh" => {
            let zenoh_config_path = std::env::var_os("ZENOH_CONFIG")
                .map(PathBuf::from)
                .ok_or(pmi::PeppyMessagingInterfaceError::RouterConfigurationNotFound)?;
            let adapter = ZenohAdapter::from_zenohd_config(&zenoh_config_path)?;
            Ok(Messenger::new(MessengerAdapter::Zenoh(adapter)))
        }
        "mock" => Ok(Messenger::new(MessengerAdapter::Mock(
            MockAdapter::default(),
        ))),
        _ => Err(pmi::PeppyMessagingInterfaceError::UnsupportedEngine),
    }
}
