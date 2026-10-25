#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::poll::AbortOnDrop;
use super::test_node_target;
use config::consts::DEFAULT_MESSAGING_HOST;
use config::node::SetupTimeout;
use core_node::test_support::parked_restart_backoff;
use core_node::{
    CoreNode, CoreNodeArguments, CoreNodeConfig, HealthMonitorPolicy, ManualTime, ReplyBounds,
    RestartBackoff, SliceOwnership, TimeSource,
};
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::messaging::{MessengerHandle, ServiceAnswer, ServiceRequestContext};
use peppylib::runtime::spawn;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::future::Future;
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;

fn init_test_data_dir() -> (Option<TempDir>, PeppyDirs) {
    let dir = new_test_data_dir();
    let peppy_dirs = PeppyDirs::new(dir.path());
    (Some(dir), peppy_dirs)
}

pub async fn create_mock_messenger() -> Arc<Mutex<Messenger>> {
    let adapter = MockAdapter::default();
    let mut messenger = Messenger::new(MessengerAdapter::Mock(adapter));
    messenger
        .start_session()
        .await
        .expect("failed to start mock session");
    Arc::new(Mutex::new(messenger))
}

pub struct StartedCoreNode {
    pub shared_messenger: Arc<Mutex<Messenger>>,
    pub caller_handle: MessengerHandle,
    pub core_node_name: String,
    pub core_node_tag: String,
    pub node_stack: NodeStack,
    pub peppy_dirs: PeppyDirs,
    pub task: AbortOnDrop<core_node::Result<()>>,
    /// The same daemon-shutdown token the core node threads into every spawned
    /// node's health monitor and exit watcher. Exposed so a test can cancel it
    /// and assert the shutdown-time suppression (no spurious "became unhealthy",
    /// no crash relabeling of intentionally-stopped nodes).
    pub shutdown_token: tokio_util::sync::CancellationToken,
    /// Which launch holds the daemon and the gate its stack is changed under,
    /// so a test holds the stack or reserves the daemon as a change does.
    pub slice_ownership: Arc<SliceOwnership>,
    /// `Some` for the default random-root starters (dropped with the test).
    /// `None` when the peppy root is a stable path the caller manages (see
    /// [`start_core_node_with_mock_messenger_outside_home`]).
    _data_dir: Option<TempDir>,
}

/// How long the test daemon waits for a started node's ready signal.
pub const NODE_STARTUP_TIMEOUT: Duration = Duration::from_secs(10);

fn default_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: NODE_STARTUP_TIMEOUT,
        default_setup_timeout: setup_budget(30),
        health_monitor: HealthMonitorPolicy {
            interval: Duration::from_secs(5),
            timeout: Duration::from_secs(3),
            failure_threshold: NonZeroU32::new(3).expect("non-zero"),
            kill_after: None,
        },
        restart_backoff: parked_restart_backoff(),
        // Faster than the production default (100 ms) so publish_clock tests
        // observe several ticks within a small fixed budget without flaking.
        clock_publish_interval: Duration::from_millis(50),
        // Faster than production (5 s) so the heartbeat test observes beats
        // quickly without flaking.
        heartbeat_interval: Duration::from_millis(200),
        // These daemons run standalone routers (no federation links), where
        // production also settles for zero.
        name_claim_settle: Duration::ZERO,
        time_source: core_node::TimeSource::System,
        reply_bounds: core_node::ReplyBounds::DEFAULT,
    }
}

pub async fn start_core_node_with_mock_messenger() -> StartedCoreNode {
    start_core_node_with_mock_messenger_on(new_test_data_dir()).await
}

