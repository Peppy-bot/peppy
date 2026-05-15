use config::consts::{PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use config::node::Toolchain;
use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;
use peppy::commands::Command;
use peppy::commands::node::{
    AddNodeParams, NodeCommand, NodeCommands, NodeName, TimeoutConfig, add_node,
};
use peppy::context::AppContext;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

use peppylib::core_node::transport::poll_stack_list;
const CALLER_INSTANCE_ID: &str = "peppy-test";

#[test]
fn node_add_command_succeeds() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the node directory
    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    peppy::test_support::disable_build_cmd(&peppy_json5_path);

    // Now add the node to the node stack
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed");

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );

    // Regression: a first-time `node add` runs a `NodeInfoRequest` preflight
    // to check for existing instances before overwriting. When the node
    // isn't in the stack (the happy-path case here), that lookup used to
    // surface as a daemon-side ERROR log from `run_handler` even though the
    // CLI intentionally swallows the rejection. Assert that no such spurious
    // service-handler error is emitted during a clean add.
    assert!(
        !logs.contains("service handler returned error"),
        "first-time node add should not emit a service-handler error log. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node was added
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph with 0 instances (since run=false)
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        added_node.instance_count(),
        0,
        "graph should show 0 instances for the added node. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn node_add_command_with_run_arg_succeeds() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_run_node";
    let instance_id = "test_add_run_instance";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the node directory
    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Avoid spawning a real node binary; provide `node_ready` + `node_health` in-process.
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");

    // Add the node to the node stack with run=true to also start an instance
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command with run should succeed");

    // Verify the logs contain success messages for both add and start
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );
    assert!(
        logs.contains("Started node instance"),
        "logs should contain success message for starting node instance. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node was added with an instance
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph with 1 instance (since run=true)
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        added_node.instance_count(),
        1,
        "graph should show 1 instance for the added node. Got: {:?}",
        graph
            .nodes
            .iter()
            .map(|n| (n.label(), n.instance_count()))
            .collect::<Vec<_>>()
    );
}

#[test]
fn node_add_after_failed_sync_succeeds() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_then_add_node";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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

    // 1. Create a node with `node init`
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the node directory
    let node_path = node_dir.path().join(node_name);
    let peppy_dir = node_path.join(PEPPY_OUTPUT_DIR);
    let git_hash_path = peppy_dir.join("git.hash");

    // 2. Modify the `git.hash` in that node to invalidate it
    assert!(
        git_hash_path.exists(),
        "git.hash should exist at {}",
        git_hash_path.display()
    );
    std::fs::write(&git_hash_path, "wrong-hash\n").expect("failed to write wrong git hash");

    // Disable build_cmd to avoid build step
    let peppy_json5_path = node_path.join("peppy.json5");
    peppy::test_support::disable_build_cmd(&peppy_json5_path);

    // 3. Run `node add .` on that node, it'll fail due to git hash mismatch
    let add_result = NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx);

    assert!(
        add_result.is_err(),
        "node add should fail due to git hash mismatch"
    );
    let error_message = add_result.unwrap_err().to_string();
    assert!(
        error_message.contains("git hash mismatch"),
        "error should mention git hash mismatch, got: {}",
        error_message
    );

    // 4. Run `node sync` on that node to update the `.peppy/git.hash` of the node
    // Create a context for the node directory (sync runs from within the node dir)
    let sync_ctx = Arc::new(
        AppContext::with_messenger(&node_path, Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    NodeCommand {
        command: NodeCommands::Sync {
            path: None,
            include_repositories: false,
        },
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    // 5. Run `node add .` again. This time it should succeed
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add command should succeed after sync");

    // Verify the logs contain success message
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );

    // Query the node stack to verify the node was added
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the added node. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        added_node.instance_count(),
        0,
        "graph should show 0 instances for the added node"
    );
}

