mod common;

use common::{AbortOnDrop, CALLER_INSTANCE_ID, start_master_node_with_health_timeout};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use config::node::NodeConfigParser;
use git2::{Repository, Signature};
use master_node::encoding::{LaunchFeedback, LaunchGoal, LaunchGoalResponse, LaunchResult};
use master_node::names;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::{ActionMessenger, PeppyError};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tempfile::tempdir;

use crate::common::start_master_node_with_mock_messenger;

struct NodeConfigOptions<'a> {
    add_cmd: &'a [&'a str],
    start_cmd: &'a [&'a str],
    subscribes_to_uvc_camera: bool,
    exposes_camera_stream: bool,
}

impl Default for NodeConfigOptions<'_> {
    fn default() -> Self {
        Self {
            add_cmd: &["true"],
            start_cmd: &[],
            subscribes_to_uvc_camera: false,
            exposes_camera_stream: false,
        }
    }
}

const LAUNCHER_EXAMPLE1: &str = r#"
{
  deployments: [
    {
      name: "uvc_camera",
      tag: "0.1.0",
      source: {
        repo: "${UVC_CAMERA_REPO}",
        path: "uvc_camera"
      },
      instances: [
        {
          instance_id: "camera_front",
          parameters: {
            device: {
              physical: "/dev/video_right",
              sim: "mujoco:camera_right",
              priority: "physical"
            },
            video: {
              frame_rate: 30,
              resolution: {
                width: 1920,
                height: 1080,
              },
              encoding: "yuyv",
            },
          }
        },
        {
          instance_id: "camera_rear",
          parameters: {
            device: {
              physical: "/dev/video_left",
              sim: "mujoco:camera_left",
              priority: "physical"
            },
            video: {
              frame_rate: 30,
              resolution: {
                width: 1920,
                height: 1080,
              },
              encoding: "yuyv",
            },
          }
        }
      ]
    },
    {
      name: "robot_brain",
      tag: "0.1.0",
      instances: [
        {
          instance_id: "main_robot_brain",
          parameters: {}
        }
      ]
    },
  ]
}
"#;

const GOAL_TIMEOUT: Duration = Duration::from_secs(30);
const RESULT_TIMEOUT: Duration = Duration::from_secs(120);

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    start_cmd: &[&str],
    subscribes_to_uvc_camera: bool,
    exposes_camera_stream: bool,
) -> PathBuf {
    write_node_config_with_options(
        nodes_directory,
        node_name,
        node_tag,
        git_hash,
        NodeConfigOptions {
            start_cmd,
            subscribes_to_uvc_camera,
            exposes_camera_stream,
            ..Default::default()
        },
    )
}

fn write_node_config_with_options(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    git_hash: &str,
    options: NodeConfigOptions<'_>,
) -> PathBuf {
    let NodeConfigOptions {
        add_cmd,
        start_cmd,
        subscribes_to_uvc_camera,
        exposes_camera_stream,
    } = options;
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");

    let add_cmd_json5 = add_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("add_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");

    let start_cmd_json5 = start_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("start_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");

    let exposes = if exposes_camera_stream {
        r#"
        interfaces: {
          exposes: {
            topics: [
              { name: "camera_stream" }
            ]
          }
        }
        "#
    } else {
        ""
    };

    let subscribes_to = if subscribes_to_uvc_camera {
        r#"
        interfaces: {
          subscribes_to: {
            topics: [
              { id: "camera_stream", node: "uvc_camera", tag: "0.1.0", name: "camera_stream" }
            ]
          }
        }
        "#
    } else {
        ""
    };

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(
        &node_config_path,
        format!(
            r#"{{
              schema_version: 1,
              manifest: {{
                name: "{node_name}",
                tag: "{node_tag}",
                language: "rust",
                add_cmd: [{add_cmd_json5}],
                start_cmd: [{start_cmd_json5}]
              }},
              {exposes}
              {subscribes_to}
            }}"#
        ),
    )
    .expect("failed to write node config");

    config::fingerprint::create_codegen_fingerprint(
        &node_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    let peppy_output_dir = node_dir.join(PEPPY_OUTPUT_DIR);
    fs::create_dir_all(&peppy_output_dir).expect("failed to create peppy output directory");
    fs::write(peppy_output_dir.join("git.hash"), git_hash).expect("failed to write node git hash");

    node_dir
}

