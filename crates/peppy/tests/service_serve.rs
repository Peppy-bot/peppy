use std::sync::Arc;
use std::thread;
use std::time::Duration;

use peppy::commands::Command;
use peppy::commands::service::serve::CancellationToken;
use peppy::commands::service::serve::ServeCommand;
use peppy::context::AppContext;

#[test]
fn serve_command() {
    let ctx = Arc::new(AppContext::default());
    let log_capture = peppy::test_support::LogCapture::new();
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
        core_node_name: Some("core-node".to_string()),
        shutdown_token: Some(shutdown_token),
    }
    .execute(&ctx)
    .expect("serve command executes with mock messaging engine");

    shutdown_thread
        .join()
        .expect("shutdown thread should complete without panic");

    let core_node_name = ctx
        .core_node_name()
        .expect("daemon state core node name should be readable");
    assert_eq!(
        core_node_name, "core-node",
        "daemon state should use the configured core node name"
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
