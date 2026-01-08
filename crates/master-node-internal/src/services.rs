mod info;
mod node;
mod ping;
mod stack;

use crate::Result;
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
use tokio::sync::{Mutex, oneshot};
use tracing::info;

const MASTER_NODE_TAG: &str = "master-node";

pub struct MasterNodeArguments {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
}

impl From<MasterNodeArguments> for NodeArguments {
    fn from(args: MasterNodeArguments) -> Self {
        let mut map = BTreeMap::new();
        map.insert(
            "node_startup_timeout_ms".to_string(),
            AnyType::UInt(args.node_startup_timeout.as_millis() as u64),
        );
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
    node_startup_timeout: Duration,
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

        let node_startup_timeout = node_arguments.node_startup_timeout;
        let node_start_health_timeout = node_arguments.node_start_health_timeout;

        let node_config = NodeConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            manifest: Manifest {
                name: manifest_name,
                tag: MASTER_NODE_TAG.to_string(),
                labels: None,
                add_cmd: None,
                start_cmd: vec![],
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
            node_startup_timeout,
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
        self.start_with_ready(None).await
    }

    pub async fn start_with_ready(&self, ready: Option<oneshot::Sender<()>>) -> Result<()> {
        let master_node_name = self.node_name(); // The master node binds to itself as the master scope
        info!(
            "Starting the master node with name {} and instance_id {}...",
            self.node_name(),
            self.instance_id(),
        );
        let handles = vec![
            ping::listen_for_ping(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            info::listen_for_info(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.start_time,
            )
            .await?,
            stack::listen_for_stack_launch(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.node_startup_timeout,
                self.node_start_health_timeout,
            )
            .await?,
            stack::listen_for_stack_list(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            stack::listen_for_stack_reset(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_add(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_remove(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_start(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.node_startup_timeout,
                self.node_start_health_timeout,
            )
            .await?,
            node::listen_for_node_stop(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_init(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            node::listen_for_node_generate(
                &self.messenger,
                master_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
        ];

        if let Some(ready) = ready {
            let _ = ready.send(());
        }

        // Wait for all service handlers
        futures::future::try_join_all(handles)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        info!("Shutting down master node...");
        Ok(())
    }
}
