use std::ffi::OsStr;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use peppy::node::{NodeCommand, NodeCommands, NodeName};
use peppy::serve::{
    CancellationToken, DAEMON_STATE_FILE_ENV, DaemonState, PID_FILE_ENV, ServeCommandBuilder,
};
use peppy::{AppContext, Command};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

#[derive(Clone, Default)]
struct LogCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl LogCapture {
    fn new() -> Self {
        Self::default()
    }

    fn logs(&self) -> String {
        let buffer = self.buffer.lock().expect("log buffer poisoned");
        String::from_utf8(buffer.clone()).expect("captured logs are valid UTF-8")
    }
}

struct LogCaptureWriter {
    buffer: Arc<Mutex<Vec<u8>>>,
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

fn wait_for_log(log_capture: &LogCapture, needle: &str, timeout: Duration) {
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

struct EnvVarGuard {
    key: &'static str,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        unsafe { std::env::set_var(key, value) };
        Self { key }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(self.key) };
    }
}

struct TempServeEnvGuard {
    _dir: TempDir,
    _pid_env: EnvVarGuard,
    _state_env: EnvVarGuard,
}

impl TempServeEnvGuard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir for serve env");
        let pid_file = dir.path().join("peppy.pid");
        let state_file = dir.path().join("daemon_state.json");
        let pid_env = EnvVarGuard::set(PID_FILE_ENV, pid_file.as_os_str());
        let state_env = EnvVarGuard::set(DAEMON_STATE_FILE_ENV, state_file.as_os_str());
        Self {
            _dir: dir,
            _pid_env: pid_env,
            _state_env: state_env,
        }
    }
}

fn node_create_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn node_create_command() {
    let _serial_guard = node_create_test_lock().lock().unwrap();
    let _env_guard = TempServeEnvGuard::new();

    let log_capture = LogCapture::new();
    let log_capture_for_serve = log_capture.clone();

    let shutdown_token = CancellationToken::new();
    let shutdown_token_for_serve = shutdown_token.clone();

    // Build the serve command manually to get access to the shared messenger
    let builder = ServeCommandBuilder::new()
        .expect("builder should create")
        .with_shutdown_token(shutdown_token_for_serve)
        .with_messaging_router("mock".to_string())
        .with_master_node(None) // Use random name to test daemon state discovery
        .expect("master node should configure");

    // Get the shared messenger before building
    let shared_messenger = builder
        .messenger()
        .expect("messenger should be available after configuring messaging router");

    // Build and run the serve command
    let serve = builder.build().expect("serve command should build");

    let serve_thread = thread::spawn(move || {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(log_capture_for_serve)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        serve
            .execute()
            .expect("serve command should start successfully");
    });

    // Wait for the serve command to initialize
    wait_for_log(
        &log_capture,
        "Serve command initialized!",
        Duration::from_secs(5),
    );

    // Verify the daemon state file was written
    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    assert!(
        !daemon_state.master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_node";

    // Create a new AppContext pointing to the temp directory, using the shared messenger
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        shared_messenger,
    ));

    // Set up logging for the node command
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Execute the node create command
    let result = NodeCommand {
        command: NodeCommands::Create {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx);

    // Shutdown the serve command
    shutdown_token.cancel();
    serve_thread
        .join()
        .expect("serve thread should terminate after shutdown");

    // Assert the node was created successfully
    result.expect("node create command should succeed");

    // Verify the node directory was created
    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.exists(),
        "node directory should exist at {}",
        created_node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        created_node_dir.join("peppy.json5").exists(),
        "peppy.json5 should exist in the node directory"
    );

    // Verify Cargo.toml was created (for Rust build system)
    assert!(
        created_node_dir.join("Cargo.toml").exists(),
        "Cargo.toml should exist in the node directory"
    );

    // Verify src/main.rs was created
    assert!(
        created_node_dir.join("src/main.rs").exists(),
        "src/main.rs should exist in the node directory"
    );

    // Verify .gitignore was created
    assert!(
        created_node_dir.join(".gitignore").exists(),
        ".gitignore should exist in the node directory"
    );

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Successfully created node '{}'", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
}
