mod common;

use common::{AbortOnDrop, CALLER_INSTANCE_ID, start_core_node_with_health_timeout};
use config::consts::{NODE_CONFIG_FILE, PEPPY_OUTPUT_DIR, PEPPYGEN_OUTPUT_PATH};
use config::node::NodeConfigParser;
use core_node::names;
use core_node_api::encoding::{
    LaunchFeedback, LaunchGoal, LaunchGoalResponse, LaunchResult, LauncherOrigin,
};
use git2::{Repository, Signature};
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use peppylib::{ActionMessenger, PeppyError};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

use crate::common::{TestPackagesCache, start_core_node_with_mock_messenger};

struct NodeConfigOptions<'a> {
    build_cmd: &'a [&'a str],
    run_cmd: &'a [&'a str],
    expects_uvc_camera: bool,
    emits_camera_stream: bool,
}

impl Default for NodeConfigOptions<'_> {
    fn default() -> Self {
        Self {
            build_cmd: &["true"],
            run_cmd: &[],
            expects_uvc_camera: false,
            emits_camera_stream: false,
        }
    }
}

const LAUNCHER_EXAMPLE1: &str = r#"
{
  peppy_schema: "launcher_v1",
  deployments: [
    {
      source: {
        repo: "${UVC_CAMERA_REPO}",
        path: "uvc_camera",
        ref: "0.1.0"
      },
      instances: [
        {
          instance_id: "camera_front",
          arguments: {
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
          arguments: {
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
      source: { local: "./robot_brain" },
      instances: [
        {
          instance_id: "main_robot_brain",
          arguments: {}
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
    run_cmd: &[&str],
    expects_uvc_camera: bool,
    emits_camera_stream: bool,
) -> PathBuf {
    write_node_config_with_options(
        nodes_directory,
        node_name,
        node_tag,
        git_hash,
        NodeConfigOptions {
            run_cmd,
            expects_uvc_camera,
            emits_camera_stream,
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
        build_cmd,
        run_cmd,
        expects_uvc_camera,
        emits_camera_stream,
    } = options;
    let node_dir = nodes_directory.join(node_name);
    fs::create_dir_all(&node_dir).expect("failed to create node directory");

    let build_cmd_json5 = build_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("build_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");

    let run_cmd_json5 = run_cmd
        .iter()
        .map(|arg| serde_json::to_string(arg).expect("run_cmd arg should serialize"))
        .collect::<Vec<_>>()
        .join(", ");

    let emits_topics = if emits_camera_stream {
        r#"emits: [
                  { name: "camera_stream" }
                ],"#
    } else {
        ""
    };

    let expects_topics = if expects_uvc_camera {
        r#"consumes: [
                  { local_node_id: "uvc_camera", name: "camera_stream" }
                ],"#
    } else {
        ""
    };

    let interfaces = if emits_camera_stream || expects_uvc_camera {
        format!(
            r#"interfaces: {{
                topics: {{
                  {emits_topics}
                  {expects_topics}
                }}
              }},"#
        )
    } else {
        String::new()
    };

    let depends_on = if expects_uvc_camera {
        r#"depends_on: {
                    nodes: [
                        { name: "uvc_camera", tag: "0.1.0", local_id: "uvc_camera" }
                    ]
                },"#
    } else {
        ""
    };

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(
        &node_config_path,
        r#"{
              peppy_schema: "node_v1",
              manifest: {
                name: "{node_name}",
                tag: "{node_tag}",
                {depends_on}
              },
              {interfaces}
              execution: {
                language: "rust",
                build_cmd: [{build_cmd_json5}],
                run_cmd: [{run_cmd_json5}]
              }
            }"#
        .replace("{node_name}", node_name)
        .replace("{node_tag}", node_tag)
        .replace("{depends_on}", depends_on)
        .replace("{build_cmd_json5}", &build_cmd_json5)
        .replace("{run_cmd_json5}", &run_cmd_json5)
        .replace("{interfaces}", &interfaces),
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
        r#"{
              peppy_schema: "node_v1",
              manifest: {
                name: "uvc_camera",
                tag: "{node_tag}",
              },
              interfaces: {
                topics: {
                  emits: [
                    { name: "camera_stream" }
                  ]
                }
              },
              execution: {
                language: "rust",
                build_cmd: ["true"],
                run_cmd: ["sleep", "60"]
              }
            }"#
        .replace("{node_tag}", node_tag),
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

async fn send_node_launch_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    peppy_launch_file_path: &Path,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> Result<(LaunchGoalResponse, LaunchResult), String> {
    send_launch_origin_and_wait(
        messenger,
        core_node_name,
        LauncherOrigin::Fs(peppy_launch_file_path.to_path_buf()),
        goal_timeout,
        result_timeout,
        vec![],
    )
    .await
}

async fn send_node_launch_and_wait_with_env(
    messenger: &MessengerHandle,
    core_node_name: &str,
    peppy_launch_file_path: &Path,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
) -> Result<(LaunchGoalResponse, LaunchResult), String> {
    send_launch_origin_and_wait(
        messenger,
        core_node_name,
        LauncherOrigin::Fs(peppy_launch_file_path.to_path_buf()),
        goal_timeout,
        result_timeout,
        env_vars,
    )
    .await
}

async fn send_launch_origin_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    launcher_origin: LauncherOrigin,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
) -> Result<(LaunchGoalResponse, LaunchResult), String> {
    let goal = LaunchGoal::new(launcher_origin, 300, 300, 300, Some(3600)).with_env_vars(env_vars);
    let goal_payload = goal
        .encode()
        .map_err(|e| format!("Failed to encode launch goal: {e}"))?;

    let mut action_handle = ActionMessenger::send_goal(
        messenger,
        core_node_name,
        CALLER_INSTANCE_ID,
        core_node_name,
        names::STACK_LAUNCH_ACTION,
        None,
        None,
        goal_payload,
        config::node::QoSProfile::default(),
        goal_timeout,
    )
    .await
    .map_err(|e| format!("Failed to send launch goal: {e}"))?;

    let goal_response_payload = action_handle.goal_response().payload();
    let goal_response = LaunchGoalResponse::decode(&goal_response_payload)
        .map_err(|e| format!("Failed to decode goal response: {e}"))?;

    if !goal_response.accepted {
        return Err(goal_response
            .rejection_reason
            .unwrap_or_else(|| "launch goal rejected".to_string()));
    }

    let absolute_deadline = tokio::time::Instant::now() + result_timeout;
    let mut last_activity = tokio::time::Instant::now();

    loop {
        // Drain feedback so the publisher doesn't block on a full channel.
        loop {
            let now = tokio::time::Instant::now();
            if now >= absolute_deadline {
                return Err("Timeout waiting for launch result".to_string());
            }
            if now.duration_since(last_activity) >= result_timeout {
                return Err("Timeout waiting for launch result (idle)".to_string());
            }
            let drain_timeout = Duration::from_millis(50);
            match tokio::time::timeout(drain_timeout, action_handle.on_next_feedback()).await {
                Ok(Ok(msg)) => {
                    last_activity = tokio::time::Instant::now();
                    let payload = msg.payload();
                    let _ = LaunchFeedback::decode(&payload);
                }
                Ok(Err(_)) => break,
                Err(_) => break,
            }
        }

        let now = tokio::time::Instant::now();
        if now >= absolute_deadline {
            return Err("Timeout waiting for launch result".to_string());
        }
        if now.duration_since(last_activity) >= result_timeout {
            return Err("Timeout waiting for launch result (idle)".to_string());
        }
        let poll_timeout = Duration::from_millis(200);

        match ActionMessenger::request_result(messenger, &action_handle, poll_timeout).await {
            Ok(msg) => {
                let payload = msg.payload();
                match LaunchResult::decode(&payload) {
                    Ok(result) => return Ok((goal_response, result)),
                    Err(err) => {
                        if !peppylib::encoding::is_result_pending(payload.as_ref()) {
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
async fn listen_for_launch_configuration_succeed_with_complex_dependencies() {
    const FAKE_UVC_CAMERA: &str = "fake_uvc_camera";
    const FAKE_ROBOT_BRAIN: &str = "fake_robot_brain";
    const FAKE_OPENARM01_CONTROLLER: &str = "fake_openarm01_controller";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));

    // Copy launch assets to a temp directory. The stack_launch process will generate
    // peppygen files (including git.hash) automatically using the "stack-launch" marker.
    let temp_dir = tempdir().expect("failed to create temp directory");
    let source_assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/launch_assets");
    for node_name in [FAKE_UVC_CAMERA, FAKE_ROBOT_BRAIN, FAKE_OPENARM01_CONTROLLER] {
        let source_node_dir = source_assets_dir.join(node_name);
        let dest_node_dir = temp_dir.path().join(node_name);
        fs::create_dir_all(&dest_node_dir).expect("failed to create node directory");
        let dest_config_path = dest_node_dir.join(NODE_CONFIG_FILE);
        fs::copy(source_node_dir.join(NODE_CONFIG_FILE), &dest_config_path)
            .expect("failed to copy node config");
    }
    let launch_file_path = temp_dir.path().join("peppy_launcher.json5");
    fs::copy(
        source_assets_dir.join("peppy_launcher.json5"),
        &launch_file_path,
    )
    .expect("failed to copy launcher file");

    // Set up ready/health responders for all instances in the launcher config.
    let _ready_camera_front = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_front",
            FAKE_UVC_CAMERA,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_camera_front = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_front",
            FAKE_UVC_CAMERA,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_camera_rear = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_rear",
            FAKE_UVC_CAMERA,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_camera_rear = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_rear",
            FAKE_UVC_CAMERA,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_brain = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_brain",
            FAKE_ROBOT_BRAIN,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_brain = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_brain",
            FAKE_ROBOT_BRAIN,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_controller = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_nervous_system",
            FAKE_OPENARM01_CONTROLLER,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_controller = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_nervous_system",
            FAKE_OPENARM01_CONTROLLER,
        )
        .await
        .expect("health service should start"),
    );

    // Allow listeners to establish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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

    assert!(node_stack.contains(FAKE_UVC_CAMERA, NODE_TAG));
    assert!(node_stack.contains(FAKE_ROBOT_BRAIN, NODE_TAG));
    assert!(node_stack.contains(FAKE_OPENARM01_CONTROLLER, NODE_TAG));
    assert_eq!(node_stack.len(), 4, "root + 3 deployed nodes");

    let uvc_camera = node_stack
        .find(FAKE_UVC_CAMERA, NODE_TAG)
        .expect("fake_uvc_camera should be in stack");
    assert_eq!(
        uvc_camera.read().instances().len(),
        2,
        "fake_uvc_camera should have 2 instances"
    );

    let robot_brain = node_stack
        .find(FAKE_ROBOT_BRAIN, NODE_TAG)
        .expect("fake_robot_brain should be in stack");
    assert_eq!(
        robot_brain.read().instances().len(),
        1,
        "fake_robot_brain should have 1 instance"
    );

    let controller = node_stack
        .find(FAKE_OPENARM01_CONTROLLER, NODE_TAG)
        .expect("fake_openarm01_controller should be in stack");
    assert_eq!(
        controller.read().instances().len(),
        1,
        "fake_openarm01_controller should have 1 instance"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const UVC_NODE_NAME: &str = "uvc_camera";
    const ROBOT_NODE_NAME: &str = "robot_brain";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

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
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready_front = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_front",
            UVC_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_front = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_front",
            UVC_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_rear = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_rear",
            UVC_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_rear = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "camera_rear",
            UVC_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_brain = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "main_robot_brain",
            ROBOT_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_brain = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
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
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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
        uvc.read().instances().len(),
        2,
        "uvc_camera should have 2 instances"
    );

    let brain = node_stack
        .find(ROBOT_NODE_NAME, NODE_TAG)
        .expect("robot_brain should be in stack");
    assert_eq!(
        brain.read().instances().len(),
        1,
        "robot_brain should have 1 instance"
    );

    // Each deployed node should have an add log entry with a valid path and not marked as failed.
    assert_eq!(
        result.node_add_logs.len(),
        2,
        "should have 2 add log entries (uvc_camera and robot_brain)"
    );
    assert!(
        result
            .node_add_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all add log paths should be non-empty, got: {:?}",
        result
            .node_add_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_add_logs.iter().all(|e| !e.failed),
        "no add log entry should be marked failed, got: {:?}",
        result
            .node_add_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.node_label)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_add_logs.iter().all(|e| e.log_path.exists()),
        "all add log files should exist on disk, missing: {:?}",
        result
            .node_add_logs
            .iter()
            .filter(|e| !e.log_path.exists())
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );

    // Each deployed node should have a build log entry with a valid path and not marked as failed.
    assert_eq!(
        result.node_build_logs.len(),
        2,
        "should have 2 build log entries (uvc_camera and robot_brain)"
    );
    assert!(
        result
            .node_build_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all build log paths should be non-empty, got: {:?}",
        result
            .node_build_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_build_logs.iter().all(|e| !e.failed),
        "no build log entry should be marked failed, got: {:?}",
        result
            .node_build_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.node_label)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_build_logs.iter().all(|e| e.log_path.exists()),
        "all build log files should exist on disk, missing: {:?}",
        result
            .node_build_logs
            .iter()
            .filter(|e| !e.log_path.exists())
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );

    // Each started instance should have a start log entry with a valid path and not marked as failed.
    assert_eq!(
        result.node_run_logs.len(),
        3,
        "should have 3 start log entries (camera_front, camera_rear, main_robot_brain)"
    );
    assert!(
        result
            .node_run_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all start log paths should be non-empty, got: {:?}",
        result
            .node_run_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_run_logs.iter().all(|e| !e.failed),
        "no start log entry should be marked failed, got: {:?}",
        result
            .node_run_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.instance_id)
            .collect::<Vec<_>>()
    );
}

/// Launch file references nodes by `{ name, tag }` (long form) and
/// `{ name: "foo:tag" }` (combined form). Both shapes resolve through the
/// user's packages cache, which we pre-populate with local filesystem
/// entries so the test needs no network access.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeeds_with_repo_sources() {
    const BRAIN_NODE: &str = "repo_brain";
    const ARM_NODE: &str = "repo_arm";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));

    // On-disk node configs referenced by the packages cache.
    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let brain_dir = write_node_config(
        nodes_dir.path(),
        BRAIN_NODE,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );
    let arm_dir = write_node_config(
        nodes_dir.path(),
        ARM_NODE,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );

    // Map both nodes into the user-repo cache. This is what the
    // `{ name, tag }` launcher shape resolves against.
    TestPackagesCache::new()
        .fs_entry(BRAIN_NODE, NODE_TAG, &brain_dir, &[])
        .fs_entry(ARM_NODE, NODE_TAG, &arm_dir, &[])
        .write(&peppy_dirs);

    let _ready_brain = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_brain",
            BRAIN_NODE,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_brain = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_brain",
            BRAIN_NODE,
        )
        .await
        .expect("health service should start"),
    );
    let _ready_arm = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_arm",
            ARM_NODE,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_arm = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "the_arm",
            ARM_NODE,
        )
        .await
        .expect("health service should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Real peppy_launcher.json5 file exercising both the long-form and the
    // `name:tag` shorthand.
    let launcher_json5 = format!(
        r#"{{
  peppy_schema: "launcher_v1",
  deployments: [
    {{
      source: {{ name: "{BRAIN_NODE}", tag: "{NODE_TAG}" }},
      instances: [
        {{ instance_id: "the_brain" }}
      ]
    }},
    {{
      source: {{ name: "{ARM_NODE}:{NODE_TAG}" }},
      instances: [
        {{ instance_id: "the_arm" }}
      ]
    }}
  ]
}}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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

    assert!(
        node_stack.contains(BRAIN_NODE, NODE_TAG),
        "brain should be in stack"
    );
    assert!(
        node_stack.contains(ARM_NODE, NODE_TAG),
        "arm should be in stack"
    );
    assert_eq!(node_stack.len(), 3, "root + 2 repo-sourced nodes");

    let brain = node_stack
        .find(BRAIN_NODE, NODE_TAG)
        .expect("brain should be in stack");
    assert_eq!(brain.read().instances().len(), 1);

    let arm = node_stack
        .find(ARM_NODE, NODE_TAG)
        .expect("arm should be in stack");
    assert_eq!(arm.read().instances().len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    let bad_launcher_json5 = r#"{ peppy_schema: "launcher_v1", deployments: [ }"#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, bad_launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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
async fn listen_for_launch_configuration_launch_file_path_must_be_a_file() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // Create a file (not a directory) to use as the "launch file"
    let bad_launch_file = nodes_dir.path().join("not_a_file_dir");
    fs::create_dir_all(&bad_launch_file).expect("failed to create directory");
    // Point to a path that is a directory, not a file
    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &bad_launch_file,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    assert!(
        !result.success,
        "launch should fail when launch file path is a directory"
    );
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not be mutated on invalid launch file path"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // One deployment exists, the other is missing and required.
    let launcher_json5 = r#"
    {
      peppy_schema: "launcher_v1",
      deployments: [
        { source: { local: "./existing_node" }, instances: [ { instance_id: "ok" } ] },
        { source: { local: "./missing_node" }, instances: [ { instance_id: "nope" } ] }
      ]
    }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let _uvc_path = write_node_config(
        nodes_dir.path(),
        UVC_NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        // Intentionally do NOT emit camera_stream so dependency validation fails.
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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    let launcher_json5 = r#"
    {
      peppy_schema: "launcher_v1",
      deployments: [
        { source: { local: "./uvc_camera" }, instances: [ { instance_id: "camera_front" } ] },
        { source: { local: "./robot_brain" }, instances: [ { instance_id: "main_robot_brain" } ] }
      ]
    }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
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

    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready_a = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "a1",
            "node_a",
        )
        .await
        .expect("ready should start"),
    );
    let _health_a = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "a1",
            "node_a",
        )
        .await
        .expect("health should start"),
    );
    let _ready_b = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("ready should start"),
    );
    let _health_b = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("health should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let launch_a = r#"
    { peppy_schema: "launcher_v1", deployments: [ { source: { local: "./node_a" }, instances: [ { instance_id: "a1" } ] } ] }
    "#;
    let launch_file_path_a = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path_a, launch_a).expect("failed to write launch file");
    let (_goal_a, result_a) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path_a,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("first launch should complete");
    assert!(result_a.success, "first launch should succeed");
    assert!(node_stack.contains("node_a", NODE_TAG));

    let launch_b = r#"
    { peppy_schema: "launcher_v1", deployments: [ { source: { local: "./node_b" }, instances: [ { instance_id: "b1" } ] } ] }
    "#;
    let launch_file_path_b = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path_b, launch_b).expect("failed to write launch file");
    let (_goal_b, result_b) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path_b,
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
    let started_core_node = start_core_node_with_health_timeout(Duration::from_secs(2)).await;
    let node_stack = started_core_node.node_stack.clone();

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
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
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready_b = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "b1",
            "node_b",
        )
        .await
        .expect("ready should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    let launch_b = r#"
    { peppy_schema: "launcher_v1", deployments: [ { source: { local: "./node_b" }, instances: [ { instance_id: "b1" } ] } ] }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launch_b).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
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
async fn listen_for_launch_configuration_fails_when_build_cmd_fails_and_restores_stack() {
    const NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // Node with a failing build_cmd.
    let _failing_node_path = write_node_config_with_options(
        nodes_dir.path(),
        "failing_node",
        NODE_TAG,
        "test-hash",
        NodeConfigOptions {
            build_cmd: &["false"], // This command always fails with exit code 1
            run_cmd: &["sleep", "60"],
            ..Default::default()
        },
    );

    let launcher_json5 = r#"
    { peppy_schema: "launcher_v1", deployments: [ { source: { local: "./failing_node" }, instances: [ { instance_id: "f1" } ] } ] }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    assert!(
        !result.success,
        "launch should fail because build_cmd fails"
    );

    let error_message = result
        .error_message
        .expect("error_message should be set on build_cmd failure");
    assert!(
        error_message.contains("failing_node:0.1.0"),
        "error message should contain the node name:tag, got: {error_message}"
    );

    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should be restored on build_cmd failure"
    );
    assert!(
        !node_stack.contains("failing_node", NODE_TAG),
        "failing_node should not be present after failed launch"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_run_cmd_exits_with_error() {
    const NODE_NAME: &str = "failing_start_node";
    const NODE_TAG: &str = "0.1.0";
    const INSTANCE_ID: &str = "fs1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");

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
        .expect("existing node config should parse")
        .into_resolved()
        .expect("test config has execution");
    node_stack
        .push_config(existing_config, false, &existing_path)
        .expect("should seed stack");

    // Node whose run_cmd exits immediately with a non-zero status.
    let _failing_node_path = write_node_config_with_options(
        nodes_dir.path(),
        NODE_NAME,
        NODE_TAG,
        "test-hash",
        NodeConfigOptions {
            build_cmd: &["true"],
            run_cmd: &["false"], // exits immediately with status 1
            ..Default::default()
        },
    );

    let launcher_json5 = format!(
        r#"{{ peppy_schema: "launcher_v1", deployments: [ {{ source: {{ local: "./{NODE_NAME}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}" }} ] }} ] }}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");

    let (_goal_response, result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    assert!(
        !result.success,
        "launch should fail because run_cmd exits with error"
    );

    let error_message = result
        .error_message
        .expect("error_message should be set on run_cmd failure");
    assert!(
        error_message.contains(&format!("{}:{}", NODE_NAME, NODE_TAG)),
        "error message should contain the node name:tag, got: {error_message}"
    );
    assert!(
        error_message.contains(INSTANCE_ID),
        "error message should contain the instance ID, got: {error_message}"
    );

    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should be restored on run_cmd failure"
    );
    assert!(
        !node_stack.contains(NODE_NAME, NODE_TAG),
        "{NODE_NAME} should not be present after failed launch"
    );

    // The node was successfully added before the start failed, so we expect one add log entry.
    assert_eq!(
        result.node_add_logs.len(),
        1,
        "should have 1 add log entry for the node that was added before start failed"
    );
    assert!(
        !result.node_add_logs[0].failed,
        "add log entry should not be marked failed (add succeeded)"
    );
    assert_eq!(
        result.node_add_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );

    // The node was successfully built before the start failed, so we expect one build log entry.
    assert_eq!(
        result.node_build_logs.len(),
        1,
        "should have 1 build log entry for the node that was built before start failed"
    );
    assert!(
        !result.node_build_logs[0].failed,
        "build log entry should not be marked failed (build succeeded)"
    );
    assert_eq!(
        result.node_build_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );

    // The failed start should produce one start log entry marked as failed.
    assert_eq!(
        result.node_run_logs.len(),
        1,
        "should have 1 start log entry for the failed instance"
    );
    assert!(
        result.node_run_logs[0].failed,
        "start log entry should be marked failed"
    );
    assert_eq!(result.node_run_logs[0].instance_id, INSTANCE_ID);
    assert_eq!(
        result.node_run_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );
    assert!(
        !result.node_run_logs[0].log_path.as_os_str().is_empty(),
        "start log path should be non-empty"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_launch_uses_env_overrides_for_path() {
    // Emulates a real case scenario where the caller environment differs from the already-running
    // daemon environment. In practice, users often "install a tool then source it" (e.g.
    // `. "$HOME/.cargo/env"`), but that only affects their shell, not the daemon. We model this by
    // passing a PATH override in the goal on the second attempt.
    const NODE_NAME: &str = "node_b";
    const NODE_TAG: &str = "0.1.0";
    const INSTANCE_ID: &str = "b1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let _node_path = write_node_config(
        nodes_dir.path(),
        NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["printout", "60"], // run_cmd that sleeps via printout
        false,
        false,
    );
    let launch_json5 = format!(
        r#"{{ peppy_schema: "launcher_v1", deployments: [ {{ source: {{ local: "./{NODE_NAME}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}" }} ] }} ] }}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launch_json5).expect("failed to write launch file");

    // `printout` does not exist in the system when this is run
    let (_, launch_result) = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch request should complete");

    assert!(
        !launch_result.success,
        "The launch should fail, printout does not exist: {:?}",
        launch_result.error_message
    );

    // Create a temp bin directory with a `printout` script that sleeps
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    let printout_path = bin_dir.path().join("printout");
    std::fs::write(&printout_path, "#!/bin/sh\nsleep \"${1:-60}\"\n")
        .expect("failed to write printout script");

    // Make it executable
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&printout_path)
            .expect("failed to get printout metadata")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&printout_path, perms)
            .expect("failed to set printout permissions");
    }

    // Set up ready/health responders for the instance
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            INSTANCE_ID,
            NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            INSTANCE_ID,
            NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );

    // Allow listeners to establish.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Pass the bin directory in PATH via env overrides to simulate the caller having an updated
    // PATH without restarting the daemon.
    let current_path = std::env::var("PATH").unwrap_or_default();
    let new_path = format!("{}:{}", bin_dir.path().display(), current_path);
    let env_vars = vec![("PATH".to_string(), new_path)];

    let (_, launch_result) = send_node_launch_and_wait_with_env(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        env_vars,
    )
    .await
    .expect("launch request should complete");

    // Now the launch should succeed, since `printout` is available in the PATH override
    assert!(
        launch_result.success,
        "The launch should succeed, got error: {:?}",
        launch_result.error_message
    );
}

