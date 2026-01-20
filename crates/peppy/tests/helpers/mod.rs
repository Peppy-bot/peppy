#![allow(dead_code)]

use config::consts::{DAEMON_STATE_FILE_ENV, PEPPYGEN_OUTPUT_PATH};
use config::node::NodeConfigParser;
use peppy::commands::service::serve::{CancellationToken, ServeCommandBuilder};
use peppy::daemon_state::DaemonState;
use pmi::Messenger;
use std::ffi::OsStr;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;
use tracing_subscriber::fmt::MakeWriter;

pub fn override_start_cmd(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");
    // Avoid spawning a real node binary in tests, but keep the process alive long enough for
    // `node_start` to complete its `node_ready` + health check phases.
    cfg.manifest.start_cmd = vec!["sleep".to_string(), "5".to_string()];

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");

    // `node_init` generates a fingerprint during peppygen generation; keep it in sync.
    config::fingerprint::create_codegen_fingerprint(peppy_json5, Path::new(PEPPYGEN_OUTPUT_PATH));
}

#[derive(Clone, Default)]
pub struct LogCapture {
    buffer: Arc<Mutex<Vec<u8>>>,
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

pub fn serve_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

pub fn serve_test_guard() -> std::sync::MutexGuard<'static, ()> {
    serve_test_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub struct EnvVarGuard {
    key: &'static str,
}

impl EnvVarGuard {
    pub fn set(key: &'static str, value: impl AsRef<OsStr>) -> Self {
        unsafe { std::env::set_var(key, value) };
        Self { key }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        unsafe { std::env::remove_var(self.key) };
    }
}

pub struct TempServeEnvGuard {
    _dir: TempDir,
    _state_env: EnvVarGuard,
}

impl TempServeEnvGuard {
    pub fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir for serve env");
        let state_file = DaemonState::state_file_in(dir.path());
        let state_env = EnvVarGuard::set(DAEMON_STATE_FILE_ENV, state_file.as_os_str());
        Self {
            _dir: dir,
            _state_env: state_env,
        }
    }
}

/// A test helper that starts a serve command with mock messaging in the background.
/// Each instance is fully isolated and can run in parallel with other tests.
///
/// The serve command is automatically shut down when this handle is dropped.
pub struct TestServeHandle {
    _env_guard: TempServeEnvGuard,
    log_capture: LogCapture,
    messenger: Arc<TokioMutex<Messenger>>,
    shutdown_token: CancellationToken,
    serve_thread: Option<JoinHandle<()>>,
}

impl TestServeHandle {
    /// Creates a new test serve handle, starting the serve command in a background thread.
    /// Blocks until the serve command is initialized and ready to accept commands.
    /// Uses mock messaging - suitable for in-process tests only.
    pub fn with_mock_messenger() -> Self {
        Self::with_messaging_router("mock")
    }

    /// Creates a new test serve handle with real zenoh messaging.
    /// This allows spawned node processes to communicate with the master node.
    /// Each call uses a unique port to enable parallel test execution.
    pub fn with_zenoh() -> Self {
        Self::with_messaging_router("zenoh")
    }

    fn with_messaging_router(router: &str) -> Self {
        let env_guard = TempServeEnvGuard::new();

        let log_capture = LogCapture::new();
        let log_capture_for_serve = log_capture.clone();

        let shutdown_token = CancellationToken::new();
        let shutdown_token_for_serve = shutdown_token.clone();

        // Build the serve command with the specified messaging router
        let root_dir = std::env::current_dir().expect("failed to get current directory");
        let builder = ServeCommandBuilder::new(root_dir)
            .expect("builder should create")
            .with_shutdown_token(shutdown_token_for_serve)
            .with_messaging_router(router.to_string())
            .expect("messaging router should configure")
            .with_master_node(None)
            .expect("master node should configure");

        // Get the shared messenger before building
        let messenger = builder
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

        Self {
            _env_guard: env_guard,
            log_capture,
            messenger,
            shutdown_token,
            serve_thread: Some(serve_thread),
        }
    }

    /// Returns a clone of the shared messenger for use with AppContext.
    pub fn messenger(&self) -> Arc<TokioMutex<Messenger>> {
        Arc::clone(&self.messenger)
    }

    /// Returns the log capture for inspecting serve logs.
    pub fn log_capture(&self) -> &LogCapture {
        &self.log_capture
    }

    fn shutdown(&mut self) {
        self.shutdown_token.cancel();
        if let Some(thread) = self.serve_thread.take() {
            thread
                .join()
                .expect("serve thread should terminate after shutdown");
        }
    }
}

impl Drop for TestServeHandle {
    fn drop(&mut self) {
        self.shutdown();
    }
}
