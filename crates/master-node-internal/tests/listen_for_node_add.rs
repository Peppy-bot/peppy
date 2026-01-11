mod common;

use common::{CALLER_INSTANCE_ID, create_test_node_with_name, start_master_node};
use config::consts::NODE_CONFIG_FILE;
use master_node::encoding::{NodeAddFeedback, NodeAddGoal, NodeAddResult};
use peppylib::MessengerHandle;
use std::path::Path;
use std::time::Duration;

const ADD_CMD_MARKER_FILE: &str = "add_cmd_executed.marker";
const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

fn write_peppy_json5(dir: &Path, content: &str) {
    std::fs::write(dir.join(NODE_CONFIG_FILE), content).expect("failed to write peppy.json5");
}

/// Helper function to send a node_add goal and wait for the result.
/// This wraps the action pattern for simpler test usage.
async fn send_node_add_and_wait(
    messenger: &MessengerHandle,
    master_node_name: &str,
    from_dir: &Path,
) -> Result<NodeAddResult, String> {
    let goal = NodeAddGoal::new(from_dir);
    let mut action_handle = goal
        .send_goal(
            messenger,
            master_node_name,
            CALLER_INSTANCE_ID,
            Some(master_node_name),
            None,
            GOAL_TIMEOUT,
        )
        .await
        .map_err(|e| format!("Failed to send goal: {}", e))?;

    // TODO At least one test should test if the stdout and stderr feedback are correctly being sent here
    // Collect feedback (optional, we drain it to completion)
    let feedback_handle = tokio::spawn(async move {
        let mut feedback_lines = Vec::new();
        loop {
            match action_handle.on_next_feedback().await {
                Ok(msg) => {
                    let payload = msg.payload();
                    if let Ok(feedback) = NodeAddFeedback::decode(&payload.to_bytes()) {
                        feedback_lines.push(feedback);
                    }
                }
                Err(_) => break,
            }
        }
        (action_handle, feedback_lines)
    });

    let (action_handle, _feedback) = tokio::time::timeout(RESULT_TIMEOUT, feedback_handle)
        .await
        .map_err(|_| "Timeout waiting for feedback".to_string())?
        .map_err(|e| format!("Feedback task failed: {}", e))?;

    NodeAddResult::request_result(messenger, &action_handle, RESULT_TIMEOUT)
        .await
        .map_err(|e| format!("Failed to get result: {}", e))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_success() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    // Use a pre-built test node to avoid compilation delays during the test
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_dir,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    assert!(node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 2, "root + added node");

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");
    // `add` only adds the node to the NodeStack but doesn't spawn any instance
    assert_eq!(entity.instances().len(), 0);

    // Verify the node was copied to the peppy storage directory
    let snapshot_path = add_result.snapshot_path.as_path();
    let root_path = entity.root_path();
    assert_eq!(
        snapshot_path, root_path,
        "snapshot_path should match copied node path"
    );
    assert!(
        root_path != node_dir.as_path(),
        "node should be copied to a different location, got: {}",
        root_path.display()
    );
    assert!(
        root_path.exists(),
        "copied node directory should exist: {}",
        root_path.display()
    );
    assert!(
        root_path.join(NODE_CONFIG_FILE).exists(),
        "config file should be present at the root of the node folder: {}",
        root_path.join(NODE_CONFIG_FILE).display()
    );

    // Verify the path follows the expected naming convention: <node_name>_<tag>_<uuid>
    let folder_name = root_path
        .file_name()
        .and_then(|n| n.to_str())
        .expect("should have folder name");
    assert!(
        folder_name.starts_with(&format!("{TARGET_NODE_NAME}_{TARGET_NODE_TAG}_")),
        "folder name should start with '<node_name>_<tag>_', got: {}",
        folder_name
    );

    // Clean up the copied directory
    let _ = std::fs::remove_dir_all(root_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_config_found() {
    const TARGET_NODE_NAME: &str = "runnable_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    // Use a pre-built test node to avoid compilation delays during the test
    let node_dir = create_test_node_with_name(TARGET_NODE_NAME, TARGET_NODE_TAG);

    std::fs::remove_file(node_dir.join(NODE_CONFIG_FILE))
        .expect("failed to remove peppy.json5 config file");

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &node_dir,
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        !add_result.success,
        "node_add should not succeed, the config file is missing",
    );

    assert!(!node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG));
    assert_eq!(node_stack.len(), 1, "root");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_invalid_config_fails() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{ manifest: [unclosed"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail for invalid json5"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to parse node config"))
            .unwrap_or(false),
        "error message should indicate parse failure, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_no_start_cmd_fails() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "no_start_cmd_node",
            tag: "0.1.0",
        },
        parameters: {}
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when start_cmd is missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("start_cmd"))
            .unwrap_or(false),
        "error message should mention start_cmd, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_dependency_not_resolved() {
    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // Try to add a consumer node that depends on a non-existent provider
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "consumer_node",
            tag: "1.0.0",
            start_cmd: ["sleep", "10"],
        },
        interfaces: {
            subscribes_to: {
                topics: [
                    {
                        id: "sensor_input",
                        node: "non_existent_node",
                        name: "sensor_data",
                        tag: "1.0.0"
                    }
                ]
            }
        }
    }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when dependencies are missing"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Failed to add node"))
            .unwrap_or(false),
        "error message should indicate add failure, got: {:?}",
        add_result.error_message
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("does not exist in the stack"))
            .unwrap_or(false),
        "error message should indicate missing dependency, got: {:?}",
        add_result.error_message
    );

    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_same_tags_fails() {
    const NODE_NAME: &str = "mismatch_node";
    const NODE_TAG: &str = "1.0.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    // First add: no interfaces
    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                start_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir_v1.path(),
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    assert_eq!(node_stack.len(), 2, "root + v1");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should exist after v1");
    assert_eq!(entity.instances().len(), 0);
    let copied_path = entity.root_path().to_path_buf();

    // Second add: same name+tag but different interfaces -> should be rejected.
    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "{NODE_TAG}",
                start_cmd: ["sleep", "10"]
            }},
            interfaces: {{
                exposes: {{
                    topics: [{{ name: "/example" }}]
                }}
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir_v2.path(),
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        !add_v2.success,
        "node_add should fail when interfaces mismatch for same name+tag"
    );
    assert!(
        add_v2
            .error_message
            .as_ref()
            .map(|msg| msg.contains("Config mismatch"))
            .unwrap_or(false),
        "error message should indicate config mismatch, got: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 2, "stack should be unchanged");
    let entity = node_stack
        .find(NODE_NAME, NODE_TAG)
        .expect("node should still exist after v2 failure");
    assert_eq!(entity.instances().len(), 0, "should not have any instances");

    // Clean up
    let _ = std::fs::remove_dir_all(&copied_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_same_node_different_tags_create_two_entities() {
    const NODE_NAME: &str = "versioned_node";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir_v1 = tempfile::tempdir().expect("failed to create temp source dir");
    let source_dir_v2 = tempfile::tempdir().expect("failed to create temp source dir");

    let peppy_json5_v1 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "1.0.0",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v1.path(), &peppy_json5_v1);

    let add_v1 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir_v1.path(),
    )
    .await
    .expect("node_add v1 should complete");

    assert!(
        add_v1.success,
        "node_add v1 should succeed, got error: {:?}",
        add_v1.error_message
    );

    let peppy_json5_v2 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{NODE_NAME}",
                tag: "2.0.0",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir_v2.path(), &peppy_json5_v2);

    let add_v2 = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir_v2.path(),
    )
    .await
    .expect("node_add v2 should complete");

    assert!(
        add_v2.success,
        "node_add v2 should succeed, got error: {:?}",
        add_v2.error_message
    );

    assert_eq!(node_stack.len(), 3, "root + two versions");
    assert!(node_stack.contains(NODE_NAME, "1.0.0"));
    assert!(node_stack.contains(NODE_NAME, "2.0.0"));

    // Clean up copied directories
    if let Some(entity) = node_stack.find(NODE_NAME, "1.0.0") {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }
    if let Some(entity) = node_stack.find(NODE_NAME, "2.0.0") {
        let _ = std::fs::remove_dir_all(entity.root_path());
    }

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_copies_files_to_storage() {
    const TARGET_NODE_NAME: &str = "copy_test_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    // Create a temporary source directory with some files
    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let test_file_content = "test file content";
    std::fs::write(source_dir.path().join("test_file.txt"), test_file_content)
        .expect("failed to write test file");

    // Create a subdirectory with a file
    let sub_dir = source_dir.path().join("subdir");
    std::fs::create_dir(&sub_dir).expect("failed to create subdir");
    std::fs::write(sub_dir.join("nested_file.txt"), "nested content")
        .expect("failed to write nested file");

    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");

    let copied_path = entity.root_path();
    assert_eq!(
        add_result.snapshot_path.as_path(),
        copied_path,
        "snapshot_path should match copied node path"
    );

    // Verify the file was copied
    let copied_file = copied_path.join("test_file.txt");
    assert!(copied_file.exists(), "test_file.txt should be copied");
    let content = std::fs::read_to_string(&copied_file).expect("should read copied file");
    assert_eq!(content, test_file_content, "file content should match");

    // Verify the subdirectory and nested file were copied
    let copied_nested = copied_path.join("subdir").join("nested_file.txt");
    assert!(copied_nested.exists(), "nested file should be copied");
    let nested_content = std::fs::read_to_string(&copied_nested).expect("should read nested file");
    assert_eq!(
        nested_content, "nested content",
        "nested content should match"
    );

    // Clean up
    let _ = std::fs::remove_dir_all(copied_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_runs_add_cmd() {
    const TARGET_NODE_NAME: &str = "add_cmd_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd creates a marker file to prove it was executed
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                add_cmd: ["touch", "{ADD_CMD_MARKER_FILE}"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should succeed");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    let entity = node_stack
        .find(TARGET_NODE_NAME, TARGET_NODE_TAG)
        .expect("node should exist in stack");

    let copied_path = entity.root_path();

    // Verify that add_cmd was executed on the COPIED folder (not the source)
    // by checking the marker file exists only in the copied path
    let marker_file = copied_path.join(ADD_CMD_MARKER_FILE);
    assert!(
        marker_file.exists(),
        "add_cmd should have created marker file at {}",
        marker_file.display()
    );

    // Verify add_cmd did NOT run on the source directory
    let source_marker = source_dir.path().join(ADD_CMD_MARKER_FILE);
    assert!(
        !source_marker.exists(),
        "add_cmd should NOT have created marker file in source dir at {}",
        source_marker.display()
    );

    // Clean up
    let _ = std::fs::remove_dir_all(copied_path);

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_add_cmd_failure_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that will fail (non-existent command)
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                add_cmd: ["this_command_does_not_exist_12345"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when add_cmd fails"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("add_cmd failed"))
            .unwrap_or(false),
        "error message should mention add_cmd failure, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );
    assert_eq!(node_stack.len(), 1, "only root should exist");

    started_master.task.abort();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_add_add_cmd_nonzero_exit_fails_add() {
    const TARGET_NODE_NAME: &str = "add_cmd_exit_fail_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node().await;
    let node_stack = started_master.node_stack.clone();

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");

    // add_cmd that exits with non-zero status
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                add_cmd: ["sh", "-c", "exit 1"],
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add request should complete");

    assert!(
        !add_result.success,
        "node_add should fail when add_cmd exits with non-zero status"
    );
    assert!(
        add_result
            .error_message
            .as_ref()
            .map(|msg| msg.contains("add_cmd failed"))
            .unwrap_or(false),
        "error message should mention add_cmd failure, got: {:?}",
        add_result.error_message
    );

    // Node should not be in the stack
    assert!(
        !node_stack.contains(TARGET_NODE_NAME, TARGET_NODE_TAG),
        "node should not be added when add_cmd fails"
    );

    started_master.task.abort();
}
