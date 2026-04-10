use crate::daemon_state::DaemonState;
use config::consts::{PEPPYGEN_OUTPUT_PATH, PeppyDirs};
use config::node::NodeConfigParser;
use core_node::{CoreNode, CoreNodeArguments};
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
    let mut cfg = NodeConfigParser::from_path(peppy_json5)
        .expect("peppy.json5 should read")
        .into_resolved()
        .expect("test node should resolve");
    modify(&mut cfg);
    let content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, content).expect("peppy.json5 should update");
    config::fingerprint::create_codegen_fingerprint(peppy_json5, Path::new(PEPPYGEN_OUTPUT_PATH));
}

/// Overrides the node start command to `sleep 4` and disables the add command,
/// preventing the test from spawning a real binary.
pub fn override_start_cmd(peppy_json5: &Path) {
    modify_node_config(peppy_json5, |cfg| {
        cfg.execution.start_cmd = Some(vec!["sleep".to_string(), "4".to_string()]);
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

pub fn wait_for_log(log_capture: &LogCapture, needle: &str, timeout: Duration) {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if log_capture.logs().contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "Timeout waiting for log entry '{}'. Last logs:\n{}",
        needle,
        log_capture.logs()
    );
}

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
    messaging_port: u16,
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
        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None).await?;
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
        let core_node = CoreNode::new(
            Arc::clone(&shared_messenger),
            Some("test-core-node"),
            CoreNodeArguments {
                node_startup_timeout: Duration::from_secs(120),
                node_start_health_timeout: Duration::from_secs(30),
            },
            temp_dir.path().to_path_buf(),
            peppy_dirs,
        );
        let core_node_name = core_node.node_name().to_string();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let core_node_task =
            tokio::spawn(async move { core_node.start_with_ready(Some(ready_tx)).await });
        ready_rx.await.expect("core node ready signal failed");

        let daemon_state = DaemonState::new(&core_node_name, port, "test-git-hash");
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: instance,
            _core_node_task: core_node_task,
            shared_messenger,
            daemon_state_path,
            core_node_name,
            messaging_port: port,
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

    pub fn messaging_port(&self) -> u16 {
        self.messaging_port
    }
}
