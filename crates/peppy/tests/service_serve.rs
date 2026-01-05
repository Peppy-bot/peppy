mod helpers;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use helpers::TestServeHandle;
use peppy::commands::Command;
use peppy::commands::service::serve::CancellationToken;
use peppy::commands::service::serve::ServeCommand;
use peppy::context::{AppContext, DaemonState};
use peppy::error::Error;

#[test]
fn serve_command() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let _serve_env = helpers::TempServeEnvGuard::new();

    let ctx = Arc::new(AppContext::default());
    let log_capture = helpers::LogCapture::new();
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

    let daemon_state = DaemonState::read().expect("daemon state should be readable");
    assert_eq!(
        daemon_state.master_node_name, "master-node",
        "daemon state should use the configured master name"
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
fn serve_command_fails_when_zenoh_port_already_in_use() {
    let _serial_guard = helpers::serve_test_lock().lock().unwrap();
    let _serve = TestServeHandle::with_zenoh();

    let ctx = Arc::new(AppContext::default());

    // Best-effort hang protection in case the second serve unexpectedly starts.
    let shutdown_token = CancellationToken::new();
    let shutdown_token_for_cancel = shutdown_token.clone();
    let cancel_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(500));
        shutdown_token_for_cancel.cancel();
    });

    let err = ServeCommand {
        messaging_engine: "zenoh".to_string(),
        master_name: Some("master-node-2".to_string()),
        shutdown_token: Some(shutdown_token),
    }
    .execute(&ctx)
    .expect_err("second serve should fail when zenoh port is already in use");

    cancel_thread
        .join()
        .expect("cancel thread should complete without panic");

    match err {
        Error::PeppyMessagingInterface(err) => {
            let msg = format!("{err:?}").to_lowercase();
            assert!(
                msg.contains("already in use") || msg.contains("eaddrinuse"),
                "expected port-in-use error, got: {err:?}"
            );
        }
        other => panic!("expected PeppyMessagingInterface error, got: {other:?}"),
    }
}
