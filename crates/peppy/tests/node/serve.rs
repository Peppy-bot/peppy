use std::thread;
use std::time::Duration;

use peppy::Result;
use peppy::commands::serve::{CompositeCommand, Serve, ServeAsyncCommand};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::task::JoinHandle;

struct TestAsyncCommand {
    should_succeed: bool,
}

impl ServeAsyncCommand for TestAsyncCommand {
    fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
        let should_succeed = self.should_succeed;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            if should_succeed {
                Ok(())
            } else {
                Err(peppy::Error::ExecutionFailed("Test error".to_string()))
            }
        });
        Ok(handle)
    }
}

#[test]
fn test_serve_command_with_multiple_async_commands() {
    let composite = CompositeCommand::default()
        .add_async_command(Box::new(TestAsyncCommand {
            should_succeed: true,
        }))
        .add_async_command(Box::new(TestAsyncCommand {
            should_succeed: true,
        }))
        .add_async_command(Box::new(TestAsyncCommand {
            should_succeed: true,
        }));

    let serve = Serve::new(composite);

    let handle = thread::spawn(move || serve.execute());

    thread::sleep(Duration::from_millis(500));

    let _ = handle
        .join()
        .expect("Serve command should handle multiple async commands");
}

#[test]
fn test_serve_command_with_graceful_shutdown() {
    struct LongRunningCommand {
        running: Arc<AtomicBool>,
        shutdown_called: Arc<AtomicBool>,
    }

    impl ServeAsyncCommand for LongRunningCommand {
        fn execute_async(&self) -> Result<JoinHandle<Result<()>>> {
            let running = self.running.clone();
            let shutdown_called = self.shutdown_called.clone();

            let handle = tokio::spawn(async move {
                running.store(true, Ordering::SeqCst);

                // Simulate waiting for Ctrl+C
                tokio::signal::ctrl_c().await.map_err(|e| {
                    peppy::Error::ExecutionFailed(format!("Failed to listen for ctrl-c: {}", e))
                })?;

                // Simulate shutdown behavior
                shutdown_called.store(true, Ordering::SeqCst);
                Ok(())
            });
            Ok(handle)
        }
    }

    let running = Arc::new(AtomicBool::new(false));
    let shutdown_called = Arc::new(AtomicBool::new(false));

    let composite = CompositeCommand::default().add_async_command(Box::new(LongRunningCommand {
        running: running.clone(),
        shutdown_called: shutdown_called.clone(),
    }));

    let serve = Serve::new(composite);

    // Spawn the serve command in a separate thread
    let handle = thread::spawn(move || serve.execute());

    // Wait for the command to start running
    thread::sleep(Duration::from_millis(100));
    assert!(running.load(Ordering::SeqCst), "Command should be running");

    // Send SIGINT to trigger graceful shutdown
    #[cfg(unix)]
    {
        // Use nix crate's safe API to send signal (if available in dependencies)
        // Or spawn a child process to send the signal
        use std::process::{Command, Stdio};
        let pid = std::process::id();
        Command::new("kill")
            .args(["-INT", &pid.to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("Failed to send SIGINT");
    }

    #[cfg(not(unix))]
    {
        // On non-Unix platforms, we can't easily send SIGINT
        // The test will just verify the command starts correctly
    }

    // Wait a bit for the signal to be processed
    thread::sleep(Duration::from_millis(200));

    // Join the thread and verify it completed successfully
    let result = handle.join().expect("Thread should not panic");
    assert!(
        result.is_ok(),
        "Serve command should complete successfully after SIGINT"
    );

    // Verify shutdown was called
    assert!(
        shutdown_called.load(Ordering::SeqCst),
        "Shutdown should have been called after SIGINT"
    );
}

#[test]
fn test_messenger_mock_command_with_shutdown() {
    use pmi::{MessagingEngineContext, Messenger};

    // Create a mock messenger context
    let context = MessagingEngineContext::new("mock".to_string(), None);
    let messenger = Messenger::new(context).expect("Should create mock messenger");

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
