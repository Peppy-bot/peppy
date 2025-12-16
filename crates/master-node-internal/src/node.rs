use crate::Result;
use crate::services::{
    listen_for_info, listen_for_launch_configuration, listen_for_node_add, listen_for_node_list,
    listen_for_node_sync, listen_for_ping,
};
use config::{
    node::{Manifest, Name, NodeConfig},
    peppy_config::CURRENT_SCHEMA_VERSION,
};
use names_generator2::get_random;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use pmi::Messenger;
use rand::rng;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Mutex;
use tracing::info;

const MASTER_NODE_TAG: &str = "internal";
pub const LAUNCH_CONFIGURATION_SERVICE: &str = "launch_configuration";

pub struct MasterNode {
    node_stack: Arc<NodeStack>,
    node_config: NodeConfig,
    instance_id: Name,
    messenger: MessengerHandle,
    start_time: Instant,
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
        // The master node is the root of the node stack
        let node_stack = NodeStack::new(node_config.clone(), None);

        Self {
            node_stack: Arc::new(node_stack),
            node_config,
            instance_id,
            messenger,
            start_time: Instant::now(),
        }
    }

    pub fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    pub fn set_node_stack(&mut self, node_stack: NodeStack) {
        self.node_stack = Arc::new(node_stack);
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
                Arc::clone(&self.node_stack),
                self.start_time,
            )
            .await?,
            listen_for_launch_configuration(
                &self.messenger,
                LAUNCH_CONFIGURATION_SERVICE,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_list(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_add(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_sync(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
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