/// Boots the core node on a data root the test prepared, for a test of what
/// the daemon does at start with the state it finds there.
pub async fn start_core_node_with_mock_messenger_on(data_dir: TempDir) -> StartedCoreNode {
    let peppy_dirs = PeppyDirs::new(data_dir.path());
    let shared_messenger = create_mock_messenger().await;
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        Some(data_dir),
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

/// A data root for [`start_core_node_with_mock_messenger_on`], under the
/// same test root as every other starter's.
pub fn new_test_data_dir() -> TempDir {
    TempDir::new_in(config_test_support::test_tmp_root()).expect("test data dir")
}

/// Boots the core node with its peppy data root at a stable path under the
/// system temp dir, i.e. outside `$HOME`, mirroring a `PEPPY_HOME` outside
/// `$HOME` (see `daemon_config::consts::resolve_root`).
///
/// The harness for container builds/runs from an outside-`$HOME` root:
/// on macOS the Lima guest VM only auto-mounts `$HOME`, so container
/// operations from this root only work when the daemon registers the root as
/// an extra Lima mount (`Apptainer::ensure_host_mounts`). Every other starter
/// roots under `test_tmp_root()` (inside the repo, under `$HOME`), which
/// silently sidesteps that requirement.
///
/// The path is deliberately stable rather than a `TempDir`: registering a new
/// mount rewrites `lima.yaml` and restarts the VM, and a random per-run path
/// would pay that restart on every run (and disturb concurrently running
/// container tests). With a stable path the restart happens at most once per
/// machine. Contents are wiped on entry so runs stay independent.
pub async fn start_core_node_with_mock_messenger_outside_home() -> StartedCoreNode {
    let root = std::env::temp_dir().join("peppy-test-outside-home-root");
    std::fs::create_dir_all(&root).expect("create stable outside-home test root");
    for entry in std::fs::read_dir(&root).expect("read stable outside-home test root") {
        let path = entry.expect("read dir entry").path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path).expect("wipe stale test root subdir");
        } else {
            std::fs::remove_file(&path).expect("wipe stale test root file");
        }
    }
    let peppy_dirs = PeppyDirs::new(&root);
    let shared_messenger = create_mock_messenger().await;
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        None,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

/// A setup budget of `secs` seconds, in range.
pub fn setup_budget(secs: u64) -> SetupTimeout {
    SetupTimeout::from_secs(secs).expect("a setup budget in range")
}

pub async fn start_core_node_with_real_messenger() -> StartedCoreNode {
    start_real_core_node(
        default_node_arguments(),
        daemon_config::peppy_config::LocalNodesTopology::Peer,
    )
    .await
}

/// Variant of [`start_core_node_with_real_messenger`] in an explicit messaging
/// `topology`, for the dual-topology e2e tests parameterized over the
/// topology.
pub async fn start_core_node_with_real_messenger_topology(
    topology: daemon_config::peppy_config::LocalNodesTopology,
) -> StartedCoreNode {
    start_real_core_node(default_node_arguments(), topology).await
}

pub async fn start_core_node_with_real_messenger_and_timeouts(
    node_startup_timeout: Duration,
    default_setup_timeout: SetupTimeout,
) -> StartedCoreNode {
    let mut args = default_node_arguments();
    args.node_startup_timeout = node_startup_timeout;
    args.default_setup_timeout = default_setup_timeout;
    start_real_core_node(args, daemon_config::peppy_config::LocalNodesTopology::Peer).await
}

/// Boots the core node on a real zenoh router in the messaging `topology`
/// (peer vs router). The core node's own session is built in that topology,
/// and its `PeppyConfig` carries it so spawned nodes are injected with the
/// same topology (faithful to production).
async fn start_real_core_node(
    node_arguments: CoreNodeArguments,
    topology: daemon_config::peppy_config::LocalNodesTopology,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let mut instance = pmi::ZenohAdapter::start_router_ephemeral_in_mode(
        DEFAULT_MESSAGING_HOST,
        None,
        topology.gossip(),
        pmi::SubscriberBufferSizes::default(),
        // The core node stamps the `local` workspace namespace onto every node it
        // spawns (see `namespace` below); its own session must open
        // under the same namespace or it cannot reach a spawned node's
        // node_ready/health services. Mirrors the daemon's
        // `with_router(...).with_namespace(...)` pairing in production.
        Some(config::namespace::Namespace::local()),
    )
    .await
    .expect("failed to start zenoh router for test");
    instance
        .messenger()
        .start_session()
        .await
        .expect("failed to start zenoh session");
    let shared_messenger = Arc::new(Mutex::new(instance.take_messenger()));
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        zenoh: daemon_config::peppy_config::ZenohConfig::Managed(
            daemon_config::peppy_config::ManagedZenohConfig {
                local_nodes_topology: topology,
                ..Default::default()
            },
        ),
        ..Default::default()
    };
    start_core_node_with_messenger(
        shared_messenger,
        node_arguments,
        data_dir,
        peppy_dirs,
        peppy_config,
    )
    .await
}

