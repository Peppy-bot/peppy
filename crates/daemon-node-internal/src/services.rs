mod info;
mod node;
mod ping;
mod stack;

pub use node::FORBIDDEN_ENV_KEYS;

use crate::Result;
use config::{
    AnyType, NodeArguments,
    consts::PeppyDirs,
    node::{Build, Manifest, Name, NodeConfig, PeppygenLanguage},
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

const DAEMON_NODE_TAG: &str = "daemon-node";

/// Clears instance directories from previous runs.
fn clear_instances_dir(peppy_dirs: &PeppyDirs) {
    let inst_dir = peppy_dirs.instances_dir();
    if inst_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&inst_dir)
    {
        tracing::warn!("Failed to clear instances directory: {}", e);
    }
}

pub struct DaemonNodeArguments {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
}

impl From<DaemonNodeArguments> for NodeArguments {
    fn from(args: DaemonNodeArguments) -> Self {
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

pub struct DaemonNode {
    node_stack: Arc<NodeStack>,
    node_config: NodeConfig,
    instance_id: Name,
    messenger: MessengerHandle,
    peppy_dirs: PeppyDirs,
    start_time: Instant,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
}

impl DaemonNode {
    pub fn new<P: Into<PathBuf>>(
        messenger: Arc<Mutex<Messenger>>,
        node_name: Option<&str>,
        node_arguments: DaemonNodeArguments,
        root_dir: P,
        peppy_dirs: PeppyDirs,
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
                tag: DAEMON_NODE_TAG.to_string(),
                language: PeppygenLanguage::Rust,
                labels: None,
            },
            build: Some(Build {
                add_cmd: None,
                start_cmd: vec![],
            }),
            container: None,
            parameters: node_arguments.into(),
            interfaces: Default::default(),
        };

        let messenger = MessengerHandle::from_shared(messenger);
        let instance_id = Name::new(get_random(rng())).unwrap();
        // The daemon node is the root of the node stack
        let node_stack = NodeStack::new(node_config.clone(), None, root_dir);

        Self {
            node_stack: Arc::new(node_stack),
            node_config,
            instance_id,
            messenger,
            peppy_dirs,
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
        clear_instances_dir(&self.peppy_dirs);

        let daemon_node_name = self.node_name(); // The daemon node binds to itself as the daemon scope
        info!(
            "Starting the daemon node with name {} and instance_id {}...",
            self.node_name(),
            self.instance_id(),
        );
        let handles = vec![
            ping::listen_for_ping(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            info::listen_for_info(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.start_time,
            )
            .await?,
            stack::listen_for_stack_launch(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .await?,
            stack::listen_for_stack_list(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            stack::listen_for_stack_reset(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_add(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .await?,
            node::listen_for_node_info(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.node_startup_timeout,
            )
            .await?,
            node::listen_for_node_remove(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_start(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                node::NodeStartServiceConfig {
                    node_startup_timeout: self.node_startup_timeout,
                    node_start_health_timeout: self.node_start_health_timeout,
                    peppy_dirs: self.peppy_dirs.clone(),
                },
            )
            .await?,
            node::listen_for_node_stop(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_init(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .await?,
            node::listen_for_node_sync(
                &self.messenger,
                daemon_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
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

        info!("Shutting down daemon node...");
        Ok(())
    }
}
