use config::consts::DAEMON_STATE_FILE_ENV;
use peppy::commands::service::serve::{CancellationToken, ServeCommandBuilder};
use pmi::zenohd_support::{reserve_free_tcp_port, write_zenohd_config};
use std::ffi::OsStr;
use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tracing_subscriber::fmt::MakeWriter;

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
    _state_env: EnvVarGuard,
}

impl TempServeEnvGuard {
    fn new() -> Self {
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
struct ZenohConfigGuard {
    _dir: TempDir,
    _env: EnvVarGuard,
    pub port: u16,
}

impl ZenohConfigGuard {
    fn new() -> Self {
        let host = "127.0.0.1";
        let reservation = reserve_free_tcp_port();
        let port = reservation.port();
        let (temp_dir, config_path) =
            write_zenohd_config(host, port).expect("failed to write zenoh config");
        drop(reservation);
        let env = EnvVarGuard::set("ZENOH_CONFIG", config_path.as_os_str());
        Self {
            _dir: temp_dir,
            _env: env,
            port,
        }
    }
}

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

/// A test helper that starts the peppy service in the background on a random port.
///
/// The serve command is automatically shut down when this handle is dropped.
pub struct TestServeHandle {
    _env_guard: TempServeEnvGuard,
    _zenoh_config_guard: ZenohConfigGuard,
    shutdown_token: CancellationToken,
    serve_thread: Option<JoinHandle<()>>,
    port: u16,
}

impl TestServeHandle {
    /// Starts `peppy service serve` on a random port in the background.
    ///
    /// Blocks until the serve command is initialized and ready to accept commands.
    /// The service is automatically shut down when this handle is dropped.
    pub fn start() -> Self {
        let env_guard = TempServeEnvGuard::new();
        let zenoh_config_guard = ZenohConfigGuard::new();
        let port = zenoh_config_guard.port;

        let log_capture = LogCapture::new();
        let log_capture_for_serve = log_capture.clone();

        let shutdown_token = CancellationToken::new();
        let shutdown_token_for_serve = shutdown_token.clone();

        let root_dir = std::env::current_dir().expect("failed to get current directory");
        let serve = ServeCommandBuilder::new(root_dir)
            .expect("builder should create")
            .with_shutdown_token(shutdown_token_for_serve)
            .with_messaging_router("zenoh".to_string())
            .with_master_node(None)
            .expect("master node should configure")
            .build()
            .expect("serve command should build");

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

        wait_for_log(
            &log_capture,
            "Serve command initialized!",
            Duration::from_secs(30),
        );

        Self {
            _env_guard: env_guard,
            _zenoh_config_guard: zenoh_config_guard,
            shutdown_token,
            serve_thread: Some(serve_thread),
            port,
        }
    }

    /// Returns the port the service is listening on.
    pub fn port(&self) -> u16 {
        self.port
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