/// Variant of [`start_core_node_with_mock_messenger`] with a custom
/// cooperative-shutdown grace period (`peppy_config.lifecycle
/// .shutdown_grace_secs`). For tests that assert timing around the grace
/// window and need wider margins than the 5s default gives under parallel
/// test load.
pub async fn start_core_node_with_shutdown_grace(shutdown_grace_secs: u64) -> StartedCoreNode {
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        lifecycle: daemon_config::peppy_config::LifecycleConfig {
            shutdown_grace_secs,
            ..Default::default()
        },
        ..Default::default()
    };
    start_mock_core_node(default_node_arguments(), peppy_config).await
}

/// Variant of [`start_core_node_with_mock_messenger`] whose config names a
/// PyPI mirror (`peppy_config.pypi_mirror`), for the tests of Python builds
/// that download from it.
pub async fn start_core_node_with_pypi_mirror(
    mirror: daemon_config::peppy_config::PackagesBaseUrl,
) -> StartedCoreNode {
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        pypi_mirror: Some(mirror),
        ..Default::default()
    };
    start_mock_core_node(default_node_arguments(), peppy_config).await
}

/// Variant of [`start_core_node_with_mock_messenger`] whose default setup
/// budget is `default_setup_timeout`.
pub async fn start_core_node_with_default_setup_timeout(
    default_setup_timeout: SetupTimeout,
) -> StartedCoreNode {
    let mut args = default_node_arguments();
    args.default_setup_timeout = default_setup_timeout;
    start_mock_core_node(args, daemon_config::peppy_config::PeppyConfig::default()).await
}

/// A health monitor policy that probes every 200ms with a 100ms budget, so
/// tests observe transitions within a few hundred milliseconds. The threshold
/// is the test's choice: the production value (3) to exercise the real
/// tolerance, or 1 when the test asserts that no transition is ever logged and
/// wants a regressed monitor to show itself on the first missed probe.
pub fn fast_health_monitor(failure_threshold: u32) -> HealthMonitorPolicy {
    HealthMonitorPolicy {
        interval: Duration::from_millis(200),
        timeout: Duration::from_millis(100),
        failure_threshold: NonZeroU32::new(failure_threshold).expect("non-zero threshold"),
        kill_after: None,
    }
}

/// [`fast_health_monitor`] that kills an instance whose policy restarts on
/// failure once it has answered no health check for `kill_after`.
pub fn fast_health_monitor_with_kill(
    failure_threshold: u32,
    kill_after: Duration,
) -> HealthMonitorPolicy {
    HealthMonitorPolicy {
        kill_after: Some(kill_after),
        ..fast_health_monitor(failure_threshold)
    }
}

pub async fn start_core_node_with_health_monitor(
    health_monitor: HealthMonitorPolicy,
) -> StartedCoreNode {
    start_core_node_with_supervision(health_monitor, parked_restart_backoff()).await
}

/// A core node whose health monitor and restart backoff the test chooses.
pub async fn start_core_node_with_supervision(
    health_monitor: HealthMonitorPolicy,
    restart_backoff: RestartBackoff,
) -> StartedCoreNode {
    start_core_node_with_supervision_and_grace(
        health_monitor,
        restart_backoff,
        config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
    )
    .await
}

/// [`start_core_node_with_supervision`] with a cooperative-shutdown grace
/// of `shutdown_grace_secs`, which every kill's force-kill deadline grows
/// from.
pub async fn start_core_node_with_supervision_and_grace(
    health_monitor: HealthMonitorPolicy,
    restart_backoff: RestartBackoff,
    shutdown_grace_secs: u64,
) -> StartedCoreNode {
    let mut args = default_node_arguments();
    args.health_monitor = health_monitor;
    args.restart_backoff = restart_backoff;
    start_mock_core_node(args, config_with_grace(shutdown_grace_secs)).await
}