/// When adding a node that already exists with running instances:
/// - With `force: true`: instances are automatically stopped by the core node and the node is overwritten
/// - Without `force`: user is prompted for confirmation (tested manually, not in this automated test)
///
/// This test verifies the `force: true` path where existing instances are shut down automatically.
#[test]
fn node_add_same_node_shutdown_existing_instances() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    // Mock messaging is sufficient: we run in-process node services for health/ready.
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();
    assert!(
        !core_node_name.is_empty(),
        "core_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_shutdown_node";
    let instance_id = "test_shutdown_instance";

    // Create AppContext pointing to the temp directory
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
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

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    // Get the path to the node directory
    let node_path = node_dir.path().join(node_name);
    let peppy_json5_path = node_path.join("peppy.json5");
    assert!(
        peppy_json5_path.exists(),
        "peppy.json5 should exist at {}",
        peppy_json5_path.display()
    );

    // Avoid spawning a real node binary; provide `node_ready` + `node_health` in-process.
    peppy::test_support::override_run_cmd(&peppy_json5_path);

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");
    let (_node_shutdown_handle, _shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    // Step 1: Add the node with start=true to create an instance
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("first node add command should succeed");

    // Verify we have 1 instance running
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    let node_before = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the node after first add. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node_before.instance_count(),
        1,
        "should have 1 instance running after first add"
    );
    assert!(
        node_before.running_instance_ids().contains(&instance_id),
        "instance ID should match"
    );

    // Step 2: Add the same node again with force=true
    // This should shut down the existing instance and overwrite the node
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false, // Don't run a new instance this time
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: true, // Bypass confirmation prompt
        },
    }
    .execute(&node_ctx)
    .expect("second node add command with force should succeed");

    // Verify the instance was stopped and node was re-added with 0 instances
    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete after re-add");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    let node_after = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the node after re-add. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node_after.instance_count(),
        0,
        "should have 0 instances after re-add with force (instance should be stopped)"
    );

    // Verify the logs show the node was added successfully
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message for adding node. Logs:\n{}",
        logs
    );
}
#[test]
fn node_add_same_node_different_sources_show_overwrite_prompt() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_overwrite_git_source";
    let instance_id = "test_overwrite_git_instance";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // Init the node
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
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

    peppy::test_support::override_run_cmd(&peppy_json5_path);

    let node_messenger = MessengerHandle::from_shared(Arc::clone(&shared_messenger));
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");
    let (_node_shutdown_handle, _shutdown_rx) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &core_node_name,
            instance_id,
            node_name,
        ))
        .expect("node shutdown service should start");

    // Step 1: Add the node from local filesystem with start=true
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("first node add from local filesystem should succeed");

    // Step 2: Verify 1 instance running
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    let node_before = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the node after first add. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node_before.instance_count(),
        1,
        "should have 1 instance running after first add from local filesystem"
    );

    // Step 3: Create a git repo containing the same node (same name:tag)
    let git_dir = tempfile::tempdir().expect("failed to create temp dir for git repo");
    let repo_dir = git_dir.path().join("repo");
    let git_node_dir = repo_dir.join(node_name);
    std::fs::create_dir_all(&git_node_dir).expect("should create node dir in git repo");
    std::fs::copy(&peppy_json5_path, git_node_dir.join("peppy.json5"))
        .expect("should copy peppy.json5 to git repo");

    let run_git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(&repo_dir)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git command should run");
        assert!(
            output.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_git(&["init"]);
    run_git(&["add", "."]);
    run_git(&[
        "-c",
        "user.name=Test",
        "-c",
        "user.email=test@test.com",
        "commit",
        "-m",
        "add test node",
    ]);

    // Step 4: Re-add the same node from the git source with force=false.
    // The preflight fetch_node_info clones the git repo, reads peppy.json5,
    // discovers the same node name:tag, and finds the running instance.
    // confirm_overwrite then reads "y\n" from the cursor to approve.
    let git_source = format!("file://{}/.git/{}", repo_dir.display(), node_name);

    add_node(
        &node_ctx,
        AddNodeParams {
            source: git_source,
            git_ref: None,
            run_options: None,
            timeouts: TimeoutConfig {
                idle_secs: 60,
                max_secs: 3600,
            },
            force: false,
            confirm_reader: Some(Box::new(std::io::Cursor::new(b"y\n" as &[u8]))),
            sync: false,
            chain_build: true,
        },
    )
    .expect("second node add from git should succeed through confirmation path");

    // Step 5: Verify the existing instance was stopped and node was re-added
    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete after re-add from git");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    let node_after = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .unwrap_or_else(|| {
            panic!(
                "graph should contain the node after git re-add. Got: {:?}",
                graph.nodes.iter().map(|n| n.label()).collect::<Vec<_>>()
            )
        });
    assert_eq!(
        node_after.instance_count(),
        0,
        "should have 0 instances after re-add from git (existing instance should be stopped)"
    );
}

