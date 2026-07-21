use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::NodeConfigParser;
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig};
use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;
use pmi::{Messenger, MessengerBackend, MockAdapter, MockInstance, ZenohAdapter, ZenohdInstance};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;

// In-memory `tracing` sink shared with the daemon crates' own tests; this
// crate's integration tests reach it as `peppy::test_support::LogCapture`.
pub use core_node::test_support::LogCapture;

/// Reads a node config, applies a mutation, writes it back, and regenerates the fingerprint.
fn modify_node_config(peppy_json5: &Path, modify: impl FnOnce(&mut config::node::NodeConfig)) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    modify(&mut cfg);
    let content = json5_pretty::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, content).expect("peppy.json5 should update");
    config::fingerprint::create_codegen_fingerprint(peppy_json5, Path::new(PEPPYGEN_OUTPUT_PATH));
}

/// Overrides the node run command to `sleep 4` and disables the build command,
/// preventing the test from spawning a real binary.
pub fn override_run_cmd(peppy_json5: &Path) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.run_cmd = Some(vec!["sleep".to_string(), "4".to_string()]);
        cfg.execution.build_cmd = None;
    });
}

/// Removes the `build_cmd` from a node's config so that integration tests can
/// exercise node operations (add, remove, runtime config) without triggering
/// the actual build step. Regenerates the codegen fingerprint after writing.
pub fn disable_build_cmd(peppy_json5: &Path) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.build_cmd = None;
    });
}

/// Overrides the node `build_cmd` to the given command, used by timeout tests that need a
/// long/quiet/loud build subprocess.
pub fn override_build_cmd(peppy_json5: &Path, cmd: Vec<String>) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.build_cmd = Some(cmd);
    });
}

/// Overrides the node `run_cmd` to a long, silent binary (`sleep 30`) and clears `build_cmd`.
/// Used by the run-idle timeout test: the process never produces output and never registers
/// itself with the messaging layer, so it never becomes "ready". Direct-binary form is used
/// so the cancellation SIGKILL (sent via `abort_started → kill_child` when the run-phase
/// idle timeout trips the cancel token) targets `sleep` directly; `sh -c "sleep N"` would
/// orphan the grandchild `sleep` and keep the daemon's stdio pipes open for the full sleep
/// duration.
pub fn override_run_cmd_silent(peppy_json5: &Path) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.run_cmd = Some(vec!["sleep".to_string(), "30".to_string()]);
        cfg.execution.build_cmd = None;
    });
}

/// Blocks until `needle` appears in the snapshot returned by `logs`, or panics
/// after `timeout` with the final snapshot. `logs` abstracts the log source:
/// pass `|| capture.logs()` for an in-process [`LogCapture`], or a clone of a
/// buffer drained from a child process's pipes (the daemon-lifecycle e2e test).
pub fn wait_for_log(logs: impl Fn() -> String, needle: &str, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if logs().contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "Timeout ({timeout:?}) waiting for log entry '{needle}'. Last logs:\n{}",
        logs()
    );
}

// Held only to keep the messaging router (a child process or in-process task)
// alive for the emulation's lifetime: the variants are constructed and stored in
// `_instance`, then dropped with the emulation, but never read. The dead-code
// lint is allowed here deliberately because the value is a lifetime guard, not
// data anyone inspects.
#[allow(dead_code)]
enum MessengerInstance {
    Mock(MockInstance),
    Zenoh(ZenohdInstance),
}

pub struct ServeCommandEmulation {
    _temp_dir: TempDir,
    _instance: Option<MessengerInstance>,
    _core_node_task: JoinHandle<core_node::Result<()>>,
    shutdown_token: tokio_util::sync::CancellationToken,
    shared_messenger: Arc<TokioMutex<Messenger>>,
    daemon_state_path: PathBuf,
    core_node_name: String,
}

