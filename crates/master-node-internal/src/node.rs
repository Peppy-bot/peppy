use crate::Result;
use config::{
    node::{Manifest, Name, NodeConfig, QoSProfile},
    peppy_config::CURRENT_SCHEMA_VERSION,
};
use names_generator2::get_random;
use peppylib::{MessengerHandle, ServiceMessenger};
use pmi::Messenger;
use rand::rng;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

const MASTER_NODE_TAG: &str = "internal";

pub struct MasterNode {
    node_config: NodeConfig,
    messenger: MessengerHandle,
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

        let messenger = MessengerHandle::from_shared(messenger);

        Self {
            node_config,
            messenger,
        }
    }

    pub fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    pub async fn start(&self) -> Result<()> {
        let node_name = self.node_config.manifest.name.as_str();
        info!("Starting the master node as {}...", node_name);

        // TODO: Finish exposing the service
        // let mut subscription = ServiceMessenger::subscribe(
        //     &self.messenger,
        //     config::consts::MASTER_NODE_NAME,
        //     config::consts::MASTER_NODE_TOPIC_NAME,
        //     None,
        //     QoSProfile::Critical,
        // )
        // .await?;

        // loop {
        //     match subscription.on_next_message().await {
        //         Some(message) => {
        //             let payload = String::from_utf8_lossy(message.payload());
        //             let command = payload.trim();

        //             match command {
        //                 "ping" => info!("Received 'ping' command over {}", message.identifier()),
        //                 "status" => info!("Would respond with status for {}", message.identifier()),
        //                 "shutdown" => info!("Received 'shutdown' command (toy example)"),
        //                 other => info!("Received unhandled command '{}'", other),
        //             }
        //         }
        //         None => {
        //             info!("Command subscription closed; no longer listening for messages");
        //             break;
        //         }
        //     }
        // }

        info!("Shutting down master node...");
        Ok(())
    }
}
