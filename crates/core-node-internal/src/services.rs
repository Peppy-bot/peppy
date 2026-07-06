mod action_loop;
mod clock;
mod datastore;
mod health;
mod info;
mod node;
pub(crate) mod repo;
mod response;
mod stack;

use clock::{ClockSource, SimClockSource, WallClockSource};

pub use node::{TEARDOWN_REAP_BUDGET, force_kill_deadline, teardown_all_instances};

use crate::Result;
use crate::names;
use config::{
    DefaultValue, ParameterSpec,
    node::{Execution, Manifest, NodeConfig, PeppygenLanguage, TypeToken},
    runtime::Name,
    schema::PeppySchema,
};
use daemon_config::consts::PeppyDirs;
use futures::future::{BoxFuture, FutureExt, try_join_all};
use names_generator2::get_random;
use node_stack::NodeStack;
use peppylib::messaging::{SenderTarget, ServiceTarget};
use peppylib::{MessengerHandle, ServiceMessenger};
use pmi::Messenger;
use rand::Rng;
use rand::SeedableRng;
use rand::rng;
use rand::rngs::StdRng;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, oneshot};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::info;

const CORE_NODE_TAG: &str = match option_env!("PEPPY_GIT_TAG") {
    Some(tag) => tag,
    None => "dev",
};

/// Boot-time self-probes sent before concluding the core-node name is
/// unclaimed. Each unanswered probe costs peppylib's probe timeout (500ms),
/// so a clean boot pays ~2s; a claimed name is refused on the first reply.
const SELF_PROBE_ATTEMPTS: u32 = 4;

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
    /// Cadence of the daemon-liveness heartbeat (small and fixed; the
    /// configurable grace period is many multiples of it).
    pub heartbeat_interval: Duration,
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

/// Everything [`CoreNode::new`] needs, grouped so the constructor reads as a
/// single named bundle rather than a long positional argument list (the
/// `Arc<Mutex<Messenger>>`, `PeppyDirs`, and `PeppyConfig` are easy to transpose
/// positionally).
pub struct CoreNodeConfig {
    /// Shared messaging interface the core node binds its services on.
    pub messenger: Arc<Mutex<Messenger>>,
    /// Explicit node name; `None` derives a deterministic machine-based default.
    pub node_name: Option<String>,
    /// Timeouts, intervals, and the daemon-wide sim-time flag.
    pub arguments: CoreNodeArguments,
    /// Root directory the node stack is anchored at.
    pub root_dir: PathBuf,
    /// Resolved peppy directory layout.
    pub peppy_dirs: PeppyDirs,
    /// Daemon-global messaging mode + peer buffer sizes, injected into every
    /// spawned node's runtime config (see `node::run`).
    pub peppy_config: daemon_config::peppy_config::PeppyConfig,
    /// The daemon's organization namespace for this generation (`"local"` when
    /// logged out, else the org id). Stamped onto every spawned node's
    /// `discovery.organization_id` so the node opens its session under the same
    /// namespace as the daemon.
    pub organization_namespace: String,
    /// Cancelled at the start of daemon shutdown to stop the core node's own
    /// clock + heartbeat publishers before the messaging session is closed, so
    /// they do not spin against a closed session logging a failed publish on
    /// every tick.
    pub shutdown_token: CancellationToken,
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
    heartbeat_interval: Duration,
    daemon_use_sim_time: bool,
    /// Daemon-global messaging mode + peer buffer sizes, read once at startup.
    /// Injected into every spawned node's runtime config (see `node::run`).
    peppy_config: daemon_config::peppy_config::PeppyConfig,
    /// The daemon's organization namespace for this generation, stamped onto
    /// every spawned node so it opens its session under the daemon's namespace.
    organization_namespace: String,
    /// Cancelled on shutdown to stop the clock + heartbeat publishers cleanly.
    /// Cloned into each publisher task in [`CoreNode::start_with_ready`].
    shutdown_token: CancellationToken,
    /// Flipped by [`CoreNode::start_with_ready`] so a second start on the same
    /// instance is rejected rather than silently re-registering listeners.
    started: AtomicBool,
}

/// Pre-flight checks that must pass before the daemon starts spawning nodes.
///
/// Returns an `Err` (rather than calling `std::process::exit`) so the caller
/// (the binary) decides how to report a failure. Keeping the library free of
/// process-exit means a `CoreNode` can be constructed in tests and embedders
/// without risking a host-process kill.
pub fn check_runtime_prerequisites() -> Result<()> {
    // Apptainer user namespaces: on Ubuntu 24.04+ an AppArmor profile is
    // required to allow unprivileged user namespace creation.
    #[cfg(target_os = "linux")]
    if let Err(e) = containers::Apptainer::new() {
        return Err(crate::Error::RuntimeCheck(e.to_string()));
    }
    Ok(())
}

