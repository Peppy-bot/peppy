mod helpers;

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use helpers::TestServeHandle;
use peppy::commands::service::serve::{CancellationToken, ServeCommand};
use peppy::commands::Command;
use peppy::context::AppContext;
use peppy::error::Error;

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
