use config::{
    node::{Manifest, Name, NodeConfig},
    peppy_config::CURRENT_SCHEMA_VERSION,
};
use names_generator2::get_random;
use rand::rng;
use std::sync::Arc;
use tracing::warn;

use crate::{Error, Result};
use pmi::{Messenger, MessengerBackend, SubscriberQoS};
use tokio::sync::Mutex;
use tracing::info;

const MASTER_NODE_TAG: &str = "internal";

pub struct MasterNode {
    node_config: NodeConfig,
    messenger: Arc<Mutex<Messenger>>,
}

impl MasterNode {
    pub fn new(messenger: Arc<Mutex<Messenger>>) -> Self {
        let manifest_name = Name::new(get_random(rng())).unwrap();

        let node_config = NodeConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            manifest: Manifest {
                name: manifest_name,
                tag: MASTER_NODE_TAG.to_string(),
                labels: None,
                launch_cmd: None,
            },
            parameters: Default::default(),
            interfaces: Default::default(),
            resources: None,
            logging: None,
        };

        Self {
            node_config,
            messenger,
        }
    }

    pub fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    pub async fn start(&self) -> Result<()> {
        info!("Starting commands listener...");

        let mut subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .subscribe(
                    config::consts::PEPPYD_COMMANDS_TOPIC,
                    SubscriberQoS::Standard,
                )
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let shutdown_signal = tokio::signal::ctrl_c();
        tokio::pin!(shutdown_signal);

        loop {
            tokio::select! {
                ctrl_c_result = &mut shutdown_signal => {
                    ctrl_c_result.map_err(Error::from)?;
                    break;
                }
                maybe_message = subscription.on_next_message() => {
                    match maybe_message {
                        Some(message) => {
                            let payload = String::from_utf8_lossy(message.payload.as_ref());
                            let command = payload.trim();

                            match command {
                                "ping" => info!("Received 'ping' command over {}", message.topic),
                                "status" => info!("Would respond with status for {}", message.topic),
                                "shutdown" => info!("Received 'shutdown' command (toy example)"),
                                other => info!("Received unhandled command '{}'", other),
                            }
                        }
                        None => {
                            info!("Command subscription closed; no longer listening for messages");
                            break;
                        }
                    }
                }
            }
        }

        info!("Shutting down commands listener...");
        Ok(())
    }
}
