use crate::Result;
use crate::context::MasterContext;
use crate::services::{
    listen_for_add_node, listen_for_info, listen_for_launch_configuration, listen_for_ping,
};
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
    context: Arc<MasterContext>,
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

        let context = Arc::new(MasterContext::default());

        Self {
            context: Arc::clone(&context),
            node_config,
            instance_id,
            messenger,
        }
    }

    pub fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    pub fn node_name(&self) -> &str {
        self.node_config.manifest.name.as_str()
    }

    pub fn instance_id(&self) -> &str {
        self.instance_id.as_str()
    }

    pub async fn start(&self) -> Result<()> {
        let master_node_name = self.node_name(); // The master node binds to itself as the master scope
        info!(
            "Starting the master node with name {} and instance_id {}...",
            self.node_name(),
            self.instance_id(),
        );
        let handles = vec![
            listen_for_ping(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            listen_for_info(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                &self.context,
            )
            .await?,
            listen_for_launch_configuration(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                &self.context,
            )
            .await?,
            listen_for_add_node(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
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