/// How long each guard bound of a daemon on manual time lasts: no wait of a
/// test comes near it on a host of any speed, so a wait of the daemon ends by
/// its own outcome or once the test moves the time past it.
pub const OUT_OF_REACH: Duration = Duration::from_secs(3600);

/// A core node whose supervision runs on a [`ManualTime`], which only the
/// test advances, with `health_monitor`, `restart_backoff` and a
/// cooperative-shutdown grace of `shutdown_grace_secs`. Its guard bounds,
/// the ready wait, the setup budget and the bounds on the answers to its
/// requests, are out of reach.
pub async fn start_core_node_on_manual_time(
    health_monitor: HealthMonitorPolicy,
    restart_backoff: RestartBackoff,
    shutdown_grace_secs: u64,
) -> StartedCoreNode {
    let mut args = default_node_arguments();
    args.health_monitor = health_monitor;
    args.restart_backoff = restart_backoff;
    args.time_source = TimeSource::Manual(ManualTime::new());
    args.node_startup_timeout = OUT_OF_REACH;
    args.default_setup_timeout = SetupTimeout::MAX;
    args.reply_bounds = ReplyBounds {
        shutdown: OUT_OF_REACH,
        notify: OUT_OF_REACH,
        // A pair commit contains the peer update of its receiver.
        pair_commit: OUT_OF_REACH * 2,
        peer_update: OUT_OF_REACH,
        binding_update: OUT_OF_REACH,
        observation_update: OUT_OF_REACH,
    };
    start_mock_core_node(args, config_with_grace(shutdown_grace_secs)).await
}