/// `LauncherOrigin::Repository` resolves the launcher name through `launchers.json5`
/// (the launcher repo cache) and uses the on-disk path it points at. The deployment
/// itself uses a local source so we don't need to spin up a fake git repo.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_resolves_launcher_from_repository_cache() {
    const ROBOT_NODE_NAME: &str = "robot_brain_repo";
    const NODE_TAG: &str = "0.1.0";
    const LAUNCHER_NAME: &str = "robot_brain_demo";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();
    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let _brain_path = write_node_config(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        "test-hash",
        &["sleep", "60"],
        false,
        false,
    );

    let _ready_brain = AbortOnDrop(
        listen_for_node_ready(
            &node_messenger,
            &started_core_node.core_node_name,
            "main_robot_brain",
            ROBOT_NODE_NAME,
        )
        .await
        .expect("ready service should start"),
    );
    let _health_brain = AbortOnDrop(
        listen_for_node_health(
            &node_messenger,
            &started_core_node.core_node_name,
            "main_robot_brain",
            ROBOT_NODE_NAME,
        )
        .await
        .expect("health service should start"),
    );
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Launcher file references the node by its directory name (`local: "./robot_brain_repo"`),
    // resolved relative to the launcher file's parent dir.
    let launcher_json5 = format!(
        r#"{{
  peppy_schema: "launcher_v1",
  deployments: [
    {{
      source: {{ local: "./{ROBOT_NODE_NAME}" }},
      instances: [
        {{ instance_id: "main_robot_brain" }}
      ]
    }}
  ]
}}"#
    );
    let launcher_file_path = nodes_dir.path().join(format!("{LAUNCHER_NAME}.json5"));
    fs::write(&launcher_file_path, &launcher_json5).expect("failed to write launcher file");

    // Hand-write the launcher cache so the daemon can map the bare name back to the file.
    let cache_entries = serde_json::json!([
        {
            "launcher_name": LAUNCHER_NAME,
            "source_type": "fs",
            "path": launcher_file_path.to_string_lossy(),
        }
    ]);
    let cache_dir = peppy_dirs.cache_dir();
    fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
    fs::write(
        cache_dir.join("launchers.json5"),
        serde_json::to_string_pretty(&cache_entries).expect("serialize cache"),
    )
    .expect("failed to write launchers.json5");

    let (_goal_response, result) = send_launch_origin_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        LauncherOrigin::Repository {
            name: LAUNCHER_NAME.to_string(),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        vec![],
    )
    .await
    .expect("launch should complete");

    assert!(
        result.success,
        "launch should succeed, got error: {:?}",
        result.error_message
    );
    assert!(
        node_stack.contains(ROBOT_NODE_NAME, NODE_TAG),
        "robot_brain_repo should be in stack"
    );
}

/// When the `Repository` origin names a launcher that is not in the cache, the daemon
/// surfaces an explicit error pointing at `launchers.json5` and does not fall back to
/// any filesystem lookup.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_repository_miss_returns_explicit_error() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let peppy_dirs = started_core_node.peppy_dirs.clone();

    // Empty launcher cache: any name lookup must miss.
    let cache_dir = peppy_dirs.cache_dir();
    fs::create_dir_all(&cache_dir).expect("failed to create cache dir");
    fs::write(
        cache_dir.join("launchers.json5"),
        serde_json::to_string(&serde_json::Value::Array(vec![])).unwrap(),
    )
    .expect("failed to write launchers.json5");

    let (_goal_response, result) = send_launch_origin_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        LauncherOrigin::Repository {
            name: "definitely_missing_launcher".to_string(),
        },
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        vec![],
    )
    .await
    .expect("launch should return a result");

    assert!(!result.success, "missing repository launcher should fail");
    let err = result.error_message.unwrap_or_default();
    assert!(
        err.contains("launcher `definitely_missing_launcher` not found"),
        "expected explicit not-found error, got: {err}"
    );
    assert!(
        err.contains("launchers.json5"),
        "error should reference the launcher cache path, got: {err}"
    );
}
