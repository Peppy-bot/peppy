use std::ffi::OsStr;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use peppy::serve::{CancellationToken, DAEMON_STATE_FILE_ENV, PID_FILE_ENV, ServeCommandBuilder};
use pmi::Messenger;
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
    _pid_env: EnvVarGuard,
    _state_env: EnvVarGuard,
}

impl TempServeEnvGuard {
    pub fn new() -> Self {
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
    pub fn new() -> Self {
        let env_guard = TempServeEnvGuard::new();

        let log_capture = LogCapture::new();
        let log_capture_for_serve = log_capture.clone();

        let shutdown_token = CancellationToken::new();
        let shutdown_token_for_serve = shutdown_token.clone();

        // Build the serve command with mock messaging router
        let builder = ServeCommandBuilder::new()
            .expect("builder should create")
            .with_shutdown_token(shutdown_token_for_serve)
            .with_messaging_router("mock".to_string())
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
