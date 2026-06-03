mod action_loop;
mod clock;
mod datastore;
mod info;
mod node;
mod ping;
pub(crate) mod repo;
mod stack;

use clock::{ClockSource, SimClockSource, WallClockSource};

pub use node::FORBIDDEN_ENV_KEYS;

use crate::Result;
use config::{
    DefaultValue, ParameterSpec,
    consts::PeppyDirs,
    node::{Execution, Manifest, Name, NodeConfig, PeppygenLanguage, TypeToken},
    schema::PeppySchema,
};
use futures::future::{BoxFuture, FutureExt, try_join_all};
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
use std::sync::atomic::AtomicU64;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
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
    pub clock_publish_interval: Duration,
    /// Daemon-wide default for the framework `use_sim_time` flag. Per-instance
    /// launcher overrides win over this; when an instance omits the override,
    /// the spawned node's `framework.use_sim_time` is set to this value.
    pub daemon_use_sim_time: bool,
}

impl CoreNodeArguments {
    /// Build the core node's parameter schema, baking the runtime values in
    /// as `$default` so the schema is self-contained.
    fn into_parameters(self) -> config::ParameterSchema {
        let mut params = BTreeMap::new();
        params.insert(
            "node_startup_timeout_ms".to_string(),
            ParameterSpec::Primitive {
                kind: TypeToken::U64,
                default: Some(DefaultValue::UInt(
                    self.node_startup_timeout.as_millis() as u64
                )),
            },
        );
        params.insert(
            "node_start_health_timeout_ms".to_string(),
            ParameterSpec::Primitive {
                kind: TypeToken::U64,
                default: Some(DefaultValue::UInt(
                    self.node_start_health_timeout.as_millis() as u64,
                )),
            },
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
    clock_publish_interval: Duration,
    daemon_use_sim_time: bool,
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
        let clock_publish_interval = node_arguments.clock_publish_interval;
        let daemon_use_sim_time = node_arguments.daemon_use_sim_time;

        let node_config = NodeConfig {
            peppy_schema: PeppySchema::NodeV1,
            manifest: Manifest {
                name: manifest_name,
                tag: CORE_NODE_TAG.to_string(),
                labels: None,
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
            clock_publish_interval,
            daemon_use_sim_time,
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

    pub fn node_tag(&self) -> &str {
        self.node_config.manifest.tag.as_str()
    }

    pub fn instance_id(&self) -> &str {
        self.instance_id.as_str()
    }

    pub async fn start(&self) -> Result<()> {
        self.start_with_ready(None).await
    }

    pub async fn start_with_ready(&self, ready: Option<oneshot::Sender<()>>) -> Result<()> {
        clear_instances_dir(&self.peppy_dirs);

        // Sync `repositories.json5` against the bundled default template so
        // entries that ship with a newer peppy build land in pre-existing
        // user configs.
        if let Err(e) = repo::ensure_default_repos(&self.peppy_dirs) {
            tracing::warn!("Failed to sync default repositories: {}", e);
        }

        let core_node_name = self.node_name(); // The core node binds to itself
        info!(
            "Starting the core node with name {} and instance_id {}...",
            self.node_name(),
            self.instance_id(),
        );
        // Build the clock source up front so the service handler and the
        // tick feeder share a single cache in sim mode. The cache is unused
        // (and the WallClockSource ignores it) in wall mode, but allocating
        // it unconditionally keeps the branch below readable.
        let clock_cache = Arc::new(AtomicU64::new(0));
        let clock_source: Arc<dyn ClockSource> = if self.daemon_use_sim_time {
            Arc::new(SimClockSource::new(Arc::clone(&clock_cache)))
        } else {
            Arc::new(WallClockSource)
        };
        // In-memory key/value store shared by the four datastore endpoints
        // (store, get, list, remove).
        let datastore = Arc::new(datastore::Datastore::new());
        // Set up all listeners concurrently so startup latency is bounded by
        // the slowest single listener, not the sum of all 24. They're
        // independent — no listener depends on another being registered first.
        let setup: Vec<BoxFuture<'_, Result<JoinHandle<Result<()>>>>> = vec![
            ping::listen_for_ping(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
            )
            .boxed(),
            clock::listen_for_clock(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&clock_source),
            )
            .boxed(),
            if self.daemon_use_sim_time {
                clock::subscribe_external_clock(
                    self.messenger.clone(),
                    core_node_name,
                    self.instance_id(),
                    self.node_name(),
                    Arc::clone(&clock_cache),
                )
                .boxed()
            } else {
                clock::publish_clock(
                    self.messenger.clone(),
                    core_node_name,
                    self.instance_id(),
                    self.node_name(),
                    self.clock_publish_interval,
                    Arc::clone(&clock_source),
                )
                .boxed()
            },
            info::listen_for_info(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.start_time,
            )
            .boxed(),
            datastore::listen_for_datastore_store(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&datastore),
            )
            .boxed(),
            datastore::listen_for_datastore_get(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&datastore),
            )
            .boxed(),
            datastore::listen_for_datastore_list(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&datastore),
            )
            .boxed(),
            datastore::listen_for_datastore_remove(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&datastore),
            )
            .boxed(),
            stack::listen_for_stack_launch(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
                stack::StackLaunchDefaults {
                    timeouts: stack::StackLaunchTimeouts {
                        node_startup: self.node_startup_timeout,
                        node_start_health: self.node_start_health_timeout,
                        health_monitor_interval: self.health_monitor_interval,
                        health_monitor_timeout: self.health_monitor_timeout,
                    },
                    use_sim_time: self.daemon_use_sim_time,
                },
            )
            .boxed(),
            stack::listen_for_stack_list(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .boxed(),
            stack::listen_for_stack_reset(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .boxed(),
            node::listen_for_node_add(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            node::listen_for_node_build(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            node::listen_for_node_info(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
                self.node_startup_timeout,
            )
            .boxed(),
            node::listen_for_node_remove(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .boxed(),
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
                },
            )
            .boxed(),
            node::listen_for_node_stop(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
            )
            .boxed(),
            node::listen_for_node_init(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            node::listen_for_node_sync(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            repo::listen_for_repo_add(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            repo::listen_for_repo_refresh(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            repo::listen_for_repo_list(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            repo::listen_for_repo_remove(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
            repo::listen_for_repo_exclude(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.peppy_dirs.clone(),
            )
            .boxed(),
        ];

        let handles = try_join_all(setup).await?;

        if let Some(ready) = ready {
            let _ = ready.send(());
        }

        // Wait for all service handlers
        try_join_all(handles)
            .await?
            .into_iter()
            .collect::<Result<Vec<_>>>()?;

        info!("Shutting down core node...");
        Ok(())
    }
}