fn create_uvc_camera_repo(to_path: &Path, node_tag: &str) -> PathBuf {
    let repo_path = to_path.join("uvc_camera_repo.git");
    fs::create_dir_all(&repo_path).expect("failed to create repo directory");

    let repo = Repository::init(&repo_path).expect("failed to init repository");
    let signature =
        Signature::now("Peppy", "peppy@example.com").expect("failed to create signature");

    let uvc_dir = repo_path.join("uvc_camera");
    fs::create_dir_all(&uvc_dir).expect("failed to create uvc directory");

    let rel_config_path = Path::new("uvc_camera").join(NODE_CONFIG_FILE);
    fs::write(
        repo_path.join(&rel_config_path),
        format!(
            r#"{{
              schema_version: 1,
              manifest: {{
                name: "uvc_camera",
                tag: "{node_tag}",
                language: "rust",
                add_cmd: ["true"],
                start_cmd: ["sleep", "60"]
              }},
              interfaces: {{
                exposes: {{
                  topics: [
                    {{ name: "camera_stream" }}
                  ]
                }}
              }}
            }}"#
        ),
    )
    .expect("failed to write uvc_camera peppy.json5");

    let mut index = repo.index().expect("failed to open index");
    index
        .add_path(&rel_config_path)
        .expect("failed to add uvc config");
    index.write().expect("failed to write index");

    let tree_id = index.write_tree().expect("failed to write tree");
    let tree = repo.find_tree(tree_id).expect("failed to find tree");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "uvc_camera initial",
            &tree,
            &[],
        )
        .expect("failed to commit");
    let commit = repo.find_commit(commit_id).expect("failed to find commit");

    // Tag the commit so deployments can reference it via `tag`.
    repo.tag(node_tag, commit.as_object(), &signature, node_tag, false)
        .expect("failed to create tag");

    repo_path
}