/// A daemon config whose cooperative-shutdown grace is `shutdown_grace_secs`.
fn config_with_grace(shutdown_grace_secs: u64) -> daemon_config::peppy_config::PeppyConfig {
    daemon_config::peppy_config::PeppyConfig {
        lifecycle: daemon_config::peppy_config::LifecycleConfig {
            shutdown_grace_secs,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// Variant of [`start_core_node_with_mock_messenger`] whose log files export
/// their lines at `min_severity` and above. Returns the receiving end of the
/// exporter's queue, which the daemon's export worker holds in production.
pub async fn start_core_node_with_log_export(
    min_severity: Option<daemon_config::peppy_config::OtlpMinSeverity>,
) -> (StartedCoreNode, log_export::LogRecordReceiver) {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let (log_exporter, records) = log_export::LogExporter::channel(min_severity);
    let started = start_core_node(
        shared_messenger,
        default_node_arguments(),
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
        log_exporter,
    )
    .await;
    (started, records)
}

/// Boots the core node on a mock messenger and a fresh test data root.
async fn start_mock_core_node(
    node_arguments: CoreNodeArguments,
    peppy_config: daemon_config::peppy_config::PeppyConfig,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    start_core_node_with_messenger(
        shared_messenger,
        node_arguments,
        data_dir,
        peppy_dirs,
        peppy_config,
    )
    .await
}

async fn start_core_node_with_messenger(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_arguments: CoreNodeArguments,
    data_dir: Option<TempDir>,
    peppy_dirs: PeppyDirs,
    peppy_config: daemon_config::peppy_config::PeppyConfig,
) -> StartedCoreNode {
    start_core_node(
        shared_messenger,
        node_arguments,
        data_dir,
        peppy_dirs,
        peppy_config,
        log_export::LogExporter::disabled(),
    )
    .await
}

async fn start_core_node(
    shared_messenger: Arc<Mutex<Messenger>>,
    node_arguments: CoreNodeArguments,
    data_dir: Option<TempDir>,
    peppy_dirs: PeppyDirs,
    peppy_config: daemon_config::peppy_config::PeppyConfig,
    log_exporter: log_export::LogExporter,
) -> StartedCoreNode {
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let core_node = CoreNode::new(CoreNodeConfig {
        messenger: Arc::clone(&shared_messenger),
        node_name: Some("test_core_node".to_string()),
        arguments: node_arguments,
        host_addresses: Some(core_node::test_host_addresses()),
        root_dir,
        peppy_dirs: peppy_dirs.clone(),
        peppy_config,
        namespace: config::namespace::Namespace::local(),
        shutdown_token: shutdown_token.clone(),
        log_exporter,
    });
    let core_node_name = core_node.node_name().to_string();
    let core_node_tag = core_node.node_config().manifest.tag.clone();
    let node_stack = core_node.node_stack().clone();
    let slice_ownership = core_node.slice_ownership();

    // `start_with_ready` reports once every service is registered, so a test
    // can send requests as soon as this returns.
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });
    ready_rx.await.expect("core node ready signal failed");

    StartedCoreNode {
        shared_messenger,
        caller_handle,
        core_node_name,
        core_node_tag,
        node_stack,
        peppy_dirs,
        task: AbortOnDrop(task),
        shutdown_token,
        slice_ownership,
        _data_dir: data_dir,
    }
}

// =============================================================================
// Real-lifecycle test helpers with calls to NodeEntity::build + prepare_and_spawn + commit_started.
// =============================================================================

/// RAII guard for a test-spawned `Running` instance. On drop it calls
/// `stop_instance` on the entity and SIGTERMs the real child process.
#[must_use = "guard keeps the spawned child alive; drop it to tear down the instance"]
pub struct TestRunningInstance {
    pub pid: u32,
    pub instance_id: config::runtime::Name,
    handle: node_stack::EntityHandle,
    _working_dir: Option<TempDir>,
    _feedback_drain: tokio::task::JoinHandle<()>,
    _shutdown_listener: Option<AbortOnDrop<peppylib::PeppyResult<()>>>,
}

impl Drop for TestRunningInstance {
    fn drop(&mut self) {
        self.handle.write().stop_instance(&self.instance_id);
        let _ = std::process::Command::new("kill")
            .arg("-TERM")
            .arg(self.pid.to_string())
            .status();
        self._feedback_drain.abort();
    }
}

struct NoOpOutputHooks;
impl node_stack::build_io::OutputReaderHooks for NoOpOutputHooks {}

/// Opens a feedback channel for an announcer and spawns the task that
/// discards its lines, as no test reads them.
fn spawn_feedback_drain() -> (
    tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
    tokio::task::JoinHandle<()>,
) {
    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    let drain = tokio::spawn(async move { while feedback_rx.recv().await.is_some() {} });
    (feedback_tx, drain)
}

/// The output sinks of a start of `instance_id` of the node `name:tag`, its
/// run log opened as the daemon opens one, with the task that drains its
/// feedback and the run log's path.
fn make_real_output_sinks(
    peppy_dirs: &PeppyDirs,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> (
    node_stack::OutputSinks,
    tokio::task::JoinHandle<()>,
    std::path::PathBuf,
) {
    use std::sync::atomic::AtomicBool;

    let log = node_stack::ActionLog::for_run(
        peppy_dirs,
        log_export::LogExporter::disabled(),
        name,
        tag,
        instance_id.as_str(),
        None,
    )
    .expect("create run log");
    let run_log_path = log.path();
    let (feedback_tx, drain) = spawn_feedback_drain();
    let output_sinks = node_stack::OutputSinks {
        announcer: node_stack::Announcer::new(log, feedback_tx),
        publish_enabled: Arc::new(AtomicBool::new(true)),
        hooks: Arc::new(NoOpOutputHooks),
    };
    (output_sinks, drain, run_log_path)
}

/// A child that `prepare_and_spawn` started for an instance whose start is
/// not committed yet.
struct PreparedChild {
    handle: node_stack::EntityHandle,
    child: tokio::process::Child,
    started_ctx: node_stack::StartedInstanceCtx,
    pid: u32,
    feedback_drain: tokio::task::JoinHandle<()>,
}

/// Drives a real `prepare_and_spawn` on the entity at `(name, tag)`, which
/// must already be in `Ready`. Spawns a real child via the entity's existing
/// `run_cmd`; callers are responsible for ensuring the node config's run_cmd
/// is spawnable in the test environment.
async fn prepare_and_spawn_on_ready_entity(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> PreparedChild {
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("entity to spawn should exist");
    let (output_sinks, feedback_drain, run_log_path) =
        make_real_output_sinks(&started.peppy_dirs, name, tag, instance_id);

    let (child, started_ctx) = node_stack::NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            core_node: &started.core_node_name,
            run_log_path: &run_log_path,
            instance_id,
            copy: None,
            runtime_config_json5: "{}",
            slot_bindings: std::collections::BTreeMap::new(),
            clock: Default::default(),
            launch: None,
            spawn: new_spawn(instance_id, name, tag),
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &started.peppy_dirs,
            output_sinks,
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");
    let pid = child.id().expect("child should have pid");
    PreparedChild {
        handle,
        child,
        started_ctx,
        pid,
        feedback_drain,
    }
}

/// Drives a real `prepare_and_spawn` + `commit_started` on the entity at
/// `(name, tag)`, which must already be in `Ready` (the listener tests use a
/// `run_cmd` of `["sleep", "10"]`). Also installs the shutdown listener of
/// [`install_kill_on_shutdown_listener`], so the production stop path observes
/// the child as cooperatively terminated within its graceful window rather
/// than having to force-kill a stubborn `sleep 10`. Returns a guard that
/// SIGTERMs the child on drop.
pub async fn spawn_real_running_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    let mut running = spawn_real_stuck_instance(started, name, tag, instance_id).await;
    running._shutdown_listener =
        Some(install_kill_on_shutdown_listener(started, name, instance_id, running.pid).await);
    running
}

/// Variant of [`spawn_real_running_instance`] that skips installing a
/// shutdown listener. Used by tests that want the production shutdown path to
/// observe a stuck process that never terminates.
pub async fn spawn_real_stuck_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    let PreparedChild {
        handle,
        child,
        started_ctx,
        pid,
        feedback_drain,
    } = prepare_and_spawn_on_ready_entity(started, name, tag, instance_id).await;
    node_stack::NodeEntity::commit_started(
        &handle,
        child,
        started_ctx,
        instance_id.clone(),
        Vec::new(),
        started.node_stack.time().now(),
    )
    .await
    .expect("commit_started should succeed");

    TestRunningInstance {
        pid,
        instance_id: instance_id.clone(),
        handle,
        _working_dir: None,
        _feedback_drain: feedback_drain,
        _shutdown_listener: None,
    }
}

/// Answers the ready probe the daemon sends to `instance_id` of `node`,
/// standing in for a node process that has started, until the returned guard
/// drops.
pub async fn answer_ready_probe(
    started: &StartedCoreNode,
    node: &str,
    instance_id: &str,
) -> AbortOnDrop<peppylib::PeppyResult<()>> {
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    AbortOnDrop(
        peppylib::services::ready::listen_for_node_ready(
            &messenger,
            &started.core_node_name,
            instance_id,
            test_node_target(node),
        )
        .await
        .expect("ready service should start"),
    )
}

/// Answers the health probes the daemon sends to `instance_id` of `node`,
/// standing in for a node process that reports healthy, until the returned
/// guard drops. A test drops the guard to make a running node stop answering.
pub async fn answer_health_probes(
    started: &StartedCoreNode,
    node: &str,
    instance_id: &str,
) -> AbortOnDrop<peppylib::PeppyResult<()>> {
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    AbortOnDrop(
        peppylib::services::health::listen_for_node_health(
            &messenger,
            &started.core_node_name,
            instance_id,
            test_node_target(node),
        )
        .await
        .expect("health service should start"),
    )
}

/// Answers the ready and health probes the daemon sends to `instance_id` of
/// `node`, standing in for a node process that reports healthy, until the
/// returned guards drop.
pub async fn answer_readiness(
    started: &StartedCoreNode,
    node: &str,
    instance_id: &str,
) -> Vec<AbortOnDrop<peppylib::PeppyResult<()>>> {
    vec![
        answer_ready_probe(started, node, instance_id).await,
        answer_health_probes(started, node, instance_id).await,
    ]
}

/// Installs a messenger-side shutdown listener that SIGKILLs `pid` when the
/// daemon fires a cooperative `SHUTDOWN_SERVICE` signal at `(name,
/// instance_id)`. A node started through the real `node_run` service path gets a
/// live exit watcher but no node-side shutdown handling (the `run_cmd` is a bare
/// `sleep`), so without this it would ignore the cooperative phase and force the
/// stop/reset/teardown to wait out the whole force-kill deadline. Bridging the
/// signal to a kill lets those tests cooperate quickly while still exercising the
/// watcher-versus-stop interaction. The returned guard aborts the listener on
/// drop.
pub async fn install_kill_on_shutdown_listener(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &config::runtime::Name,
    pid: u32,
) -> AbortOnDrop<peppylib::PeppyResult<()>> {
    let shutdown_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (shutdown_task, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
        &shutdown_handle,
        &started.core_node_name,
        instance_id.as_str(),
        test_node_target(name),
    )
    .await
    .expect("failed to start shutdown listener for test instance");
    tokio::spawn(async move {
        if shutdown_rx.await.is_ok() {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
    });
    AbortOnDrop(shutdown_task)
}

/// Serves the shutdown request of the instance `instance_id` of `name:tag`
/// in the test process, as the runtime of a node serves it from its ready
/// signal on: the request SIGKILLs the process the stack records for the
/// instance, a `Starting` one included, and is reported on the returned
/// receiver. The guard aborts the listener on drop.
pub async fn install_shutdown_listener_for_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &str,
) -> (
    AbortOnDrop<peppylib::PeppyResult<()>>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let shutdown_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let (shutdown_task, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
        &shutdown_handle,
        &started.core_node_name,
        instance_id,
        test_node_target(name),
    )
    .await
    .expect("failed to start shutdown listener for test instance");
    let node_stack = started.node_stack.clone();
    let (name, tag, instance_id) = (name.to_owned(), tag.to_owned(), instance_id.to_owned());
    let (requested_tx, requested_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        if shutdown_rx.await.is_err() {
            return;
        }
        let pid = node_stack.find(&name, &tag).and_then(|handle| {
            handle
                .read()
                .instances()
                .iter()
                .find(|inst| inst.instance_id().as_str() == instance_id)
                .and_then(|inst| inst.pid())
        });
        if let Some(pid) = pid {
            let _ = std::process::Command::new("kill")
                .arg("-KILL")
                .arg(pid.to_string())
                .status();
        }
        let _ = requested_tx.send(());
    });
    (AbortOnDrop(shutdown_task), requested_rx)
}

