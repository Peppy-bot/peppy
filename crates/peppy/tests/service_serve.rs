use std::sync::Arc;
use std::thread;
use std::time::Duration;

use daemon::state::DaemonState;
use daemon_config::consts::PeppyDirs;
use peppy::commands::Command;
use peppy::commands::service::ClockSource;
use peppy::commands::service::serve::CancellationToken;
use peppy::commands::service::serve::ServeCommand;
use peppy::context::AppContext;
use peppy::test_support::wait_for_log;

#[test]
fn serve_command() {
    // Root the daemon at a per-test temp dir: config resolution, the daemon
    // state file, and the singleton lock all live under it, so the test never
    // reads (or mutates) the real peppy home of the machine it runs on. The
    // context's daemon-state read is pointed at the same root.
    let temp_dir = tempfile::tempdir().expect("temp dir for the daemon root");
    let peppy_dirs = PeppyDirs::new(temp_dir.path());
    let ctx = Arc::new(
        AppContext::from_current_dir()
            .expect("current dir is readable")
            .with_daemon_state_file(DaemonState::state_file_in(temp_dir.path())),
    );
    let log_capture = peppy::test_support::LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    let shutdown_token = CancellationToken::new();
    let shutdown_token_clone = shutdown_token.clone();

    // Gate the shutdown on serve's observable readiness rather than a guessed
    // delay: cancel only once it has logged that it is initialized and is
    // waiting on the shutdown signal. The cancellation token is sticky, so even
    // if we cancel between the log line and the select loop, the signal is seen.
    let log_for_shutdown = log_capture.clone();
    let shutdown_thread = thread::spawn(move || {
        wait_for_log(
            || log_for_shutdown.logs(),
            "Serve command initialized!",
            Duration::from_secs(30),
        );
        shutdown_token_clone.cancel();
    });

    ServeCommand {
        messaging_engine: "mock".to_string(),
        core_node_name: Some("core-node".to_string()),
        clock_source: ClockSource::Wall,
        shutdown_token: Some(shutdown_token),
        peppy_dirs: peppy_dirs.clone(),
    }
    .execute(&ctx)
    .expect("serve command executes with mock messaging engine");

    assert!(
        peppy_dirs.conf_dir().join("peppy_config.json5").exists(),
        "the daemon should create its config under the injected temp root"
    );

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
    // This test drives shutdown via the injected external cancellation token
    // (not an OS signal), so the coordinator takes the external-shutdown arm.
    // The OS-signal arm logs "Shutdown signal received" instead; asserting that
    // here made the test pass only when a sibling test's process-wide SIGINT
    // happened to race in first.
    assert!(
        logs.contains("External shutdown requested"),
        "serve command should log external shutdown reception. Logs:\n{}",
        logs
    );
}
