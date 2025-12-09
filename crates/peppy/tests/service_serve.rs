use std::ffi::OsStr;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use peppy::serve::{CancellationToken, PID_FILE_ENV, PROMPT_ANSWER_ENV, ServeCommand};
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

struct TempPidFileGuard {
    _dir: TempDir,
    _env: EnvVarGuard,
}

impl TempPidFileGuard {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("failed to create temp dir for pid file");
        let pid_file = dir.path().join("peppy.pid");
        let env_guard = EnvVarGuard::set(PID_FILE_ENV, pid_file.as_os_str());
        Self {
            _dir: dir,
            _env: env_guard,
        }
    }
}

fn serve_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

#[test]
fn test_serve_command() {
    let _serial_guard = serve_test_lock().lock().unwrap();
    let _pid_guard = TempPidFileGuard::new();

    let ctx = Arc::new(AppContext::default());
    assert!(
        ctx.node_stack().is_none(),
        "node stack should not be initialized before serve runs"
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let shutdown_token = CancellationToken::new();
    let shutdown_token_clone = shutdown_token.clone();

    let shutdown_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(200));
        shutdown_token_clone.cancel();
    });

    ServeCommand {
        messaging_engine: "mock".to_string(),
        master_name: Some("master-node".to_string()),
        shutdown_token: Some(shutdown_token),
    }
    .execute(&ctx)
    .expect("serve command executes with mock messaging engine");

    shutdown_thread
        .join()
        .expect("shutdown thread should complete without panic");

    let node_stack = ctx
        .node_stack()
        .expect("node stack should be initialized by ServeCommand");
    assert!(
        node_stack.contains("master-node", "internal"),
        "node stack should register the master node"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Serve command initialized!"),
        "serve command should log initialization message. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Shutdown signal received"),
        "serve command should log shutdown signal reception. Logs:\n{}",
        logs
    );
}

#[test]
fn test_serve_command_replace_existing_stack() {
    let _serial_guard = serve_test_lock().lock().unwrap();
    let _pid_guard = TempPidFileGuard::new();

    let log_capture = LogCapture::new();
    let log_capture_for_thread = log_capture.clone();

    let shutdown_token = CancellationToken::new();
    let shutdown_token_for_thread = shutdown_token.clone();

    let ctx = Arc::new(AppContext::default());
    let ctx_for_thread = ctx.clone();
    let serve_thread = thread::spawn(move || {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(log_capture_for_thread)
            .finish();
        let _guard = tracing::subscriber::set_default(subscriber);

        ServeCommand {
            messaging_engine: "mock".to_string(),
            master_name: Some("master-node".to_string()),
            shutdown_token: Some(shutdown_token_for_thread),
        }
        .execute(&ctx_for_thread)
        .expect("initial serve command should start");
    });

    wait_for_log(
        &log_capture,
        "Serve command initialized!",
        Duration::from_secs(5),
    );

    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let second_attempt = {
        let _prompt_guard = EnvVarGuard::set(PROMPT_ANSWER_ENV, "y");
        ServeCommand {
            messaging_engine: "mock".to_string(),
            master_name: Some("master-node".to_string()),
            shutdown_token: None,
        }
        .execute(&ctx)
    };

    assert!(
        second_attempt.is_err(),
        "second serve command should fail while another instance is active"
    );

    shutdown_token.cancel();
    serve_thread
        .join()
        .expect("serve thread should terminate after shutdown");

    let logs = log_capture.logs();
    assert!(
        logs.contains("Existing peppy instance detected"),
        "existing instance detection should be logged. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("reset_requested=true"),
        "user response should be registered in logs. Logs:\n{}",
        logs
    );
    todo!("The commands_listener should receive a message to reset the stack")
}
