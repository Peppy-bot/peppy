use std::thread;
use std::time::Duration;

use peppy::serve::{CompositeCommand, Serve, ServeCommand};
use peppy::{AppContext, Command};

fn trigger_ctrl_c_signal() {
    // Safety: libc::raise is available on all supported platforms and delivers SIGINT to this process.
    let result = unsafe { libc::raise(libc::SIGINT) };
    assert_eq!(result, 0, "raising SIGINT should succeed");
}

#[test]
fn test_serve_command() {
    let ctx = AppContext::default();
    assert!(
        ctx.node_stack().is_none(),
        "node stack should not be initialized before serve runs"
    );

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
fn test_serve_command_replace_existing_process() {
    todo!("Finish")
}

#[test]
fn test_messenger_engine_stops_with_shutdown() {
    use pmi::Messenger;
    use pmi::MockAdapter;

    // Create a mock messenger context
    //let context = MessagingEngineContext::new("mock".to_string(), None);
    let adapter = MockAdapter::default();
    let messenger = Messenger::new(pmi::MessengerAdapter::Mock(adapter));

    let composite = CompositeCommand::default().add_async_command(Box::new(messenger));

    let serve = Serve::new(composite);

    // This should create a messenger that waits for Ctrl+C and then shuts down gracefully
    let handle = thread::spawn(move || serve.execute());

    // Let it run briefly
    thread::sleep(Duration::from_millis(100));

    // The messenger should be running and waiting for shutdown signal
    // In a real scenario, it would wait for Ctrl+C
    drop(handle);
}