/// Gives mocks registered by hand a moment to subscribe before the run
/// they answer.
pub async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

/// The `service` of the peer daemon `core_node`, served by its root
/// instance and answering each request with `handler`, settled before it
/// returns.
pub async fn peer_daemon_service<F, Fut>(
    started: &StartedCoreNode,
    core_node: &str,
    service: core_node_api::ServiceId,
    handler: F,
) -> AbortOnDrop<peppylib::PeppyResult<()>>
where
    F: FnMut(ServiceRequestContext) -> Fut + Send + 'static,
    Fut: Future<Output = ServiceAnswer> + Send + 'static,
{
    let mut listener = peppylib::ServiceMessenger::listen(
        &started.caller_handle,
        core_node,
        &format!("{core_node}_root"),
        super::core_node_target(core_node),
        service.name(),
    )
    .await
    .expect("the peer daemon listens");
    let mock = AbortOnDrop(spawn(
        async move { listener.handle_requests(handler).await },
    ));
    settle().await;
    mock
}

/// The `service` mock of `instance_id` of `name`: a node-side endpoint
/// answering each request with `handler`, settled before it returns. The
/// returned guard aborts it on drop.
pub async fn node_service_mock<F, Fut>(
    started: &StartedCoreNode,
    name: &str,
    instance_id: &str,
    service: &str,
    handler: F,
) -> AbortOnDrop<peppylib::PeppyResult<()>>
where
    F: FnMut(ServiceRequestContext) -> Fut + Send + 'static,
    Fut: Future<Output = ServiceAnswer> + Send + 'static,
{
    let messenger = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
    let mut endpoint = peppylib::ServiceMessenger::listen(
        &messenger,
        &started.core_node_name,
        instance_id,
        test_node_target(name),
        service,
    )
    .await
    .expect("the node's service mock starts");
    let mock = AbortOnDrop(spawn(
        async move { endpoint.handle_requests(handler).await },
    ));
    settle().await;
    mock
}

