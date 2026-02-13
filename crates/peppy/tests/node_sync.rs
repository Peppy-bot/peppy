use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::path::Path;
use std::sync::Arc;

use config::node::{ExposedTopic, MessageFormat, NodeConfigParser, QoSProfile, Toolchain};
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;

fn add_exposed_topic(peppy_json5: &Path) {
    let mut cfg = NodeConfigParser::from_path(peppy_json5).expect("peppy.json5 should read");

    let exposes = cfg.interfaces.exposes.get_or_insert_with(Default::default);
    let topics = exposes.topics.get_or_insert_with(Vec::new);
    let message_format: MessageFormat = serde_json::from_value(serde_json::json!({
        "timestamp": "time",
        "message": "string",
    }))
    .expect("message format should deserialize");
    topics.push(ExposedTopic {
        name: "goodbye_world".to_string(),
        qos_profile: QoSProfile::Standard,
        message_format: Some(message_format),
    });

    // Write JSON (valid JSON5) back to disk.
    let updated_content = serde_json::to_string_pretty(&cfg).expect("peppy.json5 should serialize");
    std::fs::write(peppy_json5, updated_content).expect("peppy.json5 should update");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_sync_rust_command_succeeds() {
    let serve = ServeCommandEmulation::with_mock()
        .await
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    assert!(
        !serve.daemon_node_name().is_empty(),
        "daemon_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    // Set up logging
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    let old_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("fingerprint should be readable");
    assert!(
        !old_fingerprint.is_empty(),
        "fingerprint should not be empty"
    );

    // Change peppy.json5 to force a new fingerprint.
    // This simulates a developer changing their node interface definitions and needing to re-sync.
    add_exposed_topic(&peppy_json5_path);

    let expected_fingerprint =
        config::runtime::RuntimeConfig::generate_peppy_config_fingerprint(&peppy_json5_path)
            .expect("peppy.json5 fingerprint should generate");
    assert_ne!(
        expected_fingerprint, old_fingerprint,
        "fingerprint should change after modifying peppy.json5"
    );

    // Run sync from inside the node directory (ctx.root_dir must contain peppy.json5)
    let sync_ctx = Arc::new(
        AppContext::with_messenger(&node_path, shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );
    NodeCommand {
        command: NodeCommands::Sync {},
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    let new_fingerprint = config::fingerprint::read_codegen_fingerprint(
        &peppy_json5_path,
        config::consts::PEPPYGEN_OUTPUT_PATH,
    )
    .expect("updated fingerprint should be readable");
    assert_eq!(
        new_fingerprint, expected_fingerprint,
        "fingerprint file should be updated by sync"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains("Synced node interfaces successfully"),
        "logs should contain sync success message. Logs:\n{}",
        logs
    );

    // Verify that the `goodbye_world` topic code was generated
    let goodbye_world_topic_path = node_path
        .join(config::consts::PEPPYGEN_OUTPUT_PATH)
        .join("src/exposed_topics/goodbye_world.rs");
    assert!(
        goodbye_world_topic_path.exists(),
        "goodbye_world topic should be generated at {}",
        goodbye_world_topic_path.display()
    );

    // Verify the generated file has expected content
    let goodbye_world_contents = std::fs::read_to_string(&goodbye_world_topic_path)
        .expect("goodbye_world.rs should be readable");
    assert!(
        goodbye_world_contents.contains("goodbye_world"),
        "goodbye_world.rs should contain topic name"
    );
}
