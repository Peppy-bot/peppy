use crate::error::Error;
use crate::error::Result;
use config::NodeConfig;
use pmi::{MessagingEngineContext, Messenger, MessengerBackend};
use std::path::PathBuf;

// TODO: We actually need an `on_node_start` and `on_node_initialize(config)` instead of letting the
// user start the node with `main()`. This way the node can be started from
// a thread (default) or fork (Process::Command) by the `peppy` program.

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

pub async fn setup_node_from_config(_node_config: NodeConfig) -> Result<()> {
    // In tests, only use the mock engine to avoid external binaries.
    // Outside tests, prefer zenoh and fall back to mock when unavailable.
    #[cfg(test)]
    let engines = ["mock"];
    #[cfg(not(test))]
    let engines = ["zenoh", "mock"]; // ordered preference in non-test builds

    let mut last_err: Option<pmi::PeppyMessagingInterfaceError> = None;
    for engine in engines {
        let ctx = MessagingEngineContext {
            engine: engine.to_string(),
            zenohd_config_path: None,
        };

        match Messenger::new(ctx) {
            Ok(mut messenger) => match messenger.start_session().await {
                Ok(()) => {
                    // TODO: parse `_node_config` and expose interfaces via peppygen
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