#[test]
fn node_add_with_sync_flag_refreshes_stale_git_hash() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let core_node_name = serve.core_node_name().to_string();

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_sync_flag_node";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    // 1. Init the node
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
            toolchain: Toolchain::Cargo,
            with_container: false,
        },
    }
    .execute(&node_ctx)
    .expect("node init command should succeed");

    let node_path = node_dir.path().join(node_name);
    let peppy_dir = node_path.join(PEPPY_OUTPUT_DIR);
    let git_hash_path = peppy_dir.join("git.hash");
    let peppy_json5_path = node_path.join("peppy.json5");

    peppy::test_support::disable_build_cmd(&peppy_json5_path);

    // 2. Invalidate git.hash to simulate a stale peppy cache
    assert!(git_hash_path.exists(), "git.hash should exist after init");
    std::fs::write(&git_hash_path, "wrong-hash\n").expect("failed to write wrong git hash");

    // 3. `node add` without `--sync` should fail with a git hash mismatch
    let add_result = NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: false,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx);
    assert!(
        add_result.is_err(),
        "node add without --sync should fail on stale git.hash"
    );
    assert!(
        add_result
            .unwrap_err()
            .to_string()
            .contains("git hash mismatch"),
        "error should mention git hash mismatch"
    );

    // 4. `node add` *with* `--sync` should refresh git.hash and succeed in one step
    NodeCommand {
        command: NodeCommands::Add {
            source: Some(node_path.display().to_string()),
            git_ref: None,
            sync: true,
            build: true,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx)
    .expect("node add with --sync should succeed");

    // 5. Verify add succeeded and the peppygen fingerprint now matches the
    //    current peppy.json5 (i.e. --sync really ran a sync)
    let logs = log_capture.logs();
    assert!(
        logs.contains(&format!("Added node {}:", node_name)),
        "logs should contain success message. Logs:\n{}",
        logs
    );
    // The sync log line should also appear, proving --sync fired
    assert!(
        logs.contains("Synced node interfaces at"),
        "logs should show sync ran. Logs:\n{}",
        logs
    );

    let fingerprint_path = node_path
        .join(PEPPYGEN_OUTPUT_PATH)
        .join("peppy.json5.sha256");
    assert!(
        fingerprint_path.exists(),
        "peppygen fingerprint should exist after --sync"
    );
    let fingerprint = std::fs::read_to_string(&fingerprint_path)
        .expect("should read fingerprint")
        .trim()
        .to_string();
    let expected = config::fingerprint::fingerprint_for_bytes(
        &std::fs::read(&peppy_json5_path).expect("should read peppy.json5"),
    );
    assert_eq!(
        fingerprint, expected,
        "fingerprint should match current peppy.json5 content after --sync"
    );

    // 6. Verify the node landed in the stack
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");
    let response = rt
        .block_on(poll_stack_list(
            &StackListRequest::new(false),
            messenger_handle,
            &core_node_name,
            CALLER_INSTANCE_ID,
            &core_node_name,
            Duration::from_secs(5),
        ))
        .expect("stack_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "v1")
        .expect("graph should contain the added node");
}

/// `--sync` is only valid for local filesystem sources. Passing it with a
/// remote (git/http) source should fail with a clear error before any
/// daemon round-trip is attempted.
#[test]
fn node_add_with_sync_flag_rejects_remote_source() {
    // No daemon / runtime setup needed — the error fires during local arg
    // validation before any async work. We still need a minimal AppContext.
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();

    let node_dir = tempfile::tempdir().expect("failed to create temp dir");
    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), Arc::clone(&shared_messenger))
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let result = NodeCommand {
        command: NodeCommands::Add {
            source: Some("https://github.com/fake-org/fake-repo.git/node".to_string()),
            git_ref: None,
            sync: true,
            build: false,
            run: false,
            args: Vec::new(),
            instance_id: None,
            idle_timeout: 60,
            max_timeout: 3600,
            force: false,
        },
    }
    .execute(&node_ctx);

    assert!(
        result.is_err(),
        "node add --sync with remote source should fail"
    );
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("--sync is only valid for local node sources"),
        "error should mention local-source restriction, got: {}",
        err
    );
}