/// Derives the deterministic default core-node name from the machine UID
/// (falling back to the hostname, then a random name). Reads host identifiers
/// (`machine_uid`/`hostname`); only called when no explicit name is supplied.
fn derive_core_node_name() -> Name {
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

    match seed_source {
        Some(src) => derive_name_from_host_id(&src),
        None => {
            tracing::warn!(
                "machine UID and hostname unavailable; falling back to non-deterministic core node name"
            );
            let mut random = rng();
            let generated = get_random(&mut random);
            let suffix = random.next_u32();
            format_core_node_name(&generated, suffix)
        }
    }
}

/// Deterministic branch of [`derive_core_node_name`], split out so tests can
/// drive it with explicit host identifiers. The SHA256 digest both seeds the
/// generator and yields the digit suffix, so the whole name is a pure
/// function of the host identifier.
fn derive_name_from_host_id(host_id: &str) -> Name {
    // Hash so the published name does not reveal the UID/hostname.
    let digest = Sha256::digest(host_id.as_bytes());
    let seed: [u8; 32] = digest.into();
    let suffix = u32::from_be_bytes(seed[..4].try_into().expect("sha256 digest >= 4 bytes"));
    let mut seeded = StdRng::from_seed(seed);
    format_core_node_name(&get_random(&mut seeded), suffix)
}

/// Assembles `core-node-{adj}-{surname}-{NNNN}-{DDDDDDDDDD}`: the generator's
/// human-readable base plus 10 zero-padded decimal digits. The 32 extra bits
/// keep fleet-wide birthday-collision odds negligible at 10k nodes (the
/// generator alone has only ~304M combinations).
fn format_core_node_name(generated: &str, suffix: u32) -> Name {
    Name::new(format!("core-node-{generated}-{suffix:010}")).unwrap()
}

