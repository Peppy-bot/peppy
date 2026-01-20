mod helpers;

use helpers::{LogCapture, ServeCommandEmulation};
use std::sync::Arc;

use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;

#[test]
fn node_rust_init_command_success() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to start mock serve emulation");

    // Verify the daemon state
    let daemon_state = serve.daemon_state();
    assert!(
        !daemon_state.master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_node";

    // Create a new AppContext pointing to the temp directory, using the shared messenger
    let node_ctx = Arc::new(AppContext::with_messenger(
        node_dir.path(),
        serve.messenger(),
    ));

    // Set up logging for the node command
    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Execute the node create command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            build_system: config::peppy_config::BuildSystem::Rust,
        },
    }
    .execute(&node_ctx)
    .expect("node create command should succeed");

    // Verify the node directory was created
    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.exists(),
        "node directory should exist at {}",
        created_node_dir.display()
    );

    // Verify peppy.json5 was created
    assert!(
        created_node_dir.join("peppy.json5").exists(),
        "peppy.json5 should exist in the node directory"
    );

    // Verify Cargo.toml was created (for Rust build system)
    assert!(
        created_node_dir.join("Cargo.toml").exists(),
        "Cargo.toml should exist in the node directory"
    );

    // Verify src/main.rs was created
    assert!(
        created_node_dir.join("src/main.rs").exists(),
        "src/main.rs should exist in the node directory"
    );

    // Verify .gitignore was created
    assert!(
        created_node_dir.join(".gitignore").exists(),
        ".gitignore should exist in the node directory"
    );

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Successfully created node '{}'", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
}

#[test]
#[ignore = "Python generator not yet implemented"]
fn node_python_init_command_success() {}
