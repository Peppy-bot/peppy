use crate::Result;
use crate::commands::{listen_for_ping, listen_for_status};
use config::{
    node::{Manifest, Name, NodeConfig},
    peppy_config::CURRENT_SCHEMA_VERSION,
};
use names_generator2::get_random;
use peppylib::MessengerHandle;
use pmi::Messenger;
use rand::rng;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::info;

const MASTER_NODE_TAG: &str = "internal";

pub struct MasterNode {
    node_config: NodeConfig,
    instance_id: Name,
    messenger: MessengerHandle,
}

impl MasterNode {
    pub fn new(messenger: Arc<Mutex<Messenger>>, node_name: Option<&str>) -> Self {
        let manifest_name = match node_name {
            Some(name) => Name::new(name).unwrap(),
            None => Name::new(get_random(rng())).unwrap(),
        };

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
        let instance_id = Name::new(get_random(rng())).unwrap();

        Self {
            node_config,
            instance_id,
            messenger,
        }
    }

    pub fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    pub async fn start(&self) -> Result<()> {
        let node_name = self.node_config.manifest.name.as_str();
        let master_node_node = "*"; // There is no other node higher in the hierarchy
        let instance_id = self.instance_id.as_str();
        info!(
            "Starting the master node with name {} and instance_id {}...",
            node_name, instance_id
        );
        let handles = vec![
            listen_for_ping(&self.messenger, node_name, master_node_node, instance_id).await?,
            listen_for_status(&self.messenger, node_name, master_node_node, instance_id).await?,
        ];

        // Wait for all service handlers
        futures::future::try_join_all(handles)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        info!("Shutting down master node...");
        Ok(())
    }
}
