use config::consts::PEPPY_OUTPUT_DIR;
use master_node::encoding::NodeListRequest;
use node_stack::SerializedNodeGraph;
use peppy::commands::Command;
use peppy::commands::node::{NodeCommand, NodeCommands, NodeName};
use peppy::context::AppContext;
use peppy::test_support::{LogCapture, ServeCommandEmulation};
use peppylib::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::services::shutdown::listen_for_shutdown;
use std::sync::Arc;
use std::time::Duration;

const CALLER_INSTANCE_ID: &str = "peppy-test";

#[test]
fn node_add_command_succeeds() {
    // Use a runtime for async setup; NodeCommand::execute creates its own runtime internally
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let master_node_name = serve.master_node_name().to_string();
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_node";

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

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
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

    peppy::test_support::disable_add_cmd(&peppy_json5_path);

    // Now add the node to the node stack
    NodeCommand {
        command: NodeCommands::Add {
            source: node_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
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

    // Query the node stack to verify the node was added
    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph with 0 instances (since run=false)
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
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
    let master_node_name = serve.master_node_name().to_string();
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_run_node";
    let instance_id = "test_add_run_instance";

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

    // First, create a node using the init command
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
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
    peppy::test_support::override_start_cmd(&peppy_json5_path);

    let node_messenger = MessengerHandle::from_shared(shared_messenger.clone());
    let _node_ready_handle = rt
        .block_on(listen_for_node_ready(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node ready service should start");
    let _node_health_handle = rt
        .block_on(listen_for_node_health(
            &node_messenger,
            &master_node_name,
            instance_id,
            node_name,
        ))
        .expect("node health service should start");

    // Add the node to the node stack with run=true to also start an instance
    NodeCommand {
        command: NodeCommands::Add {
            source: node_path.display().to_string(),
            git_ref: None,
            start: true,
            args: Vec::new(),
            instance_id: Some(instance_id.to_string()),
            timeout: 60,
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
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph with 1 instance (since run=true)
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
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
    let master_node_name = serve.master_node_name().to_string();
    assert!(
        !master_node_name.is_empty(),
        "master_node_name should not be empty"
    );

    // Create a temp directory for the node
    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_sync_then_add_node";

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

    // 1. Create a node with `node init`
    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
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

    // Disable add_cmd to avoid build step
    let peppy_json5_path = node_path.join("peppy.json5");
    peppy::test_support::disable_add_cmd(&peppy_json5_path);

    // 3. Run `node add .` on that node, it'll fail due to git hash mismatch
    let add_result = NodeCommand {
        command: NodeCommands::Add {
            source: node_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
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
        AppContext::with_messenger(&node_path, shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    NodeCommand {
        command: NodeCommands::Sync {},
    }
    .execute(&sync_ctx)
    .expect("node sync command should succeed");

    // 5. Run `node add .` again. This time it should succeed
    NodeCommand {
        command: NodeCommands::Add {
            source: node_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
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
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");

    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");

    // Verify the node is in the graph
    let added_node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == "0.1.0")
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

#[cfg(unix)]
#[test]
fn node_add_same_node_shutdown_existing_instances() {
    let rt = tokio::runtime::Runtime::new().expect("failed to create tokio runtime");
    let serve = rt
        .block_on(ServeCommandEmulation::with_mock())
        .expect("failed to create serve emulation");
    let shared_messenger = serve.messenger();
    let master_node_name = serve.master_node_name().to_string();

    let node_dir = tempfile::tempdir().expect("failed to create temp dir for node");
    let node_name = "test_add_overwrite_running_node";
    let node_tag = "0.1.0";
    let instance_id_1 = "test_add_overwrite_instance_1";
    let instance_id_2 = "test_add_overwrite_instance_2";

    let node_ctx = Arc::new(
        AppContext::with_messenger(node_dir.path(), shared_messenger.clone())
            .with_daemon_state_file(serve.daemon_state_path()),
    );

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);

    NodeCommand {
        command: NodeCommands::Init {
            node_name: NodeName::new(node_name).expect("valid node name"),
            to_dir: None,
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

    // Avoid build step during add.
    peppy::test_support::disable_add_cmd(&peppy_json5_path);

    NodeCommand {
        command: NodeCommands::Add {
            source: node_path.display().to_string(),
            git_ref: None,
            start: false,
            args: Vec::new(),
            instance_id: None,
            timeout: 60,
        },
    }
    .execute(&node_ctx)
    .expect("first node add should succeed");

    // Simulate two running instances without launching real processes.
    let instance_name_1 = config::node::Name::new(instance_id_1).expect("valid instance id 1");
    let instance_name_2 = config::node::Name::new(instance_id_2).expect("valid instance id 2");
    serve
        .node_stack()
        .add_instance(node_name, node_tag, Some(&instance_name_1), None)
        .expect("add_instance for instance 1 should succeed");
    serve
        .node_stack()
        .add_instance(node_name, node_tag, Some(&instance_name_2), None)
        .expect("add_instance for instance 2 should succeed");

    // Expose shutdown services so master node can stop the existing instances on overwrite.
    let node_messenger = MessengerHandle::from_shared(shared_messenger.clone());
    let (_shutdown_handle_1, shutdown_rx_1) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &master_node_name,
            instance_id_1,
            node_name,
        ))
        .expect("node shutdown service for instance 1 should start");
    let (_shutdown_handle_2, shutdown_rx_2) = rt
        .block_on(listen_for_shutdown(
            &node_messenger,
            &master_node_name,
            instance_id_2,
            node_name,
        ))
        .expect("node shutdown service for instance 2 should start");

    // Decline the overwrite prompt: add should be aborted and instances should remain.
    let denied = with_test_stdin("n\n", || {
        NodeCommand {
            command: NodeCommands::Add {
                source: node_path.display().to_string(),
                git_ref: None,
                start: false,
                args: Vec::new(),
                instance_id: None,
                timeout: 60,
            },
        }
        .execute(&node_ctx)
    });

    assert!(
        denied.is_err(),
        "second node add should be aborted when user declines"
    );
    let denied_msg = denied.unwrap_err().to_string();
    assert!(
        denied_msg.contains("Node add aborted by user"),
        "error should mention user abort, got: {}",
        denied_msg
    );

    let messenger_handle = node_ctx
        .messenger_handle()
        .expect("messenger handle should be available");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == node_tag)
        .expect("node should exist in stack");
    assert_eq!(node.instance_count(), 2, "instances should not be stopped");

    // Accept the prompt: overwrite should stop both instances via shutdown.
    with_test_stdin("y\n", || {
        NodeCommand {
            command: NodeCommands::Add {
                source: node_path.display().to_string(),
                git_ref: None,
                start: false,
                args: Vec::new(),
                instance_id: None,
                timeout: 60,
            },
        }
        .execute(&node_ctx)
    })
    .expect("overwrite add should succeed when user accepts");

    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), shutdown_rx_1).await })
        .expect("shutdown request for instance 1 should arrive")
        .expect("shutdown signal for instance 1 should be delivered");
    rt.block_on(async { tokio::time::timeout(Duration::from_secs(2), shutdown_rx_2).await })
        .expect("shutdown request for instance 2 should arrive")
        .expect("shutdown signal for instance 2 should be delivered");

    let response = rt
        .block_on(NodeListRequest::new(false).poll(
            messenger_handle,
            &master_node_name,
            CALLER_INSTANCE_ID,
            &master_node_name,
            Duration::from_secs(5),
        ))
        .expect("node_list request should complete");
    let graph: SerializedNodeGraph =
        serde_json::from_str(&response.graph_json).expect("graph_json should parse");
    let node = graph
        .nodes
        .iter()
        .find(|n| n.name == node_name && n.tag == node_tag)
        .expect("node should exist in stack after overwrite");
    assert_eq!(
        node.instance_count(),
        0,
        "instances should be stopped before overwrite completes"
    );

    // Best-effort cleanup of the snapshot directory in the peppy data dir.
    let _ = std::fs::remove_dir_all(std::path::Path::new(&node.fs_root_path));
}

#[cfg(not(unix))]
#[test]
fn node_add_same_node_shutdown_existing_instances() {
    eprintln!("skipped: prompt input redirection requires unix");
}

#[cfg(unix)]
fn with_test_stdin<T>(input: &str, f: impl FnOnce() -> T) -> T {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    static STDIN_LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    let _guard = STDIN_LOCK
        .get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .expect("stdin lock poisoned");

    struct StdinRedirect {
        saved_fd: i32,
    }

    impl Drop for StdinRedirect {
        fn drop(&mut self) {
            unsafe {
                let _ = libc::dup2(self.saved_fd, libc::STDIN_FILENO);
                let _ = libc::close(self.saved_fd);
            }
        }
    }

    let mut tmp = tempfile::NamedTempFile::new().expect("failed to create temp stdin file");
    tmp.write_all(input.as_bytes())
        .expect("failed to write temp stdin file");
    tmp.flush().expect("failed to flush temp stdin file");
    let file = tmp.reopen().expect("failed to reopen temp stdin file");

    let saved_fd = unsafe { libc::dup(libc::STDIN_FILENO) };
    assert!(saved_fd >= 0, "failed to dup stdin");
    let _redirect = StdinRedirect { saved_fd };
    let result = unsafe { libc::dup2(file.as_raw_fd(), libc::STDIN_FILENO) };
    assert!(result >= 0, "failed to redirect stdin");

    f()
}
