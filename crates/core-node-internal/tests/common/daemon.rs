#![allow(dead_code)] // Each test binary uses only a subset of these shared helpers.

use super::poll::AbortOnDrop;
use super::test_node_target;
use config::consts::DEFAULT_MESSAGING_HOST;
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig};
use daemon_config::consts::PeppyDirs;
use node_stack::NodeStack;
use peppylib::messaging::MessengerHandle;
use peppylib::runtime::spawn;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::Mutex;

fn init_test_data_dir() -> (Option<TempDir>, PeppyDirs) {
    let dir = TempDir::new_in(config_test_support::test_tmp_root()).expect("test data dir");
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

#[allow(dead_code)]
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
    /// `Some` for the default random-root starters (dropped with the test).
    /// `None` when the peppy root is a stable path the caller manages (see
    /// [`start_core_node_with_mock_messenger_outside_home`]).
    _data_dir: Option<TempDir>,
}

fn default_node_arguments() -> CoreNodeArguments {
    CoreNodeArguments {
        node_startup_timeout: Duration::from_secs(10),
        node_start_health_timeout: Duration::from_secs(30),
        health_monitor_interval: Duration::from_secs(5),
        health_monitor_timeout: Duration::from_secs(3),
        // Faster than the production default (100 ms) so publish_clock tests
        // observe several ticks within a small fixed budget without flaking.
        clock_publish_interval: Duration::from_millis(50),
        // Faster than production (5 s) so the heartbeat test observes beats
        // quickly without flaking.
        heartbeat_interval: Duration::from_millis(200),
        daemon_use_sim_time: false,
    }
}

pub async fn start_core_node_with_mock_messenger() -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

/// Boots the core node with its peppy data root at a stable path under the
/// system temp dir — i.e. outside `$HOME`, mirroring where dev binaries root
/// their data (`$TMPDIR/.peppy`, see `daemon_config::consts::resolve_root`).
///
/// Regression harness for container builds/runs from an outside-`$HOME` root:
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
    if root.exists() {
        for entry in std::fs::read_dir(&root).expect("read stable outside-home test root") {
            let path = entry.expect("read dir entry").path();
            if path.is_dir() {
                std::fs::remove_dir_all(&path).expect("wipe stale test root subdir");
            } else {
                std::fs::remove_file(&path).expect("wipe stale test root file");
            }
        }
    } else {
        std::fs::create_dir_all(&root).expect("create stable outside-home test root");
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

/// Boots the core node with `daemon_use_sim_time: true`. The daemon stops
/// publishing wall ticks and instead subscribes to the `clock` topic to fill
/// its internal cache, mirroring the production flow where an external
/// simulator drives the clock.
pub async fn start_core_node_with_sim_clock() -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.daemon_use_sim_time = true;
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

pub async fn start_core_node_with_real_messenger() -> StartedCoreNode {
    start_core_node_with_real_messenger_and_timeouts(
        Duration::from_secs(10),
        Duration::from_secs(30),
    )
    .await
}

/// Convenience wrapper over [`start_core_node_with_real_messenger_in_topology`]
/// with the default timeouts, for the dual-topology e2e tests parameterized
/// over the topology.
pub async fn start_core_node_with_real_messenger_topology(
    topology: daemon_config::peppy_config::LocalNodesTopology,
) -> StartedCoreNode {
    start_core_node_with_real_messenger_in_topology(
        Duration::from_secs(10),
        Duration::from_secs(30),
        topology,
    )
    .await
}

pub async fn start_core_node_with_real_messenger_and_timeouts(
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    start_core_node_with_real_messenger_in_topology(
        node_startup_timeout,
        node_start_health_timeout,
        daemon_config::peppy_config::LocalNodesTopology::Peer,
    )
    .await
}

/// Like [`start_core_node_with_real_messenger_and_timeouts`] but the messaging
/// `topology` (peer vs router) is explicit. The core node's own session is
/// built in that topology, and its `PeppyConfig` carries it so spawned nodes
/// are injected with the same topology (faithful to production). Used by the
/// dual-topology e2e tests.
pub async fn start_core_node_with_real_messenger_in_topology(
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
    topology: daemon_config::peppy_config::LocalNodesTopology,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let mut instance = pmi::ZenohAdapter::start_router_ephemeral_in_mode(
        DEFAULT_MESSAGING_HOST,
        None,
        topology.gossip(),
        pmi::SubscriberBufferSizes::default(),
        // The core node stamps the `local` org namespace onto every node it
        // spawns (see `organization_namespace` below); its own session must open
        // under the same namespace or it cannot reach a spawned node's
        // node_ready/health services. Mirrors the daemon's
        // `with_router(...).with_namespace(...)` pairing in production.
        Some(config::org::OrgNamespace::local()),
    )
    .await
    .expect("failed to start zenoh router for test");
    instance
        .messenger()
        .start_session()
        .await
        .expect("failed to start zenoh session");
    let shared_messenger = Arc::new(Mutex::new(instance.take_messenger()));
    let mut args = default_node_arguments();
    args.node_startup_timeout = node_startup_timeout;
    args.node_start_health_timeout = node_start_health_timeout;
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        zenoh: daemon_config::peppy_config::ZenohConfig::Managed(
            daemon_config::peppy_config::ManagedZenohConfig {
                local_nodes_topology: topology,
                ..Default::default()
            },
        ),
        ..Default::default()
    };
    start_core_node_with_messenger(shared_messenger, args, data_dir, peppy_dirs, peppy_config).await
}

