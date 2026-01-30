use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::info::InfoCommand;
use peppy::context::AppContext;
use peppy::test_support::ServeCommandEmulation;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn info_command_succeeds_when_daemon_running() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to start serve emulation");

    let ctx = Arc::new(
        AppContext::with_messenger(serve.temp_dir(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = peppy::test_support::LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    InfoCommand
        .execute(&ctx)
        .expect("info command should succeed when daemon is running");
}

#[test]
fn info_command_fails_without_daemon() {
    let temp_dir = tempfile::tempdir().expect("failed to create temp dir");
    let nonexistent_state = temp_dir.path().join("nonexistent_state.json");

    let ctx = Arc::new(AppContext::default().with_daemon_state_file(&nonexistent_state));

    let result = InfoCommand.execute(&ctx);

    assert!(
        result.is_err(),
        "info command should fail when daemon state file doesn't exist"
    );

    let err = result.unwrap_err();
    let err_msg = err.to_string();
    assert!(
        err_msg.contains("daemon") || err_msg.contains("state"),
        "error message should mention daemon state issue. Got: {}",
        err_msg
    );
}
