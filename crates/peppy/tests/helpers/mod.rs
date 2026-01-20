#![allow(dead_code)]

use config::consts::PEPPYGEN_OUTPUT_PATH;
use config::node::NodeConfigParser;
use master_node::{MasterNode, MasterNodeArguments};
use peppy::daemon_state::DaemonState;
use pmi::{Messenger, MessengerBackend, MockAdapter, MockInstance, ZenohAdapter, ZenohdInstance};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread::{self};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;
use tokio::task::JoinHandle;
use tracing_subscriber::fmt::MakeWriter;

pub fn override_start_cmd(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    // Avoid spawning a real node binary in tests, but keep the process alive long enough for
    // `node_start` to complete its `node_ready` + health check phases.
    cfg.manifest.start_cmd = vec!["sleep".to_string(), "5".to_string()];
    // Avoid `add_cmd` build step (network access is not available in the test runner).
    cfg.manifest.add_cmd = None;

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");

    // `node_init` generates a fingerprint during peppygen generation; keep it in sync.
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
            buffer: self.buffer.clone(),
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

/// Holds either a MockInstance or ZenohdInstance for the test messenger setup.
enum MessengerInstance {
    Mock(MockInstance),
    Zenoh(ZenohdInstance),
}

/// Emulates a running `peppy serve` command for tests.
///
/// This struct:
/// - Creates a temporary directory for test isolation
/// - Starts either a MockAdapter or ZenohAdapter router
/// - Starts a session and provides a shared messenger
/// - Creates a DaemonState in the temp directory
///
/// # Example
///
/// ```ignore
/// let setup = ServeCommandEmulation::with_mock().await.unwrap();
/// let messenger = setup.messenger();
/// let daemon_state = setup.daemon_state();
/// ```
pub struct ServeCommandEmulation {
    _temp_dir: TempDir,
    _instance: MessengerInstance,
    _master_node_task: JoinHandle<master_node::Result<()>>,
    shared_messenger: Arc<TokioMutex<Messenger>>,
    daemon_state: DaemonState,
    daemon_state_path: PathBuf,
}

/// We cannot use the read `serve` command as it expect ton run on a particular port and have access to the global `peppy_data_dir()` where the `daemon_state.json` is stored
/// Both conditions are unwanted during testing
impl ServeCommandEmulation {
    /// Creates a test setup using MockAdapter.
    ///
    /// This is the recommended approach for most tests as it doesn't require
    /// any external processes.
    pub async fn with_mock() -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let temp_dir = TempDir::new().expect("failed to create temp dir for test");
        let daemon_state_path = DaemonState::state_file_in(temp_dir.path());

        let mut instance = MockAdapter::start_router().await?;
        instance.messenger().start_session().await?;
        let messenger = instance.take_messenger();
        let port = messenger.get_host().port();
        let shared_messenger = Arc::new(TokioMutex::new(messenger));

        // Start the master node to provide services (node_init, node_add, etc.)
        let master_node = MasterNode::new(
            Arc::clone(&shared_messenger),
            Some("test-master"),
            MasterNodeArguments {
                node_startup_timeout: Duration::from_secs(10),
                node_start_health_timeout: Duration::from_secs(30),
            },
            temp_dir.path().to_path_buf(),
        );
        let master_node_name = master_node.node_name().to_string();

        // Start master node with ready signal to ensure services are registered
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let master_node_task =
            tokio::spawn(async move { master_node.start_with_ready(Some(ready_tx)).await });
        ready_rx.await.expect("master node ready signal failed");

        // Create and write daemon state
        let daemon_state = DaemonState::new(&master_node_name, port, "test-git-hash");
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: MessengerInstance::Mock(instance),
            _master_node_task: master_node_task,
            shared_messenger,
            daemon_state,
            daemon_state_path,
        })
    }

    /// Creates a test setup using ZenohAdapter with an ephemeral port.
    ///
    /// Use this when you need to test real zenoh messaging behavior.
    pub async fn with_zenoh() -> Result<Self, pmi::PeppyMessagingInterfaceError> {
        let temp_dir = TempDir::new().expect("failed to create temp dir for test");
        let daemon_state_path = DaemonState::state_file_in(temp_dir.path());

        let mut instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None).await?;
        instance.messenger().start_session().await?;
        let actual_port = instance.port;
        let messenger = instance.take_messenger();
        let shared_messenger = Arc::new(TokioMutex::new(messenger));

        // Start the master node to provide services (node_init, node_add, etc.)
        let master_node = MasterNode::new(
            Arc::clone(&shared_messenger),
            Some("test-master"),
            MasterNodeArguments {
                node_startup_timeout: Duration::from_secs(10),
                node_start_health_timeout: Duration::from_secs(30),
            },
            temp_dir.path().to_path_buf(),
        );
        let master_node_name = master_node.node_name().to_string();

        // Start master node with ready signal to ensure services are registered
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let master_node_task =
            tokio::spawn(async move { master_node.start_with_ready(Some(ready_tx)).await });
        ready_rx.await.expect("master node ready signal failed");

        // Create and write daemon state
        let daemon_state = DaemonState::new(&master_node_name, actual_port, "test-git-hash");
        DaemonState::write_to(&daemon_state_path, &daemon_state)
            .expect("failed to write daemon state");

        Ok(Self {
            _temp_dir: temp_dir,
            _instance: MessengerInstance::Zenoh(instance),
            _master_node_task: master_node_task,
            shared_messenger,
            daemon_state,
            daemon_state_path,
        })
    }

    /// Returns a clone of the shared messenger wrapped in Arc<TokioMutex<_>>.
    pub fn messenger(&self) -> Arc<TokioMutex<Messenger>> {
        self.shared_messenger.clone()
    }

    /// Returns a reference to the DaemonState.
    pub fn daemon_state(&self) -> &DaemonState {
        &self.daemon_state
    }

    /// Returns the path to the temporary directory.
    pub fn temp_dir(&self) -> &Path {
        self._temp_dir.path()
    }

    /// Returns the path to the daemon state file.
    pub fn daemon_state_path(&self) -> &Path {
        &self.daemon_state_path
    }
}
