use peppy::test_support::{LogCapture, ServeCommandEmulation};
use std::sync::Arc;
use std::time::Duration;

use config::node::Toolchain;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeInitBuilder, NodeName};
use peppy::context::AppContext;

#[test]
fn node_cargo_init_command_success() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to start mock serve emulation");

    // Verify the daemon state
    assert!(
        !serve.master_node_name().is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_node";

    // Create a new AppContext pointing to the temp directory, using the shared messenger
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

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
            toolchain: Toolchain::Cargo,
            to_dir: None,
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
fn node_uv_init_command_success() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to start mock serve emulation");

    assert!(
        !serve.master_node_name().is_empty(),
        "master_node_name should not be empty"
    );

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_node";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), serve.messenger())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Use NodeInitBuilder directly with no timeout (signal-based waiting)
    // to avoid flakiness from wall-clock dependencies in CI.
    NodeInitBuilder::new(
        &node_ctx,
        NodeName::new(node_name).expect("valid node name"),
        Toolchain::Uv,
    )
    .with_timeout(None::<Duration>)
    .build()
    .expect("node create command should succeed");

    let created_node_dir = node_dir.path().join(node_name);
    assert!(
        created_node_dir.exists(),
        "node directory should exist at {}",
        created_node_dir.display()
    );

    assert!(
        created_node_dir.join("peppy.json5").exists(),
        "peppy.json5 should exist in the node directory"
    );

    assert!(
        created_node_dir.join("pyproject.toml").exists(),
        "pyproject.toml should exist in the node directory"
    );

    assert!(
        created_node_dir
            .join(format!("src/{node_name}/__init__.py"))
            .exists(),
        "src/{node_name}/__init__.py should exist in the node directory"
    );

    assert!(
        created_node_dir
            .join(format!("src/{node_name}/__main__.py"))
            .exists(),
        "src/{node_name}/__main__.py should exist in the node directory"
    );

    assert!(
        created_node_dir.join(".gitignore").exists(),
        ".gitignore should exist in the node directory"
    );

    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Successfully created node '{}'", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
}
