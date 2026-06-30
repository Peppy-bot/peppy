use crate::daemon_state::DaemonState;
use config::consts::{PEPPYGEN_OUTPUT_PATH, PeppyDirs};
use config::node::NodeConfigParser;
use core_node::{CoreNode, CoreNodeArguments, CoreNodeConfig};
use pmi::{Messenger, MessengerBackend, MockAdapter, MockInstance, ZenohAdapter, ZenohdInstance};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tracing_subscriber::fmt::MakeWriter;

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
/// idle timeout trips the cancel token) targets `sleep` directly — `sh -c "sleep N"` would
/// orphan the grandchild `sleep` and keep the daemon's stdio pipes open for the full sleep
/// duration.
pub fn override_run_cmd_silent(peppy_json5: &Path) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.run_cmd = Some(vec!["sleep".to_string(), "30".to_string()]);
        cfg.execution.build_cmd = None;
    });
}

#[derive(Clone, Default)]
pub struct LogCapture {
    buffer: Arc<parking_lot::Mutex<Vec<u8>>>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn logs(&self) -> String {
        String::from_utf8(self.buffer.lock().clone()).expect("captured logs are valid UTF-8")
    }
}

pub struct LogCaptureWriter {
    buffer: Arc<parking_lot::Mutex<Vec<u8>>>,
}

impl<'a> MakeWriter<'a> for LogCapture {
    type Writer = LogCaptureWriter;

    fn make_writer(&'a self) -> Self::Writer {
        LogCaptureWriter {
            buffer: Arc::clone(&self.buffer),
        }
    }
}

impl Write for LogCaptureWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.buffer.lock().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
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
    _instance: MessengerInstance,
    _core_node_task: JoinHandle<core_node::Result<()>>,
    shared_messenger: Arc<TokioMutex<Messenger>>,
    daemon_state_path: PathBuf,
    core_node_name: String,
}

impl ServeCommandEmulation {
    pub async fn with_mock() -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let mut instance = MockAdapter::start_router().await?;
        instance.messenger().start_session().await?;
        let messenger = instance.take_messenger();
        let port = messenger.get_host().port();
        Self::setup(messenger, port, MessengerInstance::Mock(instance)).await
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
            Some(pmi::OrgNamespace::local()),
        )
        .await?;
        instance.messenger().start_session().await?;
        let port = instance.port;
        let messenger = instance.take_messenger();
        Self::setup(messenger, port, MessengerInstance::Zenoh(instance)).await
    }

    async fn setup(
        messenger: Messenger,
        port: u16,
        instance: MessengerInstance,
    ) -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let temp_dir = TempDir::new().expect("failed to create temp dir for test");
        let daemon_state_path = DaemonState::state_file_in(temp_dir.path());
        let shared_messenger = Arc::new(TokioMutex::new(messenger));

        let peppy_dirs = PeppyDirs::new(temp_dir.path());

        // Pre-write an empty repositories.json5 so the daemon's `ensure_default_repos`
        // sees an existing file and treats it as the user's config rather than
        // falling back to the default template wholesale.
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).expect("failed to create conf dir");
        let repos_path = conf_dir.join("repositories.json5");
        std::fs::write(&repos_path, "[]").expect("failed to write repositories.json5");

        let core_node = CoreNode::new(CoreNodeConfig {
            messenger: Arc::clone(&shared_messenger),
            node_name: Some("test-core-node".to_string()),
            arguments: CoreNodeArguments {
                node_startup_timeout: Duration::from_secs(120),
                node_start_health_timeout: Duration::from_secs(30),
                health_monitor_interval: Duration::from_secs(5),
                health_monitor_timeout: Duration::from_secs(3),
                clock_publish_interval: Duration::from_millis(100),
                heartbeat_interval: Duration::from_secs(5),
                daemon_use_sim_time: false,
            },
            root_dir: temp_dir.path().to_path_buf(),
            peppy_dirs,
            peppy_config: config::peppy_config::PeppyConfig::default(),
            organization_namespace: "local".to_string(),
            shutdown_token: tokio_util::sync::CancellationToken::new(),
        });
        let core_node_name = core_node.node_name().to_string();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let core_node_task =
            tokio::spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });
        ready_rx.await.expect("core node ready signal failed");

        // `ensure_default_repos` runs during `start_with_ready` and appends the
        // bundled defaults (real GitHub URLs) to the empty file we wrote above.
        // Reset the file once the daemon is ready so tests that trigger a
        // refresh — e.g. `repo remove`, which fires a synchronous refresh
        // before responding — don't try to clone real remotes and time out on
        // slow networks. Tests that need specific contents can overwrite this.
        std::fs::write(&repos_path, "[]")
            .expect("failed to reset repositories.json5 after daemon startup");

        let daemon_state = DaemonState::new(
            &core_node_name,
            port,
            "test-git-hash",
            config::peppy_config::DEFAULT_SHUTDOWN_GRACE_SECS,
            "local",
        );
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: instance,
            _core_node_task: core_node_task,
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
