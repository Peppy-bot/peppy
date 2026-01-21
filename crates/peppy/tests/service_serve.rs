mod helpers;

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

    let daemon_state = ctx
        .read_daemon_state()
        .expect("daemon state should be readable");
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
