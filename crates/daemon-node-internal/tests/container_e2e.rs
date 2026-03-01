mod common;

use common::{
    CALLER_INSTANCE_ID, NodeStartTestTimeouts, send_node_add_and_wait, send_node_start_and_wait,
    start_daemon_node_with_real_messenger,
};
use config::node::Name as NodeName;
use config::node::Toolchain;
use config::peppy_config::Name;
use config::runtime::{NodeInstance, RuntimeConfig};
use daemon_node::encoding::NodeInitRequest;
use std::time::Duration;
use tempfile::tempdir;

/// Returns `true` if the Apptainer runtime is available and operational.
/// Used to gracefully skip tests on hosts without Lima/Apptainer.
fn apptainer_available() -> bool {
    let facade = match containers::Apptainer::new() {
        Ok(f) => f,
        Err(e) => {
            eprintln!("SKIPPING: Apptainer runtime not available: {e}");
            return false;
        }
    };

    if let Err(e) = facade.version() {
        eprintln!("SKIPPING: Apptainer runtime not operational: {e}");
        return false;
    }

    true
}

/// End-to-end test: init a Rust container node, build the container image,
/// and start it using the real Apptainer runtime.
///
/// Exercises the full chain: NodeInitRequest (with_container=true) ->
/// node_add (apptainer build) -> node_start (apptainer run).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
//#[ignore = "slow: builds container images via Lima VM"]
async fn container_e2e_rust_init_add_start() {
    if !apptainer_available() {
        return;
    }

    const NODE_NAME: &str = "rust_e2e_node";
    const NODE_TAG: &str = "0.1.0";
    const INSTANCE_ID: &str = "rust_e2e_instance";

    let started = start_daemon_node_with_real_messenger().await;

    // Step 1: Init the node with container support
    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let init_response = NodeInitRequest::new(
        nodes_root.path(),
        NODE_NAME,
        "test-hash",
        true,
        Toolchain::Cargo,
    )
    .poll(
        &started.caller_handle,
        &started.daemon_node_name,
        CALLER_INSTANCE_ID,
        &started.daemon_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_init request should complete");

    assert!(
        init_response.success,
        "node_init should succeed, got error: {}",
        init_response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert!(
        node_dir.join("apptainer.def").exists(),
        "init should generate apptainer.def"
    );

    // Step 2: Add the node (builds the container image)
    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.daemon_node_name,
        &node_dir,
        Duration::from_secs(30),
        Duration::from_secs(600),
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add (container build) should succeed, got error: {:?}",
        add_response.error_message
    );

    // Step 3: Start the container against a real messaging endpoint.
    // This mirrors real world usage and avoids mocked ready/health responders.
    let (messaging_host, messaging_port) = started
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");

    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstance {
            instance_id: Name::new(INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        NODE_NAME,
        &started.daemon_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.daemon_node_name,
        &runtime_config_json5,
        NODE_NAME,
        NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    assert!(
        start_response.result.success,
        "node_start should succeed, got error: {:?}",
        start_response.result.error_message
    );

    assert!(
        start_response.result.pid.is_some(),
        "node_start should return a PID on success"
    );
    assert!(
        start_response.result.pid.unwrap() > 0,
        "node_start PID should be a positive number"
    );

    let instance_id = NodeName::new(INSTANCE_ID).expect("valid instance id");
    assert!(
        started
            .node_stack
            .find_by_instance_id(&instance_id)
            .is_some(),
        "instance should be registered in the node stack"
    );

    assert!(
        start_response.goal_response.accepted,
        "goal should be accepted"
    );

    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("Executing apptainer run"),
        "log file should contain apptainer run command, got:\n{}",
        log_content
    );
    assert!(
        !log_content.contains("CodegenFingerprintRead"),
        "log file should not contain config fingerprint startup errors, got:\n{}",
        log_content
    );
}

/// End-to-end test: init a Python container node, build the container image,
/// and start it using the real Apptainer runtime.
///
/// Exercises the full chain: NodeInitRequest (with_container=true) ->
/// node_add (apptainer build) -> node_start (apptainer run).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
//#[ignore = "slow: builds container images via Lima VM"]
async fn container_e2e_python_init_add_start() {
    if !apptainer_available() {
        return;
    }

    const NODE_NAME: &str = "python_e2e_node";
    const NODE_TAG: &str = "0.1.0";
    const INSTANCE_ID: &str = "python_e2e_instance";

    let started = start_daemon_node_with_real_messenger().await;

    // Step 1: Init the node with container support
    let nodes_root = tempdir().expect("failed to create temp nodes root directory");

    let init_response = NodeInitRequest::new(
        nodes_root.path(),
        NODE_NAME,
        "test-hash",
        true,
        Toolchain::Uv,
    )
    .poll(
        &started.caller_handle,
        &started.daemon_node_name,
        CALLER_INSTANCE_ID,
        &started.daemon_node_name,
        Duration::from_secs(30),
    )
    .await
    .expect("node_init request should complete");

    assert!(
        init_response.success,
        "node_init should succeed, got error: {}",
        init_response.error_message
    );

    let node_dir = nodes_root.path().join(NODE_NAME);
    assert!(
        node_dir.join("apptainer.def").exists(),
        "init should generate apptainer.def"
    );

    // Step 2: Add the node (builds the container image)
    let add_response = send_node_add_and_wait(
        &started.caller_handle,
        &started.daemon_node_name,
        &node_dir,
        Duration::from_secs(30),
        Duration::from_secs(300),
        None,
    )
    .await
    .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add (container build) should succeed, got error: {:?}",
        add_response.error_message
    );

    // Step 3: Start the container against a real messaging endpoint.
    // This mirrors real world usage and avoids mocked ready/health responders.
    let (messaging_host, messaging_port) = started
        .caller_handle
        .messaging_endpoint()
        .await
        .expect("zenoh endpoint should be available");

    let runtime_config = RuntimeConfig::new(
        messaging_host.as_str(),
        messaging_port,
        NodeInstance {
            instance_id: Name::new(INSTANCE_ID).unwrap(),
            arguments: Default::default(),
        },
        NODE_NAME,
        &started.daemon_node_name,
    )
    .expect("runtime config should be valid");

    let runtime_config_json5 =
        serde_json5::to_string(&runtime_config).expect("runtime config should serialize");

    let start_response = send_node_start_and_wait(
        &started.caller_handle,
        &started.daemon_node_name,
        &runtime_config_json5,
        NODE_NAME,
        NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(30),
            result: Duration::from_secs(60),
        },
        None,
    )
    .await
    .expect("node_start action should complete");

    assert!(
        start_response.result.success,
        "node_start should succeed, got error: {:?}",
        start_response.result.error_message
    );

    assert!(
        start_response.result.pid.is_some(),
        "node_start should return a PID on success"
    );
    assert!(
        start_response.result.pid.unwrap() > 0,
        "node_start PID should be a positive number"
    );

    let instance_id = NodeName::new(INSTANCE_ID).expect("valid instance id");
    assert!(
        started
            .node_stack
            .find_by_instance_id(&instance_id)
            .is_some(),
        "instance should be registered in the node stack"
    );

    assert!(
        start_response.goal_response.accepted,
        "goal should be accepted"
    );

    let log_path = &start_response.goal_response.log_path;
    assert!(log_path.exists(), "log file should exist at {:?}", log_path);

    let log_content = std::fs::read_to_string(log_path).expect("should be able to read log file");
    assert!(
        log_content.contains("Executing apptainer run"),
        "log file should contain apptainer run command, got:\n{}",
        log_content
    );
    assert!(
        !log_content.contains("CodegenFingerprintRead"),
        "log file should not contain config fingerprint startup errors, got:\n{}",
        log_content
    );
}
