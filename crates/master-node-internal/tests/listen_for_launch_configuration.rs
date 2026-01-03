mod common;

use common::{CALLER_INSTANCE_ID, start_master_node};
use config::consts::NODE_CONFIG_FILE;
use master_node::encoding::{LauncherRequest, NodeAddRequest, NodeInitRequest};
use peppylib::messaging::MessengerHandle;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

async fn create_test_node(
    caller_handle: &MessengerHandle,
    master_node_name: &str,
    nodes_directory: &Path,
    node_subdir: &str,
    peppy_json5: &str,
) -> PathBuf {
    let node_dir = nodes_directory.join(node_subdir);
    let init_response = NodeInitRequest::new(nodes_directory, node_subdir)
        .poll(
            caller_handle,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            Duration::from_secs(10),
        )
        .await
        .expect("node_init request should complete");
    assert!(
        init_response.success,
        "node_init should succeed, got error: {}",
        init_response.error_message
    );

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(&node_config_path, peppy_json5).expect("failed to write node config");
    node_config_path
}

async fn seed_stack_with_node(
    caller_handle: &MessengerHandle,
    master_node_name: &str,
    node_stack: &node_stack::NodeStack,
    node_name: &str,
    node_tag: &str,
    instance_id: &str,
) {
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{node_name}",
                tag: "{node_tag}",
                launch_cmd: ["sleep", "10"]
            }}
        }}"#
    );

    let add_response = NodeAddRequest::new(peppy_json5, "/tmp")
        .with_instance_id(instance_id)
        .poll(
            caller_handle,
            master_node_name,
            CALLER_INSTANCE_ID,
            master_node_name,
            Duration::from_secs(5),
        )
        .await
        .expect("node_add request should complete");

    assert!(
        add_response.success,
        "node_add should succeed, got error: {:?}",
        add_response.error_message
    );
    assert!(node_stack.contains(node_name, node_tag));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let nodes_dir = nodes_dir.path();
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir,
        TARGET_NODE_NAME,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    launch_cmd: ["cargo", "build"]
                }}
            }}"#
        ),
    )
    .await;

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(launcher_json5, nodes_dir)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    assert!(
        response.success,
        "launcher request should succeed, got error: {}",
        response.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + deployed node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("deployed node should exist in stack");
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(
        entity.instances()[0].instance_id().as_str(),
        TARGET_INSTANCE_ID
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    const EXISTING_NODE_NAME: &str = "existing_node";
    const EXISTING_NODE_TAG: &str = "0.1.0";
    const EXISTING_INSTANCE_ID: &str = "existing_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    seed_stack_with_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_stack,
        EXISTING_NODE_NAME,
        EXISTING_NODE_TAG,
        EXISTING_INSTANCE_ID,
    )
    .await;
    let before_len = node_stack.len();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let invalid_launcher_json5 = r#"{ deployments: [unclosed"#;

    let response = LauncherRequest::new(invalid_launcher_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    assert!(!response.success, "launcher request should fail");
    assert!(
        response
            .error_message
            .contains("invalid peppy_launcher_json5"),
        "error should mention invalid json5, got: {}",
        response.error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
    assert!(node_stack.contains(EXISTING_NODE_NAME, EXISTING_NODE_TAG));

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_nodes_directory_must_be_a_directory() {
    const EXISTING_NODE_NAME: &str = "existing_node";
    const EXISTING_NODE_TAG: &str = "0.1.0";
    const EXISTING_INSTANCE_ID: &str = "existing_instance";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    seed_stack_with_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_stack,
        EXISTING_NODE_NAME,
        EXISTING_NODE_TAG,
        EXISTING_INSTANCE_ID,
    )
    .await;
    let before_len = node_stack.len();

    let temp = tempdir().expect("failed to create temp directory");
    let not_a_dir = temp.path().join("not_a_directory");
    fs::write(&not_a_dir, "not a dir").expect("failed to create file");

    let launcher_json5 = r#"{}"#;
    let response = LauncherRequest::new(launcher_json5, &not_a_dir)
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    assert!(!response.success, "launcher request should fail");
    assert!(
        response
            .error_message
            .contains("nodes_directory is not a directory"),
        "error should mention nodes_directory, got: {}",
        response.error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
    assert!(node_stack.contains(EXISTING_NODE_NAME, EXISTING_NODE_TAG));

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    const EXISTING_NODE_NAME: &str = "existing_node";
    const EXISTING_NODE_TAG: &str = "0.1.0";
    const EXISTING_INSTANCE_ID: &str = "existing_instance";

    const RESOLVED_NODE_NAME: &str = "resolved_node";
    const RESOLVED_NODE_TAG: &str = "0.1.0";

    const MISSING_NODE_NAME: &str = "missing_node";
    const MISSING_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    seed_stack_with_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_stack,
        EXISTING_NODE_NAME,
        EXISTING_NODE_TAG,
        EXISTING_INSTANCE_ID,
    )
    .await;
    let before_len = node_stack.len();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir.path(),
        RESOLVED_NODE_NAME,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{RESOLVED_NODE_NAME}",
                    tag: "{RESOLVED_NODE_TAG}",
                    launch_cmd: ["sleep", "10"]
                }}
            }}"#
        ),
    )
    .await;

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{RESOLVED_NODE_NAME}",
                    tag: "{RESOLVED_NODE_TAG}",
                    instances: [{{ instance_id: "resolved_instance" }}]
                }},
                {{
                    name: "{MISSING_NODE_NAME}",
                    tag: "{MISSING_NODE_TAG}",
                    instances: [{{ instance_id: "missing_instance" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(launcher_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    assert!(!response.success, "launcher request should fail");
    assert!(
        response
            .error_message
            .contains("deployment missing_node:0.1.0 failed"),
        "error should mention missing required deployment, got: {}",
        response.error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
    assert!(node_stack.contains(EXISTING_NODE_NAME, EXISTING_NODE_TAG));
    assert!(
        !node_stack.contains(RESOLVED_NODE_NAME, RESOLVED_NODE_TAG),
        "resolved deployment should not be applied when plan fails"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    const EXISTING_NODE_NAME: &str = "existing_node";
    const EXISTING_NODE_TAG: &str = "0.1.0";
    const EXISTING_INSTANCE_ID: &str = "existing_instance";

    const CONSUMER_NODE_NAME: &str = "consumer_node";
    const CONSUMER_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    seed_stack_with_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_stack,
        EXISTING_NODE_NAME,
        EXISTING_NODE_TAG,
        EXISTING_INSTANCE_ID,
    )
    .await;
    let before_len = node_stack.len();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir.path(),
        CONSUMER_NODE_NAME,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{CONSUMER_NODE_NAME}",
                    tag: "{CONSUMER_NODE_TAG}",
                    launch_cmd: ["sleep", "10"]
                }},
                interfaces: {{
                    subscribes_to: {{
                        topics: [
                            {{
                                id: "sensor_input",
                                node: "provider_node",
                                name: "sensor_data",
                                tag: "0.1.0"
                            }}
                        ]
                    }}
                }}
            }}"#
        ),
    )
    .await;

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{CONSUMER_NODE_NAME}",
                    tag: "{CONSUMER_NODE_TAG}",
                    instances: [{{ instance_id: "consumer_instance" }}]
                }}
            ]
        }}"#
    );

    let response = LauncherRequest::new(launcher_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    assert!(!response.success, "launcher request should fail");
    assert!(
        response
            .error_message
            .contains("does not exist in the stack"),
        "error should mention missing dependency, got: {}",
        response.error_message
    );

    assert_eq!(node_stack.len(), before_len, "stack should be unchanged");
    assert!(node_stack.contains(EXISTING_NODE_NAME, EXISTING_NODE_TAG));
    assert!(
        !node_stack.contains(CONSUMER_NODE_NAME, CONSUMER_NODE_TAG),
        "deployment should not be applied when dependencies are invalid"
    );

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    const NODE_A: &str = "node_a";
    const NODE_B: &str = "node_b";
    const TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir.path(),
        NODE_A,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{NODE_A}",
                    tag: "{TAG}",
                    launch_cmd: ["cargo", "build"]
                }}
            }}"#
        ),
    )
    .await;
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir.path(),
        NODE_B,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{NODE_B}",
                    tag: "{TAG}",
                    launch_cmd: ["sleep", "10"]
                }}
            }}"#
        ),
    )
    .await;

    let launcher_a_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{NODE_A}",
                    tag: "{TAG}",
                    instances: [{{ instance_id: "a1" }}]
                }}
            ]
        }}"#
    );

    let first = LauncherRequest::new(launcher_a_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("first launcher request should complete");

    assert!(
        first.success,
        "first launcher request should succeed, got error: {}",
        first.error_message
    );
    assert!(node_stack.contains(NODE_A, TAG));
    assert_eq!(node_stack.len(), 2, "root + node_a");

    let launcher_b_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{NODE_B}",
                    tag: "{TAG}",
                    instances: [{{ instance_id: "b1" }}]
                }}
            ]
        }}"#
    );

    let second = LauncherRequest::new(launcher_b_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("second launcher request should complete");

    assert!(
        second.success,
        "second launcher request should succeed, got error: {}",
        second.error_message
    );
    assert!(
        !node_stack.contains(NODE_A, TAG),
        "node_a should be removed by second launcher request"
    );
    assert!(node_stack.contains(NODE_B, TAG));
    assert_eq!(node_stack.len(), 2, "root + node_b");

    let entity = node_stack.find(NODE_B, TAG).expect("node_b should exist");
    assert_eq!(entity.instances().len(), 1);
    assert_eq!(entity.instances()[0].instance_id().as_str(), "b1");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_runs_generate_before_launch() {
    use config::consts::PEPPYGEN_OUTPUT_PATH;

    const TARGET_NODE_NAME: &str = "generate_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "gen_instance";

    // Use a short health timeout since we expect the launch to fail (sleep doesn't respond to health)
    let started_master = common::start_master_node_with_timeout(Duration::from_millis(500)).await;

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let node_dir = nodes_dir.path().join(TARGET_NODE_NAME);
    create_test_node(
        &started_master.caller_handle,
        &started_master.master_node_name,
        nodes_dir.path(),
        TARGET_NODE_NAME,
        &format!(
            r#"{{
                schema_version: 1,
                manifest: {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    launch_cmd: ["sleep", "10"]
                }}
            }}"#
        ),
    )
    .await;

    // Verify peppygen directory doesn't exist before launch
    let peppygen_dir = node_dir.join(PEPPYGEN_OUTPUT_PATH);
    if peppygen_dir.exists() {
        fs::remove_dir_all(&peppygen_dir)
            .expect("failed to remove pre-existing peppygen directory");
    }
    assert!(
        !peppygen_dir.exists(),
        "peppygen directory should not exist before launch"
    );

    let launcher_json5 = format!(
        r#"{{
            deployments: [
                {{
                    name: "{TARGET_NODE_NAME}",
                    tag: "{TARGET_NODE_TAG}",
                    instances: [{{ instance_id: "{TARGET_INSTANCE_ID}" }}]
                }}
            ]
        }}"#
    );

    // The launch will fail because `sleep` doesn't respond to health checks,
    // but generate should still run and create the peppygen directory
    let response = LauncherRequest::new(launcher_json5, nodes_dir.path())
        .poll(
            &started_master.caller_handle,
            &started_master.master_node_name,
            CALLER_INSTANCE_ID,
            &started_master.master_node_name,
            None,
            Duration::from_secs(5),
        )
        .await
        .expect("launcher request should complete");

    // The launch should fail because sleep doesn't respond to health checks
    assert!(
        !response.success,
        "launcher request should fail due to health check timeout"
    );

    // But generate should have run and created the peppygen directory
    assert!(
        peppygen_dir.exists(),
        "peppygen directory should exist after launch attempt (proving generate ran)"
    );

    // Also verify the Cargo.toml was created/updated by generate
    let cargo_toml_path = node_dir.join("Cargo.toml");
    assert!(
        cargo_toml_path.exists(),
        "Cargo.toml should exist after generate"
    );
    let cargo_toml = fs::read_to_string(&cargo_toml_path).expect("failed to read Cargo.toml");
    assert!(
        cargo_toml.contains("peppygen"),
        "Cargo.toml should contain peppygen dependency after generate"
    );

    started_master.task.abort();
}
