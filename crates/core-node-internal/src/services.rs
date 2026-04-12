mod action_loop;
mod info;
mod node;
mod ping;
mod repo;
mod stack;

pub use node::FORBIDDEN_ENV_KEYS;

use crate::Result;
use config::{
    AnyType,
    consts::PeppyDirs,
    launcher::CURRENT_SCHEMA_VERSION,
    node::{Execution, Manifest, Name, NodeConfig, PeppygenLanguage},
};
use names_generator2::get_random;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use pmi::Messenger;
use rand::SeedableRng;
use rand::rng;
use rand::rngs::StdRng;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, oneshot};
use tracing::info;

const CORE_NODE_TAG: &str = match option_env!("PEPPY_GIT_TAG") {
    Some(tag) => tag,
    None => "dev",
};

#[cfg(test)]
mod tests;

/// Clears instance directories from previous runs.
fn clear_instances_dir(peppy_dirs: &PeppyDirs) {
    let inst_dir = peppy_dirs.instances_dir();
    if inst_dir.exists()
        && let Err(e) = std::fs::remove_dir_all(&inst_dir)
    {
        tracing::warn!("Failed to clear instances directory: {}", e);
    }
}

pub struct CoreNodeArguments {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
    pub health_monitor_interval: Duration,
    pub health_monitor_timeout: Duration,
    pub health_monitor_max_failures: u32,
}

impl CoreNodeArguments {
    fn into_parameters(self) -> BTreeMap<String, AnyType> {
        let mut params = BTreeMap::new();
        params.insert(
            "node_startup_timeout_ms".to_string(),
            AnyType::UInt(self.node_startup_timeout.as_millis() as u64),
        );
        params.insert(
            "node_start_health_timeout_ms".to_string(),
            AnyType::UInt(self.node_start_health_timeout.as_millis() as u64),
        );
        params
    }
}

pub struct CoreNode {
    node_stack: Arc<NodeStack>,
    node_config: NodeConfig,
    instance_id: Name,
    messenger: MessengerHandle,
    peppy_dirs: PeppyDirs,
    start_time: Instant,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    health_monitor_interval: Duration,
    health_monitor_timeout: Duration,
    health_monitor_max_failures: u32,
}

/// Pre-flight checks that run once at daemon startup. Exits with a
/// user-friendly message if any check fails (no panic backtrace).
fn perform_runtime_checks() {
    // Apptainer user namespaces: on Ubuntu 24.04+ an AppArmor profile is
    // required to allow unprivileged user namespace creation.
    #[cfg(target_os = "linux")]
    if let Err(e) = containers::Apptainer::new() {
        eprintln!("Apptainer pre-flight check failed:\n\n{e}");
        std::process::exit(1);
    }
}

impl CoreNode {
    pub fn new<P: Into<PathBuf>>(
        messenger: Arc<Mutex<Messenger>>,
        node_name: Option<&str>,
        node_arguments: CoreNodeArguments,
        root_dir: P,
        peppy_dirs: PeppyDirs,
    ) -> Self {
        let manifest_name = match node_name {
            Some(name) => Name::new(name).unwrap(),
            None => {
                let seed_source = machine_uid::get()
                    .map_err(|e| {
                        tracing::warn!("machine_uid::get() failed: {e}; falling back to hostname");
                    })
                    .ok()
                    .filter(|s| !s.is_empty())
                    .or_else(|| {
                        hostname::get().ok().and_then(|h| {
                            let s = h.to_string_lossy().into_owned();
                            if s.is_empty() { None } else { Some(s) }
                        })
                    });

                let generated = match seed_source {
                    Some(src) => {
                        // Hash so the published name does not reveal the UID/hostname.
                        let digest = Sha256::digest(src.as_bytes());
                        let seed: [u8; 32] = digest.into();
                        let mut seeded = StdRng::from_seed(seed);
                        get_random(&mut seeded)
                    }
                    None => {
                        tracing::warn!(
                            "machine UID and hostname unavailable; falling back to non-deterministic core node name"
                        );
                        get_random(&mut rng())
                    }
                };

                Name::new(format!("core-node-{generated}")).unwrap()
            }
        };

        let node_startup_timeout = node_arguments.node_startup_timeout;
        let node_start_health_timeout = node_arguments.node_start_health_timeout;
        let health_monitor_interval = node_arguments.health_monitor_interval;
        let health_monitor_timeout = node_arguments.health_monitor_timeout;
        let health_monitor_max_failures = node_arguments.health_monitor_max_failures;

        let node_config = NodeConfig {
            schema_version: CURRENT_SCHEMA_VERSION,
            manifest: Manifest {
                name: manifest_name,
                tag: CORE_NODE_TAG.to_string(),
                labels: None,
                variants: None,
                depends_on: None,
            },
            execution: Execution {
                language: PeppygenLanguage::Rust,
                parameters: node_arguments.into_parameters(),
                build_cmd: None,
                run_cmd: None,
                container: None,
            },
            interfaces: Default::default(),
        };

        perform_runtime_checks();

        let messenger = MessengerHandle::from_shared(messenger);
        let instance_id = Name::new(get_random(rng())).unwrap();
        // The core node is the root of the node stack
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
            health_monitor_interval,
            health_monitor_timeout,
            health_monitor_max_failures,
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

        let core_node_name = self.node_name(); // The core node binds to itself
        info!(
            "Starting the core node with name {} and instance_id {}...",
            self.node_name(),
            self.instance_id(),
        );
        let handles = vec![
            ping::listen_for_ping(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .await?,
            info::listen_for_info(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.start_time,
            )
            .await?,
            stack::listen_for_stack_launch(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
                stack::StackLaunchTimeouts {
                    node_startup: self.node_startup_timeout,
                    node_start_health: self.node_start_health_timeout,
                    health_monitor_interval: self.health_monitor_interval,
                    health_monitor_timeout: self.health_monitor_timeout,
                    health_monitor_max_failures: self.health_monitor_max_failures,
                },
            )
            .await?,
            stack::listen_for_stack_list(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            stack::listen_for_stack_reset(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_add(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .await?,
            node::listen_for_node_build(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .await?,
            node::listen_for_node_info(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
                self.node_startup_timeout,
            )
            .await?,
            node::listen_for_node_remove(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_run(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                node::NodeRunServiceConfig {
                    node_startup_timeout: self.node_startup_timeout,
                    node_start_health_timeout: self.node_start_health_timeout,
                    peppy_dirs: self.peppy_dirs.clone(),
                    health_monitor_interval: self.health_monitor_interval,
                    health_monitor_timeout: self.health_monitor_timeout,
                    health_monitor_max_failures: self.health_monitor_max_failures,
                },
            )
            .await?,
            node::listen_for_node_stop(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .await?,
            node::listen_for_node_init(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .await?,
            node::listen_for_node_sync(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .await?,
            repo::listen_for_repo_add(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .await?,
            repo::listen_for_repo_refresh(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
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

        info!("Shutting down core node...");
        Ok(())
    }
}