/// RAII guard for a test-spawned instance deliberately left in `Starting`:
/// `prepare_and_spawn` was driven but `commit_started` was NOT, so the instance
/// is registered as `Starting` with a live child, exactly the state a node is in
/// mid-launch. Holds the `Child` and `StartedInstanceCtx` so the launch is
/// neither committed nor aborted. On drop it SIGKILLs the child's whole process
/// group (negative pid) so the held `sleep`s don't leak past the test.
#[must_use = "guard keeps the half-started child alive; drop it to clean up"]
pub struct TestStartingInstance {
    pub pid: u32,
    pub instance_id: config::runtime::Name,
    _child: tokio::process::Child,
    _started_ctx: node_stack::StartedInstanceCtx,
    _feedback_drain: tokio::task::JoinHandle<()>,
}

impl Drop for TestStartingInstance {
    fn drop(&mut self) {
        // The daemon's own group kill (the `killpg` syscall), not a
        // `kill -KILL -<pid>` subprocess: util-linux's `kill` reads a
        // leading-dash pid as an option cluster and keeps only the first
        // digit, so `-126750` becomes `kill(-1, SIGKILL)`, a broadcast to
        // every process the user may signal. Best-effort: by the time a test
        // drops this, teardown has usually already reaped the group, and the
        // helper ignores that ESRCH.
        node_stack::kill_process_group(self.pid);
        self._feedback_drain.abort();
    }
}

