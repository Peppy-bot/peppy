use crate::daemon_state::DaemonState;
use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::NodeConfigParser;
use daemon_node::{DaemonNode, DaemonNodeArguments};
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

pub fn override_start_cmd(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    cfg.manifest.start_cmd = vec!["sleep".to_string(), "4".to_string()];
    cfg.manifest.add_cmd = None;

    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");

    config::fingerprint::create_codegen_fingerprint(peppy_json5, Path::new(PEPPYGEN_OUTPUT_PATH));
}

pub fn disable_add_cmd(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    cfg.manifest.add_cmd = None;

    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");

    config::fingerprint::create_codegen_fingerprint(peppy_json5, Path::new(PEPPYGEN_OUTPUT_PATH));
}

#[derive(Clone, Default)]
pub struct LogCapture {
    buffer: Arc<std::sync::Mutex<Vec<u8>>>,
}

impl LogCapture {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn logs(&self) -> String {
        let buffer = self.buffer.lock().expect("log buffer poisoned");
        String::from_utf8(buffer.clone()).expect("captured logs are valid UTF-8")
    }
}

pub struct LogCaptureWriter {
    buffer: Arc<std::sync::Mutex<Vec<u8>>>,
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
        let mut buffer = self.buffer.lock().expect("log buffer poisoned");
        buffer.extend_from_slice(buf);
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
    _daemon_node_task: JoinHandle<daemon_node::Result<()>>,
    shared_messenger: Arc<TokioMutex<Messenger>>,
    daemon_state_path: PathBuf,
    daemon_node_name: String,
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

        let daemon_node = DaemonNode::new(
            Arc::clone(&shared_messenger),
            Some("test-daemon"),
            DaemonNodeArguments {
                node_startup_timeout: Duration::from_secs(120),
                node_start_health_timeout: Duration::from_secs(30),
            },
            temp_dir.path().to_path_buf(),
        );
        let daemon_node_name = daemon_node.node_name().to_string();

        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let daemon_node_task =
            tokio::spawn(async move { daemon_node.start_with_ready(Some(ready_tx)).await });
        ready_rx.await.expect("daemon node ready signal failed");

        let daemon_state = DaemonState::new(&daemon_node_name, port, "test-git-hash");
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: instance,
            _daemon_node_task: daemon_node_task,
            shared_messenger,
            daemon_state_path,
            daemon_node_name,
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

    pub fn daemon_node_name(&self) -> &str {
        &self.daemon_node_name
    }

    pub fn messaging_port(&self) -> u16 {
        self.messaging_port
    }
}