impl CoreNode {
    pub fn new(config: CoreNodeConfig) -> Self {
        let CoreNodeConfig {
            messenger,
            node_name,
            arguments,
            root_dir,
            peppy_dirs,
            peppy_config,
            organization_namespace,
            shutdown_token,
        } = config;

        let manifest_name = match node_name {
            Some(name) => Name::new(name).unwrap(),
            None => derive_core_node_name(),
        };

        let node_startup_timeout = arguments.node_startup_timeout;
        let node_start_health_timeout = arguments.node_start_health_timeout;
        let health_monitor_interval = arguments.health_monitor_interval;
        let health_monitor_timeout = arguments.health_monitor_timeout;
        let clock_publish_interval = arguments.clock_publish_interval;
        let heartbeat_interval = arguments.heartbeat_interval;
        let daemon_use_sim_time = arguments.daemon_use_sim_time;

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
                parameters: arguments.into_parameters(),
                build_cmd: None,
                run_cmd: None,
                container: None,
            },
            interfaces: Default::default(),
        };

        let messenger = MessengerHandle::from_shared(messenger);
        let instance_id = Name::new(get_random(rng())).unwrap();
        // The core node is the root of the node stack. Resolve the cooperative
        // shutdown grace from config once and pin it on the stack so every stop
        // path (teardown, node_stop, overwrite) reads the same value.
        let node_stack = NodeStack::new(node_config.clone(), None, root_dir).with_shutdown_grace(
            Duration::from_secs(peppy_config.lifecycle.shutdown_grace_secs),
        );

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
            heartbeat_interval,
            daemon_use_sim_time,
            peppy_config,
            organization_namespace,
            shutdown_token,
            started: AtomicBool::new(false),
        }
    }

    pub fn node_stack(&self) -> &NodeStack {
        &self.node_stack
    }

    /// Tear down every spawned node on a catchable daemon shutdown (ctrl+C /
    /// SIGTERM): cooperatively stop them, then SIGKILL the process group of any
    /// straggler so none outlives the daemon as an orphan. Takes `&self` so the
    /// serve runner can call it after its shutdown-signal branch fires while the
    /// core node's own future is still pinned. See
    /// [`node::teardown_all_instances`].
    pub async fn teardown_node_stack(&self) {
        node::teardown_all_instances(
            &self.messenger,
            self.node_name(),
            self.instance_id(),
            &self.node_stack,
        )
        .await;
    }

    pub fn node_config(&self) -> &NodeConfig {
        &self.node_config
    }

    pub fn node_name(&self) -> &str {
        self.node_config.manifest.name.as_str()
    }

    pub(crate) fn instance_id(&self) -> &str {
        self.instance_id.as_str()
    }

    /// Boot-time self-probe: sends reachability probes to the `health` service
    /// under this daemon's own core-node name. Runs before this instance's
    /// listeners are registered, so any reply proves a foreign daemon already
    /// claims the name — breaking the name-based routing invariant every
    /// core-node API call rests on — and boot is refused with
    /// [`crate::Error::CoreNodeNameTaken`].
    ///
    /// Probe infrastructure errors fail open with a warning: only positive
    /// reachability refuses boot. Limitation: the probe sees only what the
    /// local router reaches when it runs. The serve runner therefore delays
    /// `start_with_ready` until the initial router federation has settled
    /// (bounded by the federation connect timeout), so the probe covers the
    /// federated mesh too; a daemon whose federation lands only later (slow
    /// backend past the bound, or a peer that federates in afterwards) is
    /// caught by the runtime alarm instead (see
    /// [`clock::watch_for_name_collision`]).
    async fn ensure_name_unclaimed(&self) -> Result<()> {
        for attempt in 1..=SELF_PROBE_ATTEMPTS {
            match ServiceMessenger::is_reachable(
                &self.messenger,
                self.node_name(),
                self.instance_id(),
                // The wire tag from `names` (`"core"`), not this file's
                // `CORE_NODE_TAG` git tag.
                SenderTarget::node(self.node_name(), names::CORE_NODE_TAG)?,
                names::HEALTH,
                ServiceTarget::Any,
            )
            .await
            {
                Ok(true) => {
                    return Err(crate::Error::CoreNodeNameTaken {
                        name: self.node_name().to_string(),
                    });
                }
                Ok(false) => {}
                Err(e) => tracing::warn!(
                    "core-node name self-probe {attempt}/{SELF_PROBE_ATTEMPTS} failed: {e}; \
                     continuing boot (only a positive reply refuses)"
                ),
            }
        }
        Ok(())
    }

    /// Boots the core node: registers every service listener and runs until the
    /// messaging session is torn down. The optional `ready` sender fires once
    /// all listeners are registered (used by tests and the serve runner to
    /// gate dependent startup).
    ///
    /// Side effects performed up front, before listeners are registered:
    /// - **Probes its own core-node name** and refuses to boot if a foreign
    ///   daemon already answers under it; see [`Self::ensure_name_unclaimed`].
    /// - **Deletes the instances directory** (`peppy_dirs.instances_dir()`) to
    ///   clear stale state from a previous run; see [`clear_instances_dir`].
    /// - **Writes/updates `repositories.json5`** via [`repo::ensure_default_repos`]
    ///   so newly-bundled default repos land in pre-existing user configs.
    pub async fn start_with_ready(&self, ready: Option<oneshot::Sender<()>>) -> Result<()> {
        // Boot exactly once per instance: a second `start` would re-run the
        // destructive setup below and register every listener twice.
        if self.started.swap(true, Ordering::SeqCst) {
            return Err(crate::Error::AlreadyStarted);
        }

        // Refuse a collision before any destructive setup: a refused boot must
        // leave the running daemon's on-disk state (instances dir) untouched.
        self.ensure_name_unclaimed().await?;

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
        // The daemon's single pairing authority. ONE instance shared by every
        // establishment hook (node run, stack launch) and clear path (node
        // stop, node add overwrite, exit watchers): its op lock serializes
        // all pairing operations daemon-wide.
        let pairing = Arc::new(node::PairingCoordinator::new(
            Arc::clone(&self.node_stack),
            self.messenger.clone(),
            core_node_name,
            self.instance_id(),
        ));
        // Set up all listeners concurrently so startup latency is bounded by
        // the slowest single listener, not the sum of all of them. They're
        // independent: no listener depends on another being registered first.
        let setup: Vec<BoxFuture<'_, Result<JoinHandle<Result<()>>>>> = vec![
            health::listen_for_health(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.start_time,
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
                    self.shutdown_token.clone(),
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
                    self.shutdown_token.clone(),
                )
                .boxed()
            },
            // Liveness beacon for spawned nodes' watchdogs. Unconditional (both
            // wall and sim mode), unlike the clock above which is wall-only.
            clock::publish_daemon_heartbeat(
                self.messenger.clone(),
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.heartbeat_interval,
                self.shutdown_token.clone(),
            )
            .boxed(),
            // Runtime complement to the boot-time self-probe: alarm if a
            // foreign daemon instance beats under this daemon's name (e.g.
            // one that federated in after boot, which the probe cannot see).
            clock::watch_for_name_collision(
                self.messenger.clone(),
                core_node_name,
                self.instance_id(),
                self.node_name(),
                self.shutdown_token.clone(),
            )
            .boxed(),
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
                    daemon_defaults: node::DaemonDefaults::from_peppy_config(
                        &self.peppy_config,
                        self.organization_namespace.clone(),
                    ),
                    shutdown_token: self.shutdown_token.clone(),
                },
                Arc::clone(&pairing),
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
            stack::listen_for_stack_benchmark(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                self.peppy_dirs.clone(),
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
                Arc::clone(&pairing),
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
                    daemon_defaults: node::DaemonDefaults::from_peppy_config(
                        &self.peppy_config,
                        self.organization_namespace.clone(),
                    ),
                    shutdown_token: self.shutdown_token.clone(),
                    pairing: Arc::clone(&pairing),
                },
            )
            .boxed(),
            node::listen_for_node_stop(
                &self.messenger,
                core_node_name,
                self.instance_id(),
                self.node_name(),
                Arc::clone(&self.node_stack),
                Arc::clone(&pairing),
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
