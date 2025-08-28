use peppy::commands::serve::CommandContext;
use peppy::commands::serve::messaging::{Messenger, MessengerBackend};

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn test_zenoh_router_lifecycle() {
    // Small delay to avoid port conflicts with other tests
    tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
    // Create a temporary config file with a random port to avoid conflicts
    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
    let config_path = temp_dir.path().join("test_zenoh_config.json5");

    // Use a random port based on thread ID and timestamp to avoid conflicts
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::time::{SystemTime, UNIX_EPOCH};

    let mut hasher = DefaultHasher::new();
    std::thread::current().id().hash(&mut hasher);
    let thread_hash = (hasher.finish() % 1000) as u16;
    let timestamp = (SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        % 1000) as u16;
    let port = 9000 + thread_hash + timestamp;
    let config_content = format!(
        r#"{{
            "mode": "router",
            "listen": {{
                "endpoints": {{
                    "router": ["tcp/127.0.0.1:{}"]
                }}
            }}
        }}"#,
        port
    );

    std::fs::write(&config_path, config_content).expect("Failed to write test config");

    let context = CommandContext::new("zenoh".to_string(), Some(config_path));
    let mut messenger = Messenger::new(context).expect("Failed to create messenger");

    // Start the router
    messenger.init().await.expect("Failed to start router");

    // Give it a moment to ensure it's fully initialized
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Shutdown the messaging system
    messenger.shutdown().await.expect("Failed to shutdown");
}