/// Drives a real `prepare_and_spawn` on the entity at `(name, tag)` (which must
/// already be in `Ready`) but intentionally does NOT call `commit_started`,
/// leaving the instance in `Starting` with a live child. Used to prove that a
/// daemon teardown during the start window force-kills the `Starting`-window
/// child instead of orphaning it. The caller is responsible for ensuring the
/// node config's `run_cmd` is spawnable in the test environment.
pub async fn spawn_real_starting_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestStartingInstance {
    let PreparedChild {
        child,
        started_ctx,
        pid,
        feedback_drain,
        ..
    } = prepare_and_spawn_on_ready_entity(started, name, tag, instance_id).await;

    TestStartingInstance {
        pid,
        instance_id: instance_id.clone(),
        _child: child,
        _started_ctx: started_ctx,
        _feedback_drain: feedback_drain,
    }
}

/// For tests that push a config directly (bypassing `process_node_add`): drives
/// the real `NodeEntity::build` (process-node archive path, no container) and
/// then a real `prepare_and_spawn` + `commit_started`.
pub async fn real_build_and_spawn_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("real_build_and_spawn_instance: entity should exist");

    let working_dir = TempDir::new().expect("working_dir tempdir");
    let build_log = node_stack::ActionLog::create(
        &started.peppy_dirs.logs_dir_add(),
        &format!("{}-build.log", instance_id.as_str()),
        log_export::LogKind::Build,
        log_export::LogExporter::disabled(),
    )
    .expect("create build log");
    let (build_feedback_tx, build_drain) = spawn_feedback_drain();

    node_stack::NodeEntity::build(
        &handle,
        node_stack::BuildContext {
            working_dir: working_dir.path(),
            peppy_dirs: &started.peppy_dirs,
            announcer: node_stack::Announcer::new(build_log, build_feedback_tx),
            env_vars: &[],
            cancel_token: tokio_util::sync::CancellationToken::new(),
            rebuild: false,
            pypi_mirror: None,
        },
    )
    .await
    .expect("real build should succeed on process node");
    build_drain.abort();

    let mut running = spawn_real_running_instance(started, name, tag, instance_id).await;
    running._working_dir = Some(working_dir);
    running
}

/// The spawn of a new instance under the default policy, with the goal a
/// fixture's start keeps.
fn new_spawn(
    instance_id: &config::runtime::Name,
    name: &str,
    tag: &str,
) -> node_stack::SpawnTarget {
    node_stack::SpawnTarget::New(node_stack::FirstStart::fixture(instance_id, name, tag))
}