impl ServeCommandEmulation {
    pub async fn with_mock() -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        Self::with_mock_named("test-core-node").await
    }

    /// Starts a named daemon on a fresh mock router.
    pub async fn with_mock_named(
        core_node_name: &str,
    ) -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let mut instance = MockAdapter::start_router().await?;
        instance.messenger().start_session().await?;
        let messenger = instance.take_messenger();
        let port = messenger.get_host().port();
        Self::setup(
            Arc::new(TokioMutex::new(messenger)),
            port,
            Some(MessengerInstance::Mock(instance)),
            core_node_name,
        )
        .await
    }

    /// Starts another named daemon on an existing mock messenger. Sharing the
    /// messenger is the mock adapter's in-process equivalent of separate
    /// sessions attached to one router and lets multi-daemon CLI tests exercise
    /// presence enumeration and cross-daemon service routing.
    pub async fn with_shared_mock(
        shared_messenger: Arc<TokioMutex<Messenger>>,
        core_node_name: &str,
    ) -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let port = shared_messenger.lock().await.get_host().port();
        Self::setup(shared_messenger, port, None, core_node_name).await
    }

    pub async fn with_zenoh() -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        // Host the router session under the `local` namespace so the core node's
        // service queryables are declared under the same namespace the spawned
        // `peppy` CLI opens its control session under. The CLI resolves `local`
        // from the daemon state we write below and connects with
        // `SessionScope::Namespace(local)`; zenoh sessions interoperate only when
        // their namespaces are equal, so a namespace-free router would leave
        // `node_init` (and every other core-node service) unreachable from the
        // CLI. The other arguments mirror `start_router_ephemeral`'s defaults.
        let mut instance = ZenohAdapter::start_router_ephemeral_in_mode(
            "127.0.0.1",
            None,
            true,
            pmi::SubscriberBufferSizes::default(),
            Some(pmi::Namespace::local()),
        )
        .await?;
        instance.messenger().start_session().await?;
        let port = instance.port;
        let messenger = instance.take_messenger();
        Self::setup(
            Arc::new(TokioMutex::new(messenger)),
            port,
            Some(MessengerInstance::Zenoh(instance)),
            "test-core-node",
        )
        .await
    }

    async fn setup(
        shared_messenger: Arc<TokioMutex<Messenger>>,
        port: u16,
        instance: Option<MessengerInstance>,
        core_node_name: &str,
    ) -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let temp_dir = TempDir::new().expect("failed to create temp dir for test");
        let daemon_state_path = DaemonState::state_file_in(temp_dir.path());

        let peppy_dirs = PeppyDirs::new(temp_dir.path());

        // Pre-write an empty repositories.json5 so the daemon's `ensure_default_repos`
        // sees an existing file and treats it as the user's config rather than
        // falling back to the default template wholesale.
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).expect("failed to create conf dir");
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(&repos_path, "[]").expect("failed to write repositories.json5");

        let shutdown_token = tokio_util::sync::CancellationToken::new();
        let core_node = CoreNode::new(CoreNodeConfig {
            messenger: Arc::clone(&shared_messenger),
            node_name: Some(core_node_name.to_string()),
            arguments: CoreNodeArguments {
                node_startup_timeout: Duration::from_secs(120),
                node_start_health_timeout: Duration::from_secs(30),
                health_monitor_interval: Duration::from_secs(5),
                health_monitor_timeout: Duration::from_secs(3),
                clock_publish_interval: Duration::from_millis(100),
                heartbeat_interval: Duration::from_secs(5),
                daemon_use_sim_time: false,
                // Zero: the emulation runs on an in-memory mock broker that
                // is authoritative immediately, with no links to settle.
                name_claim_settle: Duration::ZERO,
            },
            root_dir: temp_dir.path().to_path_buf(),
            peppy_dirs,
            peppy_config: daemon_config::peppy_config::PeppyConfig::default(),
            namespace: config::namespace::Namespace::local(),
            shutdown_token: shutdown_token.clone(),
        });
        let core_node_name = core_node.node_name().to_string();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let core_node_task =
            tokio::spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });
        if ready_rx.await.is_err() {
            // The ready sender only drops without firing when `start_with_ready`
            // bailed out (or panicked), so the join result holds the real error.
            // Surface it instead of an opaque `RecvError`.
            let failure = core_node_task.await;
            panic!("core node failed before signaling ready: {failure:?}");
        }

        // `ensure_default_repos` runs during `start_with_ready` and appends the
        // bundled defaults (real GitHub URLs) to the empty file we wrote above.
        // Reset the file once the daemon is ready so tests that trigger a
        // refresh (e.g. `repo remove`, which fires a synchronous refresh
        // before responding) don't try to clone real remotes and time out on
        // slow networks. Tests that need specific contents can overwrite this.
        std::fs::write(&repos_path, "[]")
            .expect("failed to reset repositories.json5 after daemon startup");

        let daemon_state = DaemonState::new(
            &core_node_name,
            config::consts::DEFAULT_MESSAGING_HOST,
            port,
            "test-git-hash",
            config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            config::namespace::Namespace::local(),
            None,
        );
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: instance,
            _core_node_task: core_node_task,
            shutdown_token,
            shared_messenger,
            daemon_state_path,
            core_node_name,
        })
    }

    pub fn messenger(&self) -> Arc<TokioMutex<Messenger>> {
        Arc::clone(&self.shared_messenger)
    }

    pub fn temp_dir(&self) -> &Path {
        self._temp_dir.path()
    }

    pub fn daemon_state_path(&self) -> &Path {
        &self.daemon_state_path
    }

    pub fn core_node_name(&self) -> &str {
        &self.core_node_name
    }
}

impl Drop for ServeCommandEmulation {
    fn drop(&mut self) {
        self.shutdown_token.cancel();
        self._core_node_task.abort();
    }
}
