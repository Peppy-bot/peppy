use crate::Result;
use crate::services::{
    listen_for_info, listen_for_launch_configuration, listen_for_node_add,
    listen_for_node_generate, listen_for_node_init, listen_for_node_list, listen_for_node_remove,
    listen_for_node_reset, listen_for_node_start, listen_for_node_stop, listen_for_ping,
};
use config::{
    AnyType, NodeArguments,
    node::{Manifest, Name, NodeConfig},
    peppy_config::CURRENT_SCHEMA_VERSION,
};
use names_generator2::get_random;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use pmi::Messenger;
use rand::rng;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::info;

const MASTER_NODE_TAG: &str = "internal";

pub struct MasterNodeArguments {
    pub node_start_health_timeout: Duration,
}

impl From<MasterNodeArguments> for NodeArguments {
    fn from(args: MasterNodeArguments) -> Self {
        let mut map = BTreeMap::new();
        map.insert(
            "node_start_health_timeout_ms".to_string(),
            AnyType::UInt(args.node_start_health_timeout.as_millis() as u64),
        );
        map
    }
}

pub struct MasterNode {
    node_stack: Arc<NodeStack>,
    node_config: NodeConfig,
    instance_id: Name,
    messenger: MessengerHandle,
    start_time: Instant,
    node_start_health_timeout: Duration,
}

impl MasterNode {
    pub fn new<P: Into<PathBuf>>(
        messenger: Arc<Mutex<Messenger>>,
        node_name: Option<&str>,
        node_arguments: MasterNodeArguments,
        root_dir: P,
    ) -> Self {
        let manifest_name = match node_name {
            Some(name) => Name::new(name).unwrap(),
            None => Name::new(get_random(rng())).unwrap(),
        };

        let node_start_health_timeout = node_arguments.node_start_health_timeout;

        let node_config = NodeConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            manifest: Manifest {
                name: manifest_name,
                tag: MASTER_NODE_TAG.to_string(),
                labels: None,
                launch_cmd: vec![],
            },
            parameters: node_arguments.into(),
            interfaces: Default::default(),
            resources: None,
            logging: None,
        };

        let messenger = MessengerHandle::from_shared(messenger);
        let instance_id = Name::new(get_random(rng())).unwrap();
        // The master node is the root of the node stack
        let node_stack = NodeStack::new(node_config.clone(), None, root_dir);

        Self {
            node_stack: Arc::new(node_stack),
            node_config,
            instance_id,
            messenger,
            start_time: Instant::now(),
            node_start_health_timeout,
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
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.node_start_health_timeout,
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
            listen_for_node_remove(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_reset(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_start(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.node_start_health_timeout,
            )
            .await?,
            listen_for_node_stop(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            listen_for_node_init(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            listen_for_node_generate(
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