async fn send_launch_and_wait(
    messenger: &MessengerHandle,
    master_node_name: &str,
    peppy_launch_json5: &str,
    nodes_directory: &Path,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> Result<(LaunchGoalResponse, LaunchResult), String> {
    let goal = LaunchGoal::new(peppy_launch_json5, nodes_directory);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode launch goal: {e}"))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        master_node_name,
        CALLER_INSTANCE_ID,
        master_node_name,
        names::STACK_LAUNCH_ACTION,
        None,
        None,
        goal_payload,
        config::node::QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send launch goal: {e}"))?;

    let goal_response_payload = action_handle.goal_response().payload().to_bytes();
    let goal_response = LaunchGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {e}"))?;

    if !goal_response.accepted {
        return Err(goal_response
            .rejection_reason
            .unwrap_or_else(|| "launch goal rejected".to_string()));
    }

    let deadline = tokio::time::Instant::now() + result_timeout;

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err("Timeout waiting for launch result".to_string());
            }
            let remaining = deadline - now;
            let drain_timeout = Duration::from_millis(50).min(remaining);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    let payload = msg.payload().to_bytes();
                    let _ = LaunchFeedback::decode(&payload);
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err("Timeout waiting for launch result".to_string());
        }
        let remaining = deadline - now;
        let poll_timeout = Duration::from_millis(200).min(remaining);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload().to_bytes();
                match LaunchResult::decode(&payload) {
                    Ok(result) => return Ok((goal_response, result)),
                    Err(err) => {
                        let pending = std::str::from_utf8(payload.as_ref())
                            .map(|text| text.starts_with("result pending"))
                            .unwrap_or(false);
                        if !pending {
                            return Err(format!("Failed to decode launch result: {err}"));
                        }
                    }
                }
            }
            Err(PeppyError::ActionResultTimeout { .. }) => {}
            Err(err) => return Err(format!("Failed to get launch result: {err}")),
        }

        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const UVC_NODE_NAME: &str = "uvc_camera";
    const ROBOT_NODE_NAME: &str = "robot_brain";
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let uvc_repo_path = create_uvc_camera_repo(nodes_dir.path(), NODE_TAG);
    let _robot_brain_path = write_node_config(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        true,
        false,
    );

    // Set up ready/health responders for all instances in LAUNCHER_EXAMPLE1.
    let node_messenger = MessengerHandle::from_shared(started_master.shared_messenger.clone());
    let _ready_front = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "camera_front",
            UVC_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_front = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_master.master_node_name,
            "camera_front",
            UVC_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_rear = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "camera_rear",
            UVC_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_rear = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_master.master_node_name,
            "camera_rear",
            UVC_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_brain = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "main_robot_brain",
            ROBOT_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_brain = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_master.master_node_name,
            "main_robot_brain",
            ROBOT_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );

    // Allow listeners to establish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let launcher_json5 =
        LAUNCHER_EXAMPLE1.replace("${UVC_CAMERA_REPO}", &uvc_repo_path.display().to_string());

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &launcher_json5,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    assert!(
        result.success,
        "launch should succeed, got error: {:?}",
        result.error_message
    );

    assert!(node_stack.contains(UVC_NODE_NAME, NODE_TAG));
    assert!(node_stack.contains(ROBOT_NODE_NAME, NODE_TAG));
    assert_eq!(node_stack.len(), 3, "root + 2 deployed nodes");

    let uvc = node_stack
        .find(UVC_NODE_NAME, NODE_TAG)
        .expect("uvc_camera should be in stack");
    assert_eq!(
        uvc.instances().len(),
        2,
        "uvc_camera should have 2 instances"
    );

    let brain = node_stack
        .find(ROBOT_NODE_NAME, NODE_TAG)
        .expect("robot_brain should be in stack");
    assert_eq!(
        brain.instances().len(),
        1,
        "robot_brain should have 1 instance"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let existing_path = write_node_config(
        nodes_dir.path(),
        EXISTING_NODE,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    let bad_launcher_json5 = "{ deployments: [ }";

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        bad_launcher_json5,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    assert!(!result.success, "launch should fail for invalid json5");
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not be mutated on invalid json5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_nodes_directory_must_be_a_directory() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let existing_path = write_node_config(
        nodes_dir.path(),
        EXISTING_NODE,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    let bad_nodes_dir = nodes_dir.path().join("not_a_dir.txt");
    fs::write(&bad_nodes_dir, "hello").expect("failed to write file");

    let launcher_json5 = r#"{
            deployments: [
                {
                    name: "some_node",
                    tag: "0.1.0",
                    instances: [{ instance_id: "x" }]
                }
            ]
        }"#
    .to_string();

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &launcher_json5,
        &bad_nodes_dir,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    assert!(
        !result.success,
        "launch should fail when nodes_directory is a file"
    );
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not be mutated on invalid nodes_directory"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let existing_path = write_node_config(
        nodes_dir.path(),
        EXISTING_NODE,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // One deployment exists, the other is missing and required.
    let launcher_json5 = r#"
    {
      deployments: [
        { name: "existing_node", tag: "0.1.0", instances: [ { instance_id: "ok" } ] },
        { name: "missing_node", tag: "0.1.0", instances: [ { instance_id: "nope" } ] }
      ]
    }
    "#;

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launcher_json5,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    assert!(
        !result.success,
        "launch should fail when a required deployment is missing"
    );
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not apply a partial plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    const UVC_NODE_NAME: &str = "uvc_camera";
    const ROBOT_NODE_NAME: &str = "robot_brain";
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let _uvc_path = write_node_config(
        nodes_dir.path(),
        UVC_NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        // Intentionally do NOT expose camera_stream so dependency validation fails.
        false,
    );
    let _brain_path = write_node_config(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        true,
        false,
    );

    // Seed stack with a node so we can assert rollback behavior.
    let existing_path = write_node_config(
        nodes_dir.path(),
        "existing_node",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    let launcher_json5 = r#"
    {
      deployments: [
        { name: "uvc_camera", tag: "0.1.0", instances: [ { instance_id: "camera_front" } ] },
        { name: "robot_brain", tag: "0.1.0", instances: [ { instance_id: "main_robot_brain" } ] }
      ]
    }
    "#;

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launcher_json5,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    assert!(
        !result.success,
        "launch should fail due to dependency mismatch"
    );
    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should not be mutated on dependency failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();
    let nodes_dir = tempdir().expect("failed to create nodes dir");

    let _node_a_path = write_node_config(
        nodes_dir.path(),
        "node_a",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let _node_b_path = write_node_config(
        nodes_dir.path(),
        "node_b",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );

    let node_messenger = MessengerHandle::from_shared(started_master.shared_messenger.clone());
    let _ready_a = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "a1",
            "node_a",
        )
        .await
        .expect("ready should start"),
    );
    let _health_a = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_master.master_node_name,
            "a1",
            "node_a",
        )
        .await
        .expect("health should start"),
    );
    let _ready_b = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("ready should start"),
    );
    let _health_b = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_master.master_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("health should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let launch_a = r#"
    { deployments: [ { name: "node_a", tag: "0.1.0", instances: [ { instance_id: "a1" } ] } ] }
    "#;
    let (_goal_a, result_a) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launch_a,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("first launch should complete");
    assert!(result_a.success, "first launch should succeed");
    assert!(node_stack.contains("node_a", NODE_TAG));

    let launch_b = r#"
    { deployments: [ { name: "node_b", tag: "0.1.0", instances: [ { instance_id: "b1" } ] } ] }
    "#;
    let (_goal_b, result_b) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launch_b,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("second launch should complete");
    assert!(result_b.success, "second launch should succeed");

    assert!(
        !node_stack.contains("node_a", NODE_TAG),
        "second request should replace existing stack (remove node_a)"
    );
    assert!(
        node_stack.contains("node_b", NODE_TAG),
        "second request should replace existing stack (add node_b)"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_one_node_never_becomes_healthy() {
    const NODE_TAG: &str = "0.1.0";

    // Use a short health timeout so the test doesn't take too long.
    let started_master = start_master_node_with_health_timeout(Duration::from_secs(2)).await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");

    // Seed stack with an existing node so we can verify rollback.
    let existing_path = write_node_config(
        nodes_dir.path(),
        "existing_node",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // Node to be launched.
    let _node_b_path = write_node_config(
        nodes_dir.path(),
        "node_b",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );

    // Set up ONLY the ready responder. Do not set up health responder so it times out.
    let node_messenger = MessengerHandle::from_shared(started_master.shared_messenger.clone());
    let _ready_b = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_master.master_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("ready should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let launch_b = r#"
    { deployments: [ { name: "node_b", tag: "0.1.0", instances: [ { instance_id: "b1" } ] } ] }
    "#;

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launch_b,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        Duration::from_secs(30),
    )
    .await
    .expect("launch should complete");

    assert!(
        !result.success,
        "launch should fail because the node never becomes healthy"
    );

    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should be restored on failure"
    );
    assert!(
        !node_stack.contains("node_b", NODE_TAG),
        "node_b should not be present after failed launch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_add_cmd_fails_and_restores_stack() {
    const NODE_TAG: &str = "0.1.0";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");

    // Seed stack with an existing node so we can verify rollback.
    let existing_path = write_node_config(
        nodes_dir.path(),
        "existing_node",
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let existing_config = NodeConfigParser::from_path(existing_path.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // Node with a failing add_cmd.
    let _failing_node_path = write_node_config_with_options(
        nodes_dir.path(),
        "failing_node",
        NODE_TAG,
        "test-hash",
        NodeConfigOptions {
            add_cmd: &["false"], // This command always fails with exit code 1
            start_cmd: &["sleep", "60"],
            ..Default::default()
        },
    );

    let launcher_json5 = r#"
    { deployments: [ { name: "failing_node", tag: "0.1.0", instances: [ { instance_id: "f1" } ] } ] }
    "#;

    let (_goal_response, result) = send_launch_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        launcher_json5,
        nodes_dir.path(),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    assert!(!result.success, "launch should fail because add_cmd fails");

    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should be restored on add_cmd failure"
    );
    assert!(
        !node_stack.contains("failing_node", NODE_TAG),
        "failing_node should not be present after failed launch"
    );
}
