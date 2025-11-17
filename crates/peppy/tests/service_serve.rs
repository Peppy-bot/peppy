use std::io::Write;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use peppy::serve::{CompositeCommand, Serve, ServeCommand};
use peppy::{AppContext, Command};
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
fn test_serve_command_replace_existing_process() {
    todo!("Finish")
}