/// Variant of [`start_core_node_with_mock_messenger`] with a custom
/// cooperative-shutdown grace period (`peppy_config.lifecycle
/// .shutdown_grace_secs`). For tests that assert timing around the grace
/// window and need wider margins than the 5s default gives under parallel
/// test load.
pub async fn start_core_node_with_shutdown_grace(shutdown_grace_secs: u64) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let peppy_config = daemon_config::peppy_config::PeppyConfig {
        lifecycle: daemon_config::peppy_config::LifecycleConfig {
            shutdown_grace_secs,
            ..Default::default()
        },
        ..Default::default()
    };
    start_core_node_with_messenger(
        shared_messenger,
        default_node_arguments(),
        data_dir,
        peppy_dirs,
        peppy_config,
    )
    .await
}

pub async fn start_core_node_with_health_timeout(
    node_start_health_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.node_start_health_timeout = node_start_health_timeout;
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
    )
    .await
}

pub async fn start_core_node_with_health_monitor(
    health_monitor_interval: Duration,
    health_monitor_timeout: Duration,
) -> StartedCoreNode {
    let (data_dir, peppy_dirs) = init_test_data_dir();
    let shared_messenger = create_mock_messenger().await;
    let mut args = default_node_arguments();
    args.health_monitor_interval = health_monitor_interval;
    args.health_monitor_timeout = health_monitor_timeout;
    start_core_node_with_messenger(
        shared_messenger,
        args,
        data_dir,
        peppy_dirs,
        daemon_config::peppy_config::PeppyConfig::default(),
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
    let caller_handle = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let root_dir = std::env::current_dir().expect("failed to get current directory");
    let shutdown_token = tokio_util::sync::CancellationToken::new();
    let core_node = CoreNode::new(CoreNodeConfig {
        messenger: Arc::clone(&shared_messenger),
        node_name: Some("test_core_node".to_string()),
        arguments: node_arguments,
        root_dir,
        peppy_dirs: peppy_dirs.clone(),
        peppy_config,
        organization_namespace: "local".to_string(),
        shutdown_token: shutdown_token.clone(),
    });
    let core_node_name = core_node.node_name().to_string();
    let core_node_tag = core_node.node_config().manifest.tag.clone();
    let node_stack = core_node.node_stack().clone();

    // Use start_with_ready to properly synchronize instead of a time-based sleep
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let task = spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });

    // Wait for all services to be fully registered before returning
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

fn make_real_output_sinks(
    peppy_dirs: &PeppyDirs,
    instance_id: &config::runtime::Name,
) -> (
    node_stack::OutputSinks,
    tokio::sync::mpsc::UnboundedSender<node_stack::build_io::FeedbackLine>,
    tokio::task::JoinHandle<()>,
) {
    use parking_lot::Mutex as StdMutex;
    use std::sync::atomic::AtomicBool;

    let log_dir = peppy_dirs.logs_dir_run();
    std::fs::create_dir_all(&log_dir).ok();
    let log_file = Arc::new(StdMutex::new(
        std::fs::File::create(log_dir.join(format!("{}.log", instance_id.as_str())))
            .expect("create start log"),
    ));
    let (feedback_tx, mut feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    let drain = tokio::spawn(async move { while feedback_rx.recv().await.is_some() {} });
    let output_sinks = node_stack::OutputSinks {
        feedback_tx: feedback_tx.clone(),
        log_file,
        publish_enabled: Arc::new(AtomicBool::new(true)),
        hooks: Arc::new(NoOpOutputHooks),
    };
    (output_sinks, feedback_tx, drain)
}

/// Drives a real `prepare_and_spawn` + `commit_started` on the entity at
/// `(name, tag)`, which must already be in `Ready`. Spawns a real child via
/// the entity's existing `run_cmd`; callers are responsible for ensuring
/// the node config's run_cmd is spawnable in the test environment (the
/// listener tests use `["sleep", "10"]`). Also installs a `listen_for_shutdown`
/// task on the messenger that SIGKILLs the entity-tracked pid when the
/// production stop/remove flow sends a shutdown signal. This lets the
/// production stop path observe the child as cooperatively terminated within
/// its graceful window rather than having to force-kill a stubborn `sleep 10`.
/// Returns a guard that SIGTERMs the child on drop.
pub async fn spawn_real_running_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    spawn_real_running_instance_inner(started, name, tag, instance_id, true).await
}

/// Variant of [`spawn_real_running_instance`] that skips installing a
/// shutdown listener. Used by tests that specifically want the production
/// shutdown path to observe a stuck process that never terminates (e.g. the
/// `node_add_same_node_with_running_instance_and_dependents_fails_on_stopped_node_stuck`
/// regression test).
pub async fn spawn_real_stuck_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    spawn_real_running_instance_inner(started, name, tag, instance_id, false).await
}

