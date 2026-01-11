#![allow(dead_code)]

use config::consts::DAEMON_STATE_FILE_ENV;
use peppy::commands::service::serve::{CancellationToken, ServeCommandBuilder};
use pmi::Messenger;
use pmi::zenohd_support::{reserve_free_tcp_port, write_zenohd_config};
use std::ffi::OsStr;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::sync::Mutex as TokioMutex;
use tracing_subscriber::fmt::MakeWriter;

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
        let state_file = dir.path().join("daemon_state.json");
        let state_env = EnvVarGuard::set(DAEMON_STATE_FILE_ENV, state_file.as_os_str());
        Self {
            _dir: dir,
            _state_env: state_env,
        }
    }
}

/// Guard that sets up a unique zenoh configuration for testing.
/// The config file is kept alive by the TempDir and the ZENOH_CONFIG env var is set.
pub struct ZenohConfigGuard {
    _dir: TempDir,
    _env: EnvVarGuard,
    pub config_path: PathBuf,
}

impl ZenohConfigGuard {
    /// Creates a new zenoh config with a unique port for parallel test isolation.
    pub fn new() -> Self {
        let host = "127.0.0.1";
        // Reserve a port to prevent parallel tests from getting the same port.
        // The reservation is released after writing the config, right before zenoh binds.
        let reservation = reserve_free_tcp_port();
        let port = reservation.port();
        let (temp_dir, config_path) =
            write_zenohd_config(host, port).expect("failed to write zenoh config");
        // Release the reservation now - zenoh will bind to this port next.
        drop(reservation);
        let env = EnvVarGuard::set("ZENOH_CONFIG", config_path.as_os_str());
        Self {
            _dir: temp_dir,
            _env: env,
            config_path,
        }
    }
}

/// A test helper that starts a serve command with mock messaging in the background.
/// Each instance is fully isolated and can run in parallel with other tests.
///
/// The serve command is automatically shut down when this handle is dropped.
pub struct TestServeHandle {
    _env_guard: TempServeEnvGuard,
    _zenoh_config_guard: Option<ZenohConfigGuard>,
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
        Self::with_messaging_router("mock", None)
    }

    /// Creates a new test serve handle with real zenoh messaging.
    /// This allows spawned node processes to communicate with the master node.
    /// Each call uses a unique port to enable parallel test execution.
    pub fn with_zenoh() -> Self {
        // Create a unique zenoh config with a free port before starting the serve command.
        // This sets ZENOH_CONFIG env var so spawned child processes can connect.
        let zenoh_config_guard = ZenohConfigGuard::new();
        Self::with_messaging_router("zenoh", Some(zenoh_config_guard))
    }

    fn with_messaging_router(router: &str, zenoh_config_guard: Option<ZenohConfigGuard>) -> Self {
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
            _zenoh_config_guard: zenoh_config_guard,
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
