use std::ffi::OsStr;
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use peppy::serve::{PID_FILE_ENV, PROMPT_ANSWER_ENV, ServeCommand};
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

fn trigger_ctrl_c_signal() {
    // Safety: libc::raise is available on all supported platforms and delivers SIGINT to this process.
    let result = unsafe { libc::raise(libc::SIGINT) };
    assert_eq!(result, 0, "raising SIGINT should succeed");
}

#[test]
fn manual_ctrl_c_works() {
    if std::env::var("PEPPY_SERVE_TEST_CHILD").is_ok() {
        return;
    }

    let _serial_guard = serve_test_lock().lock().unwrap();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    let signal_thread = thread::spawn(|| {
        thread::sleep(Duration::from_millis(100));
        trigger_ctrl_c_signal();
    });

    runtime.block_on(async {
        tokio::signal::ctrl_c().await.unwrap();
    });

    signal_thread.join().unwrap();
}

#[test]
fn test_serve_command() {
    let _serial_guard = serve_test_lock().lock().unwrap();
    let _pid_guard = TempPidFileGuard::new();

    let ctx = AppContext::default();
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

    let signal_thread = thread::spawn(|| {
        thread::sleep(Duration::from_millis(200));
        trigger_ctrl_c_signal();
    });

    ServeCommand {
        messaging_engine: "mock".to_string(),
    }
    .execute(&ctx)
    .expect("serve command executes with mock messaging engine");

    signal_thread
        .join()
        .expect("signal thread should complete without panic");

    assert!(
        ctx.node_stack().is_some(),
        "node stack should be initialized by ServeCommand"
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
    // TODO use random port for Zenoh
    let _serial_guard = serve_test_lock().lock().unwrap();
    let _pid_guard = TempPidFileGuard::new();

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let ctx = Arc::new(AppContext::default());
    let ctx_for_thread = ctx.clone();
    let serve_thread = thread::spawn(move || {
        ServeCommand {
            messaging_engine: "zenoh".to_string(),
        }
        .execute(&ctx_for_thread)
        .expect("initial serve command should start");
    });

    wait_for_log(
        &log_capture,
        "Serve command initialized!",
        Duration::from_secs(5),
    );

    let second_attempt = {
        let _prompt_guard = EnvVarGuard::set(PROMPT_ANSWER_ENV, "y");
        ServeCommand {
            messaging_engine: "zenoh".to_string(),
        }
        .execute(&ctx)
    };

    assert!(
        second_attempt.is_err(),
        "second serve command should fail while another instance is active"
    );

    wait_for_log(
        &log_capture,
        "Existing peppy instance detected",
        Duration::from_secs(2),
    );

    trigger_ctrl_c_signal();
    serve_thread
        .join()
        .expect("serve thread should terminate after ctrl-c");

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