async fn spawn_real_running_instance_inner(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
    install_shutdown_listener: bool,
) -> TestRunningInstance {
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("spawn_real_running_instance: entity should exist");
    let (output_sinks, _feedback_tx, drain) =
        make_real_output_sinks(&started.peppy_dirs, instance_id);

    let (child, started_ctx) = node_stack::NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id,
            runtime_config_json5: "{}",
            slot_bindings: std::collections::BTreeMap::new(),
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &started.peppy_dirs,
            output_sinks,
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");
    let pid = child.id().expect("child should have pid");
    node_stack::NodeEntity::commit_started(&handle, child, started_ctx, instance_id.clone())
        .await
        .expect("commit_started should succeed");

    // Optionally install a messenger-side shutdown listener that kills the
    // child when the production stop/remove flow fires a SHUTDOWN_SERVICE
    // signal, so the cooperative phase succeeds within its graceful window.
    // Tests that want the production stop path to fall through to force-kill
    // (a stuck process) use `spawn_real_stuck_instance`, which skips this.
    let shutdown_listener = if install_shutdown_listener {
        let shutdown_handle = MessengerHandle::from_shared(Arc::clone(&started.shared_messenger));
        let (shutdown_task, shutdown_rx) = peppylib::services::shutdown::listen_for_shutdown(
            &shutdown_handle,
            &started.core_node_name,
            instance_id.as_str(),
            test_node_target(name),
        )
        .await
        .expect("failed to start shutdown listener for test instance");
        let pid_for_kill = pid;
        tokio::spawn(async move {
            if shutdown_rx.await.is_ok() {
                let _ = std::process::Command::new("kill")
                    .arg("-KILL")
                    .arg(pid_for_kill.to_string())
                    .status();
            }
        });
        Some(AbortOnDrop(shutdown_task))
    } else {
        None
    };

    TestRunningInstance {
        pid,
        instance_id: instance_id.clone(),
        handle,
        _working_dir: None,
        _feedback_drain: drain,
        _shutdown_listener: shutdown_listener,
    }
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
        // Best-effort: by the time a test drops this, teardown has usually
        // already reaped the group, so silence the expected ESRCH/EPERM noise.
        let _ = std::process::Command::new("kill")
            .arg("-KILL")
            .arg(format!("-{}", self.pid))
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
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
    let handle = started
        .node_stack
        .find(name, tag)
        .expect("spawn_real_starting_instance: entity should exist");
    let (output_sinks, _feedback_tx, drain) =
        make_real_output_sinks(&started.peppy_dirs, instance_id);

    let (child, started_ctx) = node_stack::NodeEntity::prepare_and_spawn(
        &handle,
        node_stack::StartContext {
            instance_id,
            runtime_config_json5: "{}",
            slot_bindings: std::collections::BTreeMap::new(),
            env_vars: &[],
            mount_paths_resolved: &[],
            peppy_dirs: &started.peppy_dirs,
            output_sinks,
        },
    )
    .await
    .expect("prepare_and_spawn should succeed on Ready entity");
    let pid = child.id().expect("child should have pid");

    TestStartingInstance {
        pid,
        instance_id: instance_id.clone(),
        _child: child,
        _started_ctx: started_ctx,
        _feedback_drain: drain,
    }
}

/// For tests that push a config directly (bypassing `process_node_add`): drives
/// the real `NodeEntity::build` (process-node archive path, no container) and
/// then a real `prepare_and_spawn` + `commit_started`. Replaces the old
/// `force_built_and_start_instance` backdoor helper.
pub async fn real_build_and_spawn_instance(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &config::runtime::Name,
) -> TestRunningInstance {
    use parking_lot::Mutex as StdMutex;

    let handle = started
        .node_stack
        .find(name, tag)
        .expect("real_build_and_spawn_instance: entity should exist");

    let working_dir = TempDir::new().expect("working_dir tempdir");
    let log_dir = started.peppy_dirs.logs_dir_add();
    std::fs::create_dir_all(&log_dir).ok();
    let build_log = Arc::new(StdMutex::new(
        std::fs::File::create(log_dir.join(format!("{}-build.log", instance_id.as_str())))
            .expect("create build log"),
    ));
    let (build_feedback_tx, mut build_feedback_rx) =
        tokio::sync::mpsc::unbounded_channel::<node_stack::build_io::FeedbackLine>();
    let build_drain =
        tokio::spawn(async move { while build_feedback_rx.recv().await.is_some() {} });

    node_stack::NodeEntity::build(
        &handle,
        node_stack::BuildContext {
            working_dir: working_dir.path(),
            peppy_dirs: &started.peppy_dirs,
            feedback_tx: &build_feedback_tx,
            log_file: build_log,
            env_vars: &[],
            cancel_token: tokio_util::sync::CancellationToken::new(),
        },
    )
    .await
    .expect("real build should succeed on process node");
    build_drain.abort();

    let mut running = spawn_real_running_instance(started, name, tag, instance_id).await;
    running._working_dir = Some(working_dir);
    running
}
