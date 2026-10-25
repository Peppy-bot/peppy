mod common;
#[path = "listen_for_launch/copies.rs"]
mod copies;
#[path = "listen_for_launch/daemon_targets.rs"]
mod daemon_targets;
#[path = "listen_for_launch/fleet.rs"]
mod fleet;
#[path = "listen_for_launch/stack_copies.rs"]
mod stack_copies;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, FeedbackDrainOutcome, GOAL_TIMEOUT, GoalEnd, RESULT_TIMEOUT,
    assert_failure, drain_feedback, fetch_end, goal_answer, send_daemon_goal,
    start_core_node_with_default_setup_timeout,
};
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::failure_kind::PeppyFailureKind;
use config::node::NodeConfigParser;
use config::runtime::Name;
use core_node_api::DaemonFailureKind;
use core_node_api::encoding::{
    LaunchFeedback, LaunchGoal, LaunchGoalResponse, LaunchOutput, LaunchResult, LauncherOrigin,
    NodeLogs, StackBudgets,
};
use daemon_config::consts::PEPPY_OUTPUT_DIR;
use git2::{Repository, Signature};
use peppylib::messaging::{ActionGoalHandle, Failure, MessengerHandle};
use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tempfile::tempdir;

use crate::common::{
    StartedCoreNode, TestPackagesCache, TestRunningInstance, answer_readiness, answer_ready_probe,
    commit_path, is_process_running, path_env_with_dir_first, poll_until,
    spawn_real_running_instance, start_core_node_with_mock_messenger, wait_until_process_exits,
    write_executable_script,
};

struct NodeConfigOptions<'a> {
    build_cmd: &'a [&'a str],
    run_cmd: &'a [&'a str],
    /// The manifest's `depends_on` object, written as is; empty for none.
    depends_on: &'a str,
    /// The node's `interfaces` object, written as is; empty for none.
    interfaces: &'a str,
}

impl Default for NodeConfigOptions<'_> {
    fn default() -> Self {
        Self {
            build_cmd: &["true"],
            run_cmd: &[],
            depends_on: "",
            interfaces: "",
        }
    }
}

/// Seeds a real, running `sleep 60` instance on the daemon's stack: adds the
/// node, builds it, and starts an instance, returning the source dir + live
/// guard + pid. The launch-failure tests use this to stand up a previous stack
/// that a new launch must tear down (not orphan) when the new launch fails. The
/// returned `TempDir` and guard must be kept alive for the duration of the test
/// so the seeded source and instance are only torn down by the launch under
/// test, not by the guard's stop-on-drop.
async fn seed_running_node(
    started: &StartedCoreNode,
    name: &str,
    tag: &str,
    instance_id: &str,
) -> (tempfile::TempDir, TestRunningInstance, u32) {
    let source_dir = common::add_and_build_process_node(started, name, tag, &["sleep", "60"]).await;
    let id = Name::new(instance_id).expect("valid instance id");
    let guard = spawn_real_running_instance(started, name, tag, &id).await;
    let pid = guard.pid;
    assert!(
        is_process_running(pid),
        "seeded node {name} process {pid} should be running"
    );
    (source_dir, guard, pid)
}

const LAUNCHER_EXAMPLE1: &str = r#"
{
  peppy_schema: "launcher/v1",
  deployments: [
    {
      source: {
        name: "uvc_camera:v1"
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
      source: { name: "robot_brain:v1" },
      instances: [
        {
          instance_id: "main_robot_brain",
          arguments: {},
          links: {
            front_camera: "camera_front",
          },
        }
      ]
    },
  ]
}
"#;

/// The budgets of a stack goal of these tests: no add, build or run of a
/// test node stays idle for that long and the goal has no deadline, so a
/// goal ends by its own outcome on a host of any speed, and the wait of the
/// test is its only bound.
fn unreached_budgets() -> StackBudgets {
    StackBudgets::new(600, 600, 600, None)
}

/// The launch id `send_node_launch_and_wait` launches under.
const TEST_LAUNCH_ID: &str = "launch-test-fixture";

/// The `depends_on` of a node that consumes the `camera_stream` of a
/// `uvc_camera:v1` through its `front_camera` link.
const DEPENDS_ON_FRONT_CAMERA: &str =
    r#"{ nodes: [{ name: "uvc_camera", tag: "v1", link_id: "front_camera" }] }"#;
/// The `interfaces` that go with [`DEPENDS_ON_FRONT_CAMERA`].
const CONSUMES_FRONT_CAMERA_STREAM: &str =
    r#"{ topics: { consumes: [{ link_id: "front_camera", name: "camera_stream" }] } }"#;

fn write_node_config(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    run_cmd: &[&str],
) -> PathBuf {
    write_node_config_with_options(
        nodes_directory,
        node_name,
        node_tag,
        NodeConfigOptions {
            run_cmd,
            ..Default::default()
        },
    )
}

fn write_node_config_with_options(
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
    options: NodeConfigOptions<'_>,
) -> PathBuf {
    let NodeConfigOptions {
        build_cmd,
        run_cmd,
        depends_on: raw_depends_on,
        interfaces: raw_interfaces,
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

    let interfaces = if raw_interfaces.is_empty() {
        String::new()
    } else {
        format!("interfaces: {raw_interfaces},")
    };

    let depends_on = if raw_depends_on.is_empty() {
        String::new()
    } else {
        format!("depends_on: {raw_depends_on},")
    };

    let node_config_path = node_dir.join(NODE_CONFIG_FILE);
    fs::write(
        &node_config_path,
        r#"{
              peppy_schema: "node/v1",
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
        .replace("{depends_on}", &depends_on)
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
    fs::write(peppy_output_dir.join("git.hash"), common::TEST_GIT_HASH)
        .expect("failed to write node git hash");

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
              peppy_schema: "node/v1",
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

    let commit = commit_path(
        &repo,
        &signature,
        &rel_config_path,
        "uvc_camera initial",
        &[],
    );

    // Tag the commit so deployments can reference it via `tag`.
    repo.tag(node_tag, commit.as_object(), &signature, node_tag, false)
        .expect("failed to create tag");

    repo_path
}

/// Writes a `sleep 60` node config and pushes it onto the stack without a
/// build: a node that a refused launch must leave in place. Returns the node's
/// directory.
fn push_unbuilt_node_onto_stack(
    node_stack: &node_stack::NodeStack,
    nodes_directory: &Path,
    node_name: &str,
    node_tag: &str,
) -> PathBuf {
    let node_dir = write_node_config(nodes_directory, node_name, node_tag, &["sleep", "60"]);
    let config = NodeConfigParser::from_path(node_dir.join(NODE_CONFIG_FILE))
        .expect("existing node config should parse");
    node_stack
        .push_config(config, Vec::new(), false, &node_dir)
        .expect("should seed stack");
    node_dir
}

/// How a stack goal that a test sends ends: the launch log that its goal
/// response names, how the goal ended, and the node logs that its last
/// feedback gave.
#[derive(Debug)]
struct StackGoalEnd {
    log_path: PathBuf,
    end: GoalEnd<LaunchResult>,
    node_logs: NodeLogs,
}

impl StackGoalEnd {
    /// Asserts that the stack goal completed.
    #[track_caller]
    fn assert_completed(&self) {
        assert!(
            self.end.is_completed(),
            "the stack goal should complete: {:?}",
            self.end
        );
    }

    /// The failure of a stack goal that failed after its acceptance. Panics
    /// with the ending of any other goal.
    #[track_caller]
    fn failure(&self) -> &Failure {
        self.end.failed()
    }

    /// Asserts that the stack goal failed after its acceptance, with `kind`
    /// and a message that contains `text`.
    #[track_caller]
    fn assert_failed(&self, kind: impl Into<DaemonFailureKind>, text: &str) {
        assert_failure(self.failure(), kind, text);
    }
}

/// Sends the stack goal `goal`, gives each output line of its feedback to
/// `on_output` as it arrives, and returns how the goal ended. An error names
/// a goal that the daemon refused, or that did not end within
/// `result_timeout`.
async fn run_stack_goal(
    messenger: &MessengerHandle,
    core_node_name: &str,
    goal: &impl core_node_api::ActionGoal,
    goal_timeout: Duration,
    result_timeout: Duration,
    on_output: &mut (dyn FnMut(LaunchOutput) + Send),
) -> Result<StackGoalEnd, String> {
    let mut action_handle = send_daemon_goal(messenger, core_node_name, goal, goal_timeout)
        .await
        .map_err(|e| format!("Failed to send the stack goal: {e}"))?;
    let goal_response =
        goal_answer(&action_handle, LaunchGoalResponse::decode, "goal response")?
            .map_err(|refusal| format!("the daemon refused the stack goal: {refusal}"))?;

    let absolute_deadline = tokio::time::Instant::now() + result_timeout;

    // Drain feedback until the server closes the stream on the end of the
    // goal, then fetch how the goal ended once.
    let mut node_logs = NodeLogs::default();
    let drained = drain_feedback(
        &mut action_handle,
        |payload| LaunchFeedback::decode(payload).expect("failed to decode LaunchFeedback"),
        |feedback| {
            match feedback {
                LaunchFeedback::Output(output) => on_output(output),
                LaunchFeedback::NodeLogs(logs) => node_logs = logs,
            }
            ControlFlow::Continue(())
        },
        absolute_deadline,
    )
    .await;
    if matches!(drained, FeedbackDrainOutcome::TimedOut) {
        return Err("Timeout waiting for the end of the stack goal".to_string());
    }

    let fetch_timeout = absolute_deadline
        .saturating_duration_since(tokio::time::Instant::now())
        .max(Duration::from_secs(1));
    let end = fetch_end(
        messenger,
        &action_handle,
        fetch_timeout,
        LaunchResult::decode,
        "launch result",
    )
    .await?;
    Ok(StackGoalEnd {
        log_path: goal_response.log_path,
        end,
        node_logs,
    })
}

/// Sends the stack goal `goal` and returns its live handle. Panics when the
/// daemon refuses the goal.
async fn send_accepted_stack_goal(
    started: &StartedCoreNode,
    goal: &impl core_node_api::ActionGoal,
) -> ActionGoalHandle {
    let handle = send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        goal,
        GOAL_TIMEOUT,
    )
    .await
    .expect("send the stack goal");
    goal_answer(&handle, LaunchGoalResponse::decode, "goal response")
        .expect("decode the goal answer")
        .unwrap_or_else(|refusal| panic!("the daemon refused the stack goal: {refusal}"));
    handle
}

/// Cancels the stack goal of `handle` as its client does, runs
/// `after_cancel` once the daemon answered the cancel, and returns how the
/// goal ended. Panics when the goal does not end within the result budget.
async fn cancel_and_follow_to_end(
    started: &StartedCoreNode,
    mut handle: ActionGoalHandle,
    after_cancel: impl std::future::Future<Output = ()>,
) -> GoalEnd<LaunchResult> {
    peppylib::ActionMessenger::cancel_goal(&started.caller_handle, &handle, GOAL_TIMEOUT)
        .await
        .expect("the daemon answers the cancel");
    after_cancel.await;
    common::wait_until_action_completes(&mut handle, RESULT_TIMEOUT).await;
    fetch_end(
        &started.caller_handle,
        &handle,
        GOAL_TIMEOUT,
        LaunchResult::decode,
        "launch result",
    )
    .await
    .unwrap_or_else(|error| panic!("{error}"))
}

async fn send_node_launch_and_wait(
    messenger: &MessengerHandle,
    core_node_name: &str,
    peppy_launch_file_path: &Path,
    goal_timeout: Duration,
    result_timeout: Duration,
) -> Result<StackGoalEnd, String> {
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
) -> Result<StackGoalEnd, String> {
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
) -> Result<StackGoalEnd, String> {
    send_launch_origin_and_wait_observing(
        messenger,
        core_node_name,
        launcher_origin,
        goal_timeout,
        result_timeout,
        env_vars,
        &mut |_| {},
    )
    .await
}

/// [`send_launch_origin_and_wait`] with every output line of the feedback
/// handed to `on_output` as it arrives, for tests that assert on what the
/// launch reported and not only on how it ended.
async fn send_launch_origin_and_wait_observing(
    messenger: &MessengerHandle,
    core_node_name: &str,
    launcher_origin: LauncherOrigin,
    goal_timeout: Duration,
    result_timeout: Duration,
    env_vars: Vec<(String, String)>,
    on_output: &mut (dyn FnMut(LaunchOutput) + Send),
) -> Result<StackGoalEnd, String> {
    let goal = LaunchGoal::new(
        launcher_origin,
        TEST_LAUNCH_ID,
        unreached_budgets().with_env_vars(env_vars),
    );
    run_stack_goal(
        messenger,
        core_node_name,
        &goal,
        goal_timeout,
        result_timeout,
        on_output,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed_with_complex_dependencies() {
    const FAKE_UVC_CAMERA: &str = "fake_uvc_camera";
    const FAKE_ROBOT_BRAIN: &str = "fake_robot_brain";
    const FAKE_OPENARM01_CONTROLLER: &str = "fake_openarm01_controller";
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();

    // Copy launch assets to a temp directory. The stack_launch process will generate
    // peppygen files (including git.hash) automatically using the "stack-launch" marker.
    let temp_dir = tempdir().expect("failed to create temp directory");
    let source_assets_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/launch_assets");
    let mut cache = TestPackagesCache::new();
    for node_name in [FAKE_UVC_CAMERA, FAKE_ROBOT_BRAIN, FAKE_OPENARM01_CONTROLLER] {
        let source_node_dir = source_assets_dir.join(node_name);
        let dest_node_dir = temp_dir.path().join(node_name);
        fs::create_dir_all(&dest_node_dir).expect("failed to create node directory");
        let dest_config_path = dest_node_dir.join(NODE_CONFIG_FILE);
        fs::copy(source_node_dir.join(NODE_CONFIG_FILE), &dest_config_path)
            .expect("failed to copy node config");
        cache = cache.fs_entry(node_name, NODE_TAG, &dest_node_dir);
    }
    let launch_file_path = temp_dir.path().join("peppy_launcher.json5");
    fs::copy(
        source_assets_dir.join("peppy_launcher.json5"),
        &launch_file_path,
    )
    .expect("failed to copy launcher file");
    cache.write(&peppy_dirs);

    // Set up ready/health responders for all instances in the launcher config.
    let mut responders = Vec::new();
    for (node, instance_id) in [
        (FAKE_UVC_CAMERA, "camera_front"),
        (FAKE_UVC_CAMERA, "camera_rear"),
        (FAKE_ROBOT_BRAIN, "the_brain"),
        (FAKE_OPENARM01_CONTROLLER, "the_nervous_system"),
    ] {
        responders.extend(answer_readiness(&started_core_node, node, instance_id).await);
    }

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    result.assert_completed();

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
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let uvc_repo_path = create_uvc_camera_repo(nodes_dir.path(), NODE_TAG);
    let robot_brain_path = write_node_config_with_options(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        NodeConfigOptions {
            run_cmd: &["sleep", "60"],
            depends_on: DEPENDS_ON_FRONT_CAMERA,
            interfaces: CONSUMES_FRONT_CAMERA_STREAM,
            ..Default::default()
        },
    );

    // Set up ready/health responders for all instances in LAUNCHER_EXAMPLE1.
    let mut responders = Vec::new();
    for (node, instance_id) in [
        (UVC_NODE_NAME, "camera_front"),
        (UVC_NODE_NAME, "camera_rear"),
        (ROBOT_NODE_NAME, "main_robot_brain"),
    ] {
        responders.extend(answer_readiness(&started_core_node, node, instance_id).await);
    }

    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, LAUNCHER_EXAMPLE1).expect("failed to write launch file");

    // uvc_camera resolves through a git-backed cache entry (exercising the
    // clone-at-launch path), robot_brain through a plain fs entry.
    TestPackagesCache::new()
        .git_entry(
            UVC_NODE_NAME,
            NODE_TAG,
            &uvc_repo_path.display().to_string(),
            NODE_TAG,
            "uvc_camera",
        )
        .fs_entry(ROBOT_NODE_NAME, NODE_TAG, &robot_brain_path)
        .write(&peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    result.assert_completed();

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
        result.node_logs.node_add_logs.len(),
        2,
        "should have 2 add log entries (uvc_camera and robot_brain)"
    );
    assert!(
        result
            .node_logs
            .node_add_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all add log paths should be non-empty, got: {:?}",
        result
            .node_logs
            .node_add_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_logs.node_add_logs.iter().all(|e| !e.failed),
        "no add log entry should be marked failed, got: {:?}",
        result
            .node_logs
            .node_add_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.node_label)
            .collect::<Vec<_>>()
    );
    assert!(
        result
            .node_logs
            .node_add_logs
            .iter()
            .all(|e| e.log_path.exists()),
        "all add log files should exist on disk, missing: {:?}",
        result
            .node_logs
            .node_add_logs
            .iter()
            .filter(|e| !e.log_path.exists())
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );

    // Each deployed node should have a build log entry with a valid path and not marked as failed.
    assert_eq!(
        result.node_logs.node_build_logs.len(),
        2,
        "should have 2 build log entries (uvc_camera and robot_brain)"
    );
    assert!(
        result
            .node_logs
            .node_build_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all build log paths should be non-empty, got: {:?}",
        result
            .node_logs
            .node_build_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_logs.node_build_logs.iter().all(|e| !e.failed),
        "no build log entry should be marked failed, got: {:?}",
        result
            .node_logs
            .node_build_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.node_label)
            .collect::<Vec<_>>()
    );
    assert!(
        result
            .node_logs
            .node_build_logs
            .iter()
            .all(|e| e.log_path.exists()),
        "all build log files should exist on disk, missing: {:?}",
        result
            .node_logs
            .node_build_logs
            .iter()
            .filter(|e| !e.log_path.exists())
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );

    // Each started instance should have a start log entry with a valid path and not marked as failed.
    assert_eq!(
        result.node_logs.node_run_logs.len(),
        3,
        "should have 3 start log entries (camera_front, camera_rear, main_robot_brain)"
    );
    assert!(
        result
            .node_logs
            .node_run_logs
            .iter()
            .all(|e| !e.log_path.as_os_str().is_empty()),
        "all start log paths should be non-empty, got: {:?}",
        result
            .node_logs
            .node_run_logs
            .iter()
            .map(|e| &e.log_path)
            .collect::<Vec<_>>()
    );
    assert!(
        result.node_logs.node_run_logs.iter().all(|e| !e.failed),
        "no start log entry should be marked failed, got: {:?}",
        result
            .node_logs
            .node_run_logs
            .iter()
            .filter(|e| e.failed)
            .map(|e| &e.instance_id)
            .collect::<Vec<_>>()
    );

    // Every log entry names the machine whose filesystem holds the file. This
    // launch places everything on the coordinator, so every entry carries its
    // core node.
    let expected_core_node = &started_core_node.core_node_name;
    let all_core_nodes: Vec<&str> = result
        .node_logs
        .node_add_logs
        .iter()
        .map(|e| e.core_node.as_str())
        .chain(
            result
                .node_logs
                .node_build_logs
                .iter()
                .map(|e| e.core_node.as_str()),
        )
        .chain(
            result
                .node_logs
                .node_run_logs
                .iter()
                .map(|e| e.core_node.as_str()),
        )
        .collect();
    assert!(
        all_core_nodes
            .iter()
            .all(|core_node| core_node == expected_core_node),
        "every log entry should carry the coordinator's core node `{expected_core_node}`, got: \
         {all_core_nodes:?}"
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
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();

    // On-disk node configs referenced by the packages cache.
    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let brain_dir = write_node_config(nodes_dir.path(), BRAIN_NODE, NODE_TAG, &["sleep", "60"]);
    let arm_dir = write_node_config(nodes_dir.path(), ARM_NODE, NODE_TAG, &["sleep", "60"]);

    // Map both nodes into the user-repo cache. This is what the
    // `{ name, tag }` launcher shape resolves against.
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    TestPackagesCache::new()
        .fs_entry(BRAIN_NODE, NODE_TAG, &brain_dir)
        .fs_entry(ARM_NODE, NODE_TAG, &arm_dir)
        .write(&peppy_dirs);

    let mut responders = answer_readiness(&started_core_node, BRAIN_NODE, "the_brain").await;
    responders.extend(answer_readiness(&started_core_node, ARM_NODE, "the_arm").await);

    // Real peppy_launcher.json5 file exercising both the long-form and the
    // `name:tag` shorthand.
    let launcher_json5 = format!(
        r#"{{
  peppy_schema: "launcher/v1",
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
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    result.assert_completed();

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
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    push_unbuilt_node_onto_stack(&node_stack, nodes_dir.path(), EXISTING_NODE, NODE_TAG);

    let bad_launcher_json5 = r#"{ peppy_schema: "launcher/v1", deployments: [ }"#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    // No cache fixture: the launcher fails to parse, so no deployment is ever
    // resolved and nothing reads `nodes.json5`.
    fs::write(&launch_file_path, bad_launcher_json5).expect("failed to write launch file");

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    result.assert_failed(PeppyFailureKind::Rejected, "Invalid launcher config");
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not be mutated on invalid json5"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_file_path_must_be_a_file() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    push_unbuilt_node_onto_stack(&node_stack, nodes_dir.path(), EXISTING_NODE, NODE_TAG);

    // A launch path that names a directory, not a file.
    let bad_launch_file = nodes_dir.path().join("not_a_file_dir");
    fs::create_dir_all(&bad_launch_file).expect("failed to create directory");

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &bad_launch_file,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    result.assert_failed(
        PeppyFailureKind::Rejected,
        "launch file path must be a file",
    );
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not be mutated on an invalid launcher path"
    );
}

/// A launcher that parses but whose options fail to load, here because
/// option `real` declares an axis named like the launcher's own `robot`
/// axis, fails the launch and names the reason in the launch feedback and
/// the launch log, as every other refusal of the launcher does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launcher_whose_options_fail_to_load_reports_why_in_the_feedback() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let launcher_dir = tempdir().expect("failed to create launcher dir");
    let launch_file_path = launcher_dir.path().join("peppy_launcher.json5");
    // No cache fixture: the options fail to load before any deployment is
    // resolved, so nothing reads `nodes.json5`.
    fs::write(
        &launch_file_path,
        r#"{
            peppy_schema: "launcher/v1",
            components: [{ name: "robot", cardinality: "zero_or_more", options: {
                real: {
                    deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "arm_inst" }] }],
                    components: [{ name: "robot", options: {
                        arm: { deployments: [{ source: { name: "named_robot", tag: "v1" }, instances: [{ instance_id: "inner_inst" }] }] },
                    } }],
                },
            } }],
        }"#,
    )
    .expect("failed to write launch file");

    let mut feedback_lines: Vec<String> = Vec::new();
    let result = send_launch_origin_and_wait_observing(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        LauncherOrigin::Fs(launch_file_path),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        vec![],
        &mut |output| feedback_lines.push(output.line),
    )
    .await
    .expect("launch should return a result");

    result.assert_failed(
        PeppyFailureKind::Rejected,
        "axis `robot` is declared by both the launcher and inline option `robot.real`",
    );
    let error_message = result.failure().message().to_string();
    assert!(
        feedback_lines
            .iter()
            .any(|line| line.contains(&error_message)),
        "the reason `{error_message}` reaches the launch feedback; feedback:\n{}",
        feedback_lines.join("\n")
    );
    let launch_log = fs::read_to_string(&result.log_path).expect("read the launch log");
    assert!(
        launch_log.contains(&error_message),
        "the reason `{error_message}` is on record in the launch log at {}:\n{launch_log}",
        result.log_path.display()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    const EXISTING_NODE: &str = "existing_node";
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let existing_path =
        push_unbuilt_node_onto_stack(&node_stack, nodes_dir.path(), EXISTING_NODE, NODE_TAG);

    // One deployment resolves through the cache, the other is missing from it.
    let launcher_json5 = r#"
    {
      peppy_schema: "launcher/v1",
      deployments: [
        { source: { name: "existing_node:v1" }, instances: [ { instance_id: "ok" } ] },
        { source: { name: "missing_node:v1" }, instances: [ { instance_id: "nope" } ] }
      ]
    }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry(EXISTING_NODE, NODE_TAG, &existing_path)
        .write(&started_core_node.peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    result.assert_failed(
        PeppyFailureKind::Rejected,
        "Dependencies missing from nodes cache",
    );
    result.assert_failed(PeppyFailureKind::Rejected, "missing_node:v1");
    assert!(
        node_stack.contains(EXISTING_NODE, NODE_TAG),
        "stack should not apply a partial plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    const UVC_NODE_NAME: &str = "uvc_camera";
    const ROBOT_NODE_NAME: &str = "robot_brain";
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    // Intentionally do NOT emit camera_stream so dependency validation fails.
    let uvc_path = write_node_config(nodes_dir.path(), UVC_NODE_NAME, NODE_TAG, &["sleep", "60"]);
    let brain_path = write_node_config_with_options(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        NodeConfigOptions {
            run_cmd: &["sleep", "60"],
            depends_on: DEPENDS_ON_FRONT_CAMERA,
            interfaces: CONSUMES_FRONT_CAMERA_STREAM,
            ..Default::default()
        },
    );

    // Seed stack with a node so we can assert rollback behavior.
    push_unbuilt_node_onto_stack(&node_stack, nodes_dir.path(), "existing_node", NODE_TAG);

    let launcher_json5 = r#"
    {
      peppy_schema: "launcher/v1",
      deployments: [
        { source: { name: "uvc_camera:v1" }, instances: [ { instance_id: "camera_front" } ] },
        { source: { name: "robot_brain:v1" }, instances: [ { instance_id: "main_robot_brain" } ] }
      ]
    }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry(UVC_NODE_NAME, NODE_TAG, &uvc_path)
        .fs_entry(ROBOT_NODE_NAME, NODE_TAG, &brain_path)
        .write(&started_core_node.peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    result.assert_failed(
        PeppyFailureKind::Rejected,
        "expects Topic `camera_stream` from `uvc_camera`:v1, but it is not exposed",
    );
    assert!(
        node_stack.contains("existing_node", NODE_TAG),
        "stack should not be mutated on dependency failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let nodes_dir = tempdir().expect("failed to create nodes dir");

    let node_a_path = write_node_config(nodes_dir.path(), "node_a", NODE_TAG, &["sleep", "60"]);
    let node_b_path = write_node_config(nodes_dir.path(), "node_b", NODE_TAG, &["sleep", "60"]);

    let mut responders = answer_readiness(&started_core_node, "node_a", "a1").await;
    responders.extend(answer_readiness(&started_core_node, "node_b", "b1").await);

    let launch_a = r#"
    { peppy_schema: "launcher/v1", deployments: [ { source: { name: "node_a:v1" }, instances: [ { instance_id: "a1" } ] } ] }
    "#;
    let launch_file_path_a = nodes_dir.path().join("launch_a.json5");
    fs::write(&launch_file_path_a, launch_a).expect("failed to write launch file");
    let launch_b = r#"
    { peppy_schema: "launcher/v1", deployments: [ { source: { name: "node_b:v1" }, instances: [ { instance_id: "b1" } ] } ] }
    "#;
    let launch_file_path_b = nodes_dir.path().join("launch_b.json5");
    fs::write(&launch_file_path_b, launch_b).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry("node_a", NODE_TAG, &node_a_path)
        .fs_entry("node_b", NODE_TAG, &node_b_path)
        .write(&started_core_node.peppy_dirs);

    let result_a = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path_a,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("first launch should complete");
    result_a.assert_completed();
    assert!(node_stack.contains("node_a", NODE_TAG));

    // Capture the launch-started process so we can prove the relaunch tears it
    // down instead of orphaning it.
    let pid_a = node_stack
        .find("node_a", NODE_TAG)
        .expect("node_a should be in the stack after the first launch")
        .read()
        .instances()
        .first()
        .expect("node_a should have a running instance")
        .pid()
        .expect("node_a instance should have a pid");
    assert!(
        is_process_running(pid_a),
        "node_a process {pid_a} should be running after the first launch"
    );

    let result_b = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path_b,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("second launch should complete");
    result_b.assert_completed();

    assert!(
        !node_stack.contains("node_a", NODE_TAG),
        "second request should replace existing stack (remove node_a)"
    );
    assert!(
        node_stack.contains("node_b", NODE_TAG),
        "second request should replace existing stack (add node_b)"
    );

    // The relaunch must terminate the previous stack's process: an orphan
    // would outlive the relaunch, detached from the daemon and invisible to
    // `stack list`.
    wait_until_process_exits(
        pid_a,
        RESULT_TIMEOUT,
        &format!("node_a process {pid_a} should be terminated by the relaunch, not orphaned"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_one_node_never_becomes_healthy_and_clears_stack()
 {
    const NODE_TAG: &str = "v1";

    // A short default setup budget, so the test doesn't take too long.
    let started_core_node =
        start_core_node_with_default_setup_timeout(common::setup_budget(2)).await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");

    // Seed a real running previous stack so we can prove the launch tears it
    // down (not orphans it) when the new launch fails.
    let (_seed_src, _seed_guard, existing_pid) = seed_running_node(
        &started_core_node,
        "existing_node",
        NODE_TAG,
        "existing_inst",
    )
    .await;

    // Node to be launched: starts a real process but never reports healthy.
    let node_b_path = write_node_config(nodes_dir.path(), "node_b", NODE_TAG, &["sleep", "60"]);

    // An instance that answered ready is stopped through its shutdown
    // request; this one ends at once instead of at its force-kill deadline.
    let (_shutdown_b, _) = common::install_shutdown_listener_for_instance(
        &started_core_node,
        "node_b",
        NODE_TAG,
        "b1",
    )
    .await;

    let launch_b = r#"
    { peppy_schema: "launcher/v1", deployments: [ { source: { name: "node_b:v1" }, instances: [ { instance_id: "b1" } ] } ] }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launch_b).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry("node_b", NODE_TAG, &node_b_path)
        .write(&started_core_node.peppy_dirs);

    // Drive the launch concurrently so we can capture node_b's started pid
    // before the setup failure tears the stack down, and check that the
    // failure path terminates that partially-started instance. The start
    // waits for the ready signal of node_b, which the test answers only
    // once it holds the pid: the setup budget runs from the ready signal on.
    let caller = started_core_node.caller_handle.clone();
    let core_name = started_core_node.core_node_name.clone();
    let launch_task = tokio::spawn(async move {
        send_node_launch_and_wait(
            &caller,
            &core_name,
            &launch_file_path,
            GOAL_TIMEOUT,
            RESULT_TIMEOUT,
        )
        .await
    });

    let node_b_pid = poll_until(RESULT_TIMEOUT, "node_b should start a real process", || {
        node_stack
            .find("node_b", NODE_TAG)
            .and_then(|h| h.read().instances().first().and_then(|i| i.pid()))
    })
    .await;
    assert!(
        is_process_running(node_b_pid),
        "node_b process {node_b_pid} should be running while its start waits for the ready signal"
    );
    // Only the ready probe is answered: with no health responder, the setup
    // runs out its budget.
    let _ready_b = answer_ready_probe(&started_core_node, "node_b", "b1").await;

    let result = launch_task
        .await
        .expect("launch task panicked")
        .expect("launch should complete");

    // The node never becomes healthy, so its start fails, naming the
    // default setup budget and the key.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "the setup did not end within 2 s, the default setup budget",
    );
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "`execution.setup_timeout_secs`",
    );

    // A failed launch leaves a clean empty stack.
    assert_eq!(
        node_stack.len(),
        1,
        "only root should remain after a failed launch"
    );
    assert!(
        !node_stack.contains("existing_node", NODE_TAG),
        "previous stack should be torn down, not restored, on failure"
    );
    assert!(
        !node_stack.contains("node_b", NODE_TAG),
        "node_b should not be present after failed launch"
    );

    // Neither the previous stack's process nor the partially-started new
    // instance may be orphaned by the failure path.
    wait_until_process_exits(
        existing_pid,
        RESULT_TIMEOUT,
        &format!("previous stack process {existing_pid} should be terminated, not orphaned"),
    )
    .await;
    wait_until_process_exits(
        node_b_pid,
        RESULT_TIMEOUT,
        &format!("partial new instance {node_b_pid} should be terminated, not orphaned"),
    )
    .await;
}

/// A launch accepts a cancel from its client. The start in progress stops,
/// the launch clears the stack as a launch whose start fails clears it, and
/// the goal ends cancelled with the reason of the cancel.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_cancelled_launch_clears_the_stack_and_ends_cancelled() {
    const NODE_TAG: &str = "v1";

    // A setup budget that the test does not reach: only the cancel ends the
    // start.
    let started = start_core_node_with_default_setup_timeout(config::node::SetupTimeout::MAX).await;
    let node_stack = started.node_stack.clone();
    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let node_path = write_node_config(
        nodes_dir.path(),
        "waiting_node",
        NODE_TAG,
        &["sleep", "300"],
    );
    // The instance answers ready and never answers its health check, so its
    // setup waits. Its shutdown request ends it at once.
    let _ready = answer_ready_probe(&started, "waiting_node", "waiting_inst").await;
    let (_shutdown, _) = common::install_shutdown_listener_for_instance(
        &started,
        "waiting_node",
        NODE_TAG,
        "waiting_inst",
    )
    .await;
    let launcher = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(
        &launcher,
        r#"{ peppy_schema: "launcher/v1", deployments: [ { source: { name: "waiting_node:v1" }, instances: [ { instance_id: "waiting_inst" } ] } ] }"#,
    )
    .expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry("waiting_node", NODE_TAG, &node_path)
        .write(&started.peppy_dirs);
    let goal = LaunchGoal::new(
        LauncherOrigin::Fs(launcher),
        TEST_LAUNCH_ID,
        unreached_budgets(),
    );

    let handle = send_accepted_stack_goal(&started, &goal).await;
    let pid = poll_until(
        RESULT_TIMEOUT,
        "the start of waiting_inst records its process",
        || {
            node_stack
                .find("waiting_node", NODE_TAG)
                .and_then(|h| h.read().instances().first().and_then(|i| i.pid()))
        },
    )
    .await;
    let end = cancel_and_follow_to_end(&started, handle, async {}).await;

    assert_eq!(end.cancel_reason(), "cancelled by its caller");
    assert_eq!(
        node_stack.len(),
        1,
        "the cancelled launch clears the stack as a failed launch does"
    );
    wait_until_process_exits(
        pid,
        RESULT_TIMEOUT,
        &format!("the cancel stops the start of process {pid}"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_build_cmd_fails_and_clears_stack() {
    const NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");

    // Seed a real running previous stack so we can prove the launch tears it
    // down (not orphans it) when the new launch fails.
    let (_seed_src, _seed_guard, existing_pid) = seed_running_node(
        &started_core_node,
        "existing_node",
        NODE_TAG,
        "existing_inst",
    )
    .await;

    // Node with a failing build_cmd.
    let failing_node_path = write_node_config_with_options(
        nodes_dir.path(),
        "failing_node",
        NODE_TAG,
        NodeConfigOptions {
            build_cmd: &["false"], // This command always fails with exit code 1
            run_cmd: &["sleep", "60"],
            ..Default::default()
        },
    );

    let launcher_json5 = r#"
    { peppy_schema: "launcher/v1", deployments: [ { source: { name: "failing_node:v1" }, instances: [ { instance_id: "f1" } ] } ] }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry("failing_node", NODE_TAG, &failing_node_path)
        .write(&started_core_node.peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    // The build_cmd fails, and the failure names the node.
    result.assert_failed(PeppyFailureKind::AttemptFailed, "failing_node:v1");

    // A failed launch leaves a clean empty stack. The previous
    // stack is torn down at the clear step, not rolled back.
    assert_eq!(
        node_stack.len(),
        1,
        "only root should remain after a failed launch"
    );
    assert!(
        !node_stack.contains("existing_node", NODE_TAG),
        "previous stack should be torn down, not restored, on failure"
    );
    assert!(
        !node_stack.contains("failing_node", NODE_TAG),
        "failing_node should not be present after failed launch"
    );

    // The previous stack's process must be terminated, not orphaned.
    wait_until_process_exits(
        existing_pid,
        RESULT_TIMEOUT,
        &format!("previous stack process {existing_pid} should be terminated, not orphaned"),
    )
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_run_cmd_exits_with_error() {
    const NODE_NAME: &str = "failing_start_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "fs1";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");

    // Seed a real running previous stack so we can prove the launch tears it
    // down (not orphans it) when the new launch fails.
    let (_seed_src, _seed_guard, existing_pid) = seed_running_node(
        &started_core_node,
        "existing_node",
        NODE_TAG,
        "existing_inst",
    )
    .await;

    // Node whose run_cmd exits immediately with a non-zero status.
    let failing_node_path = write_node_config_with_options(
        nodes_dir.path(),
        NODE_NAME,
        NODE_TAG,
        NodeConfigOptions {
            build_cmd: &["true"],
            run_cmd: &["false"], // exits immediately with status 1
            ..Default::default()
        },
    );

    let launcher_json5 = format!(
        r#"{{ peppy_schema: "launcher/v1", deployments: [ {{ source: {{ name: "{NODE_NAME}:{NODE_TAG}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}" }} ] }} ] }}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry(NODE_NAME, NODE_TAG, &failing_node_path)
        .write(&started_core_node.peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");

    // The run_cmd exits with an error, and the failure names the node and
    // the instance.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        &format!("{NODE_NAME}:{NODE_TAG}"),
    );
    result.assert_failed(PeppyFailureKind::AttemptFailed, INSTANCE_ID);

    // A failed launch leaves a clean empty stack. The previous
    // stack is torn down at the clear step, not rolled back.
    assert_eq!(
        node_stack.len(),
        1,
        "only root should remain after a failed launch"
    );
    assert!(
        !node_stack.contains("existing_node", NODE_TAG),
        "previous stack should be torn down, not restored, on failure"
    );
    assert!(
        !node_stack.contains(NODE_NAME, NODE_TAG),
        "{NODE_NAME} should not be present after failed launch"
    );

    // The previous stack's process must be terminated, not orphaned.
    wait_until_process_exits(
        existing_pid,
        RESULT_TIMEOUT,
        &format!("previous stack process {existing_pid} should be terminated, not orphaned"),
    )
    .await;

    // The node was successfully added before the start failed, so we expect one add log entry.
    assert_eq!(
        result.node_logs.node_add_logs.len(),
        1,
        "should have 1 add log entry for the node that was added before start failed"
    );
    assert!(
        !result.node_logs.node_add_logs[0].failed,
        "add log entry should not be marked failed (add succeeded)"
    );
    assert_eq!(
        result.node_logs.node_add_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );

    // The node was successfully built before the start failed, so we expect one build log entry.
    assert_eq!(
        result.node_logs.node_build_logs.len(),
        1,
        "should have 1 build log entry for the node that was built before start failed"
    );
    assert!(
        !result.node_logs.node_build_logs[0].failed,
        "build log entry should not be marked failed (build succeeded)"
    );
    assert_eq!(
        result.node_logs.node_build_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );

    // The failed start should produce one start log entry marked as failed.
    assert_eq!(
        result.node_logs.node_run_logs.len(),
        1,
        "should have 1 start log entry for the failed instance"
    );
    assert!(
        result.node_logs.node_run_logs[0].failed,
        "start log entry should be marked failed"
    );
    assert_eq!(result.node_logs.node_run_logs[0].instance_id, INSTANCE_ID);
    assert_eq!(
        result.node_logs.node_run_logs[0].node_label,
        format!("{}:{}", NODE_NAME, NODE_TAG)
    );
    assert!(
        !result.node_logs.node_run_logs[0]
            .log_path
            .as_os_str()
            .is_empty(),
        "start log path should be non-empty"
    );
    assert_eq!(
        result.node_logs.node_run_logs[0].core_node, started_core_node.core_node_name,
        "a failed start's log entry still names the machine holding the file"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_launch_uses_env_overrides_for_path() {
    // Emulates a real case scenario where the caller environment differs from the already-running
    // daemon environment. In practice, users often "install a tool then source it" (e.g.
    // `. "$HOME/.cargo/env"`), but that only affects their shell, not the daemon. We model this by
    // passing a PATH override in the goal on the second attempt.
    const NODE_NAME: &str = "node_b";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "b1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let node_path = write_node_config(
        nodes_dir.path(),
        NODE_NAME,
        NODE_TAG,
        &["printout", "60"], // run_cmd that sleeps via printout
    );
    let launch_json5 = format!(
        r#"{{ peppy_schema: "launcher/v1", deployments: [ {{ source: {{ name: "{NODE_NAME}:{NODE_TAG}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}" }} ] }} ] }}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launch_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry(NODE_NAME, NODE_TAG, &node_path)
        .write(&started_core_node.peppy_dirs);

    // `printout` does not exist in the system when this is run
    let launch_result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch request should complete");

    // `printout` does not exist, so the start of the node fails.
    launch_result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to spawn `printout 60`",
    );

    // Create a temp bin directory with a `printout` script that sleeps
    let bin_dir = tempfile::tempdir().expect("failed to create temp bin dir");
    write_executable_script(
        bin_dir.path(),
        "printout",
        "#!/bin/sh
sleep \"${1:-60}\"
",
    );

    // Set up ready/health responders for the instance
    let _responders = answer_readiness(&started_core_node, NODE_NAME, INSTANCE_ID).await;

    // Pass the bin directory in PATH via env overrides to simulate the caller having an updated
    // PATH without restarting the daemon.
    let env_vars = vec![path_env_with_dir_first(bin_dir.path())];

    let launch_result = send_node_launch_and_wait_with_env(
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
    launch_result.assert_completed();
}

/// A launcher instance's `env_vars` reach the process the daemon spawns, and a
/// key it declares wins over the caller environment the launch forwards. The
/// node writes what it actually received to a file, so the assertion is on the
/// value in the spawned process rather than on anything the daemon reports.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_applies_per_instance_env_vars() {
    const NODE_NAME: &str = "env_probe_node";
    const NODE_TAG: &str = "v1";
    const INSTANCE_ID: &str = "probe_1";
    const INSTANCE_DEVICE: &str = "/dev/ttyUSB0";
    const CALLER_DEVICE: &str = "/dev/from_caller";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let probe_path = nodes_dir.path().join("received_env.txt");
    // Records the value the process was started with, then stays alive so the
    // instance can reach Running like any other node.
    let probe_script = format!(
        "printf '%s' \"$ESP32_DEVICE\" > {}; exec sleep 60",
        probe_path.display()
    );
    let node_path = write_node_config(
        nodes_dir.path(),
        NODE_NAME,
        NODE_TAG,
        &["sh", "-c", probe_script.as_str()],
    );

    let launcher_json5 = format!(
        r#"{{ peppy_schema: "launcher/v1", deployments: [ {{ source: {{ name: "{NODE_NAME}:{NODE_TAG}" }}, instances: [ {{ instance_id: "{INSTANCE_ID}", env_vars: {{ ESP32_DEVICE: "{INSTANCE_DEVICE}" }} }} ] }} ] }}"#
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, &launcher_json5).expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry(NODE_NAME, NODE_TAG, &node_path)
        .write(&started_core_node.peppy_dirs);

    let _responders = answer_readiness(&started_core_node, NODE_NAME, INSTANCE_ID).await;

    // The caller environment carries the same key with a different value, so a
    // matching probe can only come from the instance's own declaration.
    let result = send_node_launch_and_wait_with_env(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        vec![("ESP32_DEVICE".to_string(), CALLER_DEVICE.to_string())],
    )
    .await
    .expect("launch request should complete");

    result.assert_completed();

    let received = poll_until(
        RESULT_TIMEOUT,
        "the launched node should have recorded the env var it was started with",
        || fs::read_to_string(&probe_path).ok(),
    )
    .await;
    assert_eq!(
        received, INSTANCE_DEVICE,
        "the instance's `env_vars` value must reach the spawned process and \
         override the forwarded caller environment"
    );
}

/// `LauncherOrigin::Repository` resolves the launcher name through `launchers.json5`
/// (the launcher repo cache) and uses the on-disk path it points at. The deployment
/// resolves through an fs entry in the nodes cache.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_resolves_launcher_from_repository_cache() {
    const ROBOT_NODE_NAME: &str = "robot_brain_repo";
    const NODE_TAG: &str = "v1";
    const LAUNCHER_NAME: &str = "robot_brain_demo";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();
    let peppy_dirs = started_core_node.peppy_dirs.clone();

    let nodes_dir = tempdir().expect("failed to create temp nodes directory");
    let brain_path = write_node_config(
        nodes_dir.path(),
        ROBOT_NODE_NAME,
        NODE_TAG,
        &["sleep", "60"],
    );

    let _responders =
        answer_readiness(&started_core_node, ROBOT_NODE_NAME, "main_robot_brain").await;

    // Launcher file references the node by its `name:tag` in the nodes cache.
    let launcher_json5 = format!(
        r#"{{
  peppy_schema: "launcher/v1",
  deployments: [
    {{
      source: {{ name: "{ROBOT_NODE_NAME}:{NODE_TAG}" }},
      instances: [
        {{ instance_id: "main_robot_brain" }}
      ]
    }}
  ]
}}"#
    );
    let launcher_file_path = nodes_dir.path().join(format!("{LAUNCHER_NAME}.json5"));
    fs::write(&launcher_file_path, &launcher_json5).expect("failed to write launcher file");

    TestPackagesCache::new()
        .fs_entry(ROBOT_NODE_NAME, NODE_TAG, &brain_path)
        .launcher_fs_entry(LAUNCHER_NAME, &launcher_file_path)
        .write(&peppy_dirs);

    let result = send_launch_origin_and_wait(
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

    result.assert_completed();
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

    let result = send_launch_origin_and_wait(
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

    result.assert_failed(
        PeppyFailureKind::Rejected,
        "launcher `definitely_missing_launcher` not found",
    );
    // The failure names the launcher cache path.
    result.assert_failed(PeppyFailureKind::Rejected, "launchers.json5");
}

/// The launcher schema knows exactly one source shape, `{ name, tag }`: a
/// path-shaped `local:` source fails at parse time with an error naming the
/// offending key, and the stack is untouched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_rejects_a_path_shaped_deployment_source() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let launcher_json5 = r#"
    {
      peppy_schema: "launcher/v1",
      deployments: [
        { source: { local: "./uvc_camera" }, instances: [ { instance_id: "camera_front" } ] }
      ]
    }
    "#;
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(&launch_file_path, launcher_json5).expect("failed to write launch file");

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should return a result");

    // The failure names the offending key.
    result.assert_failed(
        PeppyFailureKind::Rejected,
        "deployment source `local` was removed",
    );
    assert!(
        !node_stack.contains("uvc_camera", "v1"),
        "the rejected deployment must not reach the stack"
    );
}

// =============================================================================
// The waldo engine's binding generation, through a real launch, on the
// runtime's default thread stacks.
// =============================================================================

/// Set in the environment of the child process that runs
/// [`waldo_generation_launch_child`]. The parent never carries it, so a child
/// cannot spawn a child of its own.
const WALDO_CODEGEN_CHILD_ENV: &str = "PEPPY_TEST_WALDO_CODEGEN_CHILD";
/// Printed by the fixture's `build_cmd` once every generated-output check has
/// passed, and only then.
const WALDO_CODEGEN_CHECKPOINT: &str = "WALDO_CODEGEN_CHECKPOINT";

/// Drives the waldo generation workload through a launch in a child process,
/// so a fatal stack overflow in the daemon's launch task fails this one test
/// instead of killing the test binary. The child builds a multithreaded Tokio
/// runtime with no stack-size override and without any inherited
/// `RUST_MIN_STACK`, which is what the daemon runs on.
#[test]
fn waldo_generation_launch_survives_a_default_worker_stack() {
    if std::env::var_os(WALDO_CODEGEN_CHILD_ENV).is_some() {
        return;
    }
    let test_binary = std::env::current_exe().expect("the running test binary has a path");
    let output = std::process::Command::new(test_binary)
        .args([
            "--exact",
            "waldo_generation_launch_child",
            "--ignored",
            "--nocapture",
        ])
        .env(WALDO_CODEGEN_CHILD_ENV, "1")
        .env_remove("RUST_MIN_STACK")
        .stdin(std::process::Stdio::null())
        .output()
        .expect("spawn the child test process");
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "the child launch process exited with {:?}\n--- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}",
        output.status
    );
    assert!(
        stdout.contains("test waldo_generation_launch_child ... ok"),
        "the child ran its launch test to completion\n--- child stdout ---\n{stdout}\n--- child stderr ---\n{stderr}"
    );
}

/// The child half of [`waldo_generation_launch_survives_a_default_worker_stack`].
/// Ignored so the harness never runs it in the parent process; the parent
/// runs it by name in a process of its own.
#[test]
#[ignore = "child half of waldo_generation_launch_survives_a_default_worker_stack, run by that test in its own process"]
fn waldo_generation_launch_child() {
    assert!(
        std::env::var_os(WALDO_CODEGEN_CHILD_ENV).is_some(),
        "the child test runs through its parent test"
    );
    assert!(
        std::env::var_os("RUST_MIN_STACK").is_none(),
        "the child runs on the runtime's default thread stacks"
    );
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a default multithreaded runtime");
    runtime.block_on(waldo_generation_launch());
}

/// Launches the `waldo_codegen` fixture (see its PROVENANCE.md): the waldo
/// engine's full declarative surface, resolved from filesystem contract and
/// pairing caches, added and generated through the pinned-add path, then
/// built by a `build_cmd` that checks the generated bindings, prints the
/// checkpoint, and fails on purpose. The launch must come back as an
/// ordinary build failure, with the checkpoint on record, the stack cleared,
/// the staging directory gone, and the core node still answering.
async fn waldo_generation_launch() {
    let started_core_node = start_core_node_with_mock_messenger().await;
    let peppy_dirs = started_core_node.peppy_dirs.clone();
    let node_stack = started_core_node.node_stack.clone();

    let assets =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/launch_assets/waldo_codegen");
    let mut cache = TestPackagesCache::new().fs_entry("waldo", "v1", assets.join("node"));
    for contract in [
        "scene_control",
        "object_state",
        "contact_state",
        "sensor_readout",
    ] {
        let path = assets.join("contracts").join(format!("{contract}.json5"));
        let body = fs::read_to_string(&path).expect("read the fixture's contract document");
        cache = cache.contract_fs_entry(contract, "v1", &path, &body);
    }
    for pairing in [
        "joint_link",
        "gripper_link",
        "sim_rgb_camera_link",
        "sim_rgbd_camera_link",
    ] {
        let path = assets.join("pairings").join(format!("{pairing}.json5"));
        let body = fs::read_to_string(&path).expect("read the fixture's pairing document");
        cache = cache.pairing_fs_entry(pairing, "v1", &path, &body);
    }
    cache.write(&peppy_dirs);

    let mut feedback_lines: Vec<String> = Vec::new();
    let result = send_launch_origin_and_wait_observing(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        LauncherOrigin::Fs(assets.join("peppy_launcher.json5")),
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
        vec![],
        &mut |output| feedback_lines.push(format!("[{:?}] {}", output.step, output.line)),
    )
    .await
    .expect("the launch ends");

    // The fixture's build_cmd fails on purpose, so the launch fails at the
    // build, as an ordinary build failure.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to build node waldo:v1",
    );
    result.assert_failed(PeppyFailureKind::AttemptFailed, "build_cmd");

    let feedback = feedback_lines.join("\n");
    assert!(
        feedback.contains(WALDO_CODEGEN_CHECKPOINT),
        "the build_cmd reached its checkpoint, so every generated-output check passed; \
         feedback:\n{feedback}"
    );
    let launch_log = fs::read_to_string(&result.log_path).expect("read the launch log");
    assert!(
        launch_log.contains(WALDO_CODEGEN_CHECKPOINT),
        "the checkpoint is on record in the launch log at {}:\n{launch_log}",
        result.log_path.display()
    );

    assert_eq!(
        node_stack.len(),
        1,
        "a failed launch leaves only the root in the stack"
    );
    assert!(
        !node_stack.contains("waldo", "v1"),
        "the failed node is cleared from the stack"
    );
    let staging_leftovers: Vec<PathBuf> = fs::read_dir(peppy_dirs.tmp_dir())
        .map(|entries| entries.flatten().map(|entry| entry.path()).collect())
        .unwrap_or_default();
    assert!(
        staging_leftovers.is_empty(),
        "a cleared node leaves no staging directory behind: {staging_leftovers:?}"
    );

    // The daemon is still answering after the failed launch.
    common::assert_clock_round_trip(&started_core_node).await;
}

/// Launches one node whose `build_cmd` and `run_cmd` are the given shell
/// scripts, on a core node that exports its logs. Returns the result of the
/// launch, every record exported by the time it arrived, and the core node,
/// whose data root holds the log files.
async fn launch_scripted_node(
    build_script: &str,
    run_script: &str,
) -> (StackGoalEnd, Vec<log_export::LogRecord>, StartedCoreNode) {
    launch_scripted_node_above(None, build_script, run_script).await
}

/// [`launch_scripted_node`] on a core node that exports the lines at
/// `min_severity` and above.
async fn launch_scripted_node_above(
    min_severity: Option<daemon_config::peppy_config::OtlpMinSeverity>,
    build_script: &str,
    run_script: &str,
) -> (StackGoalEnd, Vec<log_export::LogRecord>, StartedCoreNode) {
    let (started_core_node, mut exported) =
        common::start_core_node_with_log_export(min_severity).await;
    let nodes_dir = tempdir().expect("failed to create nodes dir");
    let node_path = write_node_config_with_options(
        nodes_dir.path(),
        "scripted_node",
        "v1",
        NodeConfigOptions {
            build_cmd: &["sh", "-c", build_script],
            run_cmd: &["sh", "-c", run_script],
            ..Default::default()
        },
    );
    let launch_file_path = nodes_dir.path().join("peppy_launcher.json5");
    fs::write(
        &launch_file_path,
        r#"{ peppy_schema: "launcher/v1", deployments: [ { source: { name: "scripted_node:v1" }, instances: [ { instance_id: "s1" } ] } ] }"#,
    )
    .expect("failed to write launch file");
    TestPackagesCache::new()
        .fs_entry("scripted_node", "v1", &node_path)
        .write(&started_core_node.peppy_dirs);

    let result = send_node_launch_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &launch_file_path,
        GOAL_TIMEOUT,
        RESULT_TIMEOUT,
    )
    .await
    .expect("launch should complete");
    let records = log_export::test_support::drain_records(&mut exported);
    // The launch log gets the output of a node from a task of its own, which
    // may still be writing when the result arrives.
    let output: Vec<&str> = records
        .iter()
        .filter(|record| matches!(record.origin, log_export::LineOrigin::Captured(_)))
        .map(|record| record.body.as_str())
        .collect();
    common::poll_until(
        RESULT_TIMEOUT,
        "the launch log should hold the output it relays",
        || {
            let launch_log = fs::read_to_string(&result.log_path).ok()?;
            output
                .iter()
                .all(|line| launch_log.contains(line))
                .then_some(())
        },
    )
    .await;
    (result, records, started_core_node)
}

/// A line a scripted node prints. The script that prints it does not hold it,
/// so an error that quotes the script does not hold it either.
const SCRIPT_OUTPUT: &str = "the-node-printed-this-line";
const PRINT_OUTPUT_AND_FAIL: &str = "printf 'the-node-%s\\n' printed-this-line; exit 1";

/// The launch log holds `SCRIPT_OUTPUT`, which it relayed, and exports none
/// of it; the log of kind `owner` exports it once, for the node and the launch.
fn assert_output_is_exported_by_its_own_log(
    result: &StackGoalEnd,
    records: &[log_export::LogRecord],
    owner: log_export::LogKind,
) {
    let launch_log = fs::read_to_string(&result.log_path).expect("the launch log should exist");
    assert!(
        launch_log.contains(SCRIPT_OUTPUT),
        "the launch log should hold the relayed output: {launch_log}"
    );
    let exporters: Vec<&log_export::LogRecord> = records
        .iter()
        .filter(|record| record.body.contains(SCRIPT_OUTPUT))
        .collect();
    assert_eq!(
        exporters.len(),
        1,
        "the output should be exported once: {exporters:?}"
    );
    let identity = &exporters[0].identity;
    assert_eq!(identity.kind, owner);
    let node = identity.node.as_ref().expect("the log names its node");
    assert_eq!(
        (node.name.as_str(), node.tag.as_str()),
        ("scripted_node", "v1")
    );
    assert_eq!(identity.launch_id.as_deref(), Some(TEST_LAUNCH_ID));
}

/// A launch whose build fails. The launch log exports its own narration and
/// the error that ended the launch. The build output it relays is in its file
/// and is exported by the build log alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_exports_its_own_lines_and_relays_build_output_to_its_file() {
    use daemon_config::peppy_config::Severity;
    use log_export::{LogKind, LogRecord};

    let (result, records, _core_node) =
        launch_scripted_node(PRINT_OUTPUT_AND_FAIL, "sleep 60").await;
    // The build fails, so the launch fails.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to build node scripted_node:v1",
    );
    assert_output_is_exported_by_its_own_log(&result, &records, LogKind::Build);

    let launch_records: Vec<&LogRecord> = records
        .iter()
        .filter(|record| {
            record.identity.kind
                == LogKind::Launch {
                    action: log_export::StackAction::Launch,
                }
        })
        .collect();
    assert!(
        launch_records
            .iter()
            .all(
                |record| record.identity.launch_id.as_deref() == Some(TEST_LAUNCH_ID)
                    && record.identity.file_path == result.log_path
            ),
        "every launch record names its launch and its file: {launch_records:?}"
    );
    assert!(
        launch_records
            .iter()
            .any(|record| record.severity == Some(Severity::Info)),
        "the narration is exported as info: {launch_records:?}"
    );
    assert!(
        launch_records
            .iter()
            .any(|record| record.severity == Some(Severity::Error)
                && record.body.contains("scripted_node:v1")),
        "the error that ended the launch is exported: {launch_records:?}"
    );
}

/// A launch whose node prints a line and exits while it starts. The output
/// the launch log relays is exported by the run log alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_relays_run_output_to_its_file() {
    let (result, records, _core_node) = launch_scripted_node("true", PRINT_OUTPUT_AND_FAIL).await;
    // The node exits, so the launch fails.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to start node scripted_node:v1 instance s1",
    );
    assert_output_is_exported_by_its_own_log(&result, &records, log_export::LogKind::Run);
    let run_record = records
        .iter()
        .find(|record| record.body.contains(SCRIPT_OUTPUT))
        .expect("checked above");
    assert_eq!(run_record.identity.instance_id.as_deref(), Some("s1"));
}

/// The entries of the log at `path`.
fn log_entries(path: &std::path::Path) -> Vec<String> {
    let content = fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("{} should be readable: {error}", path.display()));
    node_stack::action_log::test_support::log_entries(&content)
}

/// One launch, four logs. The add, build and run logs each export one record
/// per entry. The launch log exports its own entries, and the entries it
/// relays from the three node logs are exported by those logs alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_log_of_a_launch_exports_its_own_entries_once() {
    use log_export::LogKind;
    use std::collections::BTreeMap;

    let (result, records, _core_node) = launch_scripted_node(
        "printf 'build-%s\\n' line-one; printf 'warning: build-%s\\n' line-two",
        "printf 'run-%s\\n' line-one; printf 'ERROR run-%s\\n' line-two; exit 3",
    )
    .await;
    // The node exits, so the launch fails.
    result.assert_failed(
        PeppyFailureKind::AttemptFailed,
        "failed to start node scripted_node:v1 instance s1",
    );

    let mut bodies_by_file: BTreeMap<&std::path::Path, Vec<&str>> = BTreeMap::new();
    for record in &records {
        bodies_by_file
            .entry(record.identity.file_path.as_path())
            .or_default()
            .push(record.body.as_str());
    }
    let kinds: Vec<LogKind> =
        records
            .iter()
            .map(|record| record.identity.kind)
            .fold(Vec::new(), |mut kinds, kind| {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                kinds
            });
    for kind in [
        LogKind::Launch {
            action: log_export::StackAction::Launch,
        },
        LogKind::Add,
        LogKind::Build,
        LogKind::Run,
    ] {
        assert!(
            kinds.contains(&kind),
            "a {kind:?} log is exported: {kinds:?}"
        );
    }

    // Each node log: one record per entry, in the order of the file.
    let node_bodies: Vec<&str> = records
        .iter()
        .filter(|record| !matches!(record.identity.kind, LogKind::Launch { .. }))
        .map(|record| record.body.as_str())
        .collect();
    for (file, bodies) in &bodies_by_file {
        if *file == result.log_path.as_path() {
            continue;
        }
        assert_eq!(&log_entries(file), bodies, "{}", file.display());
    }
    let outputs = [
        "build-line-one",
        "warning: build-line-two",
        "run-line-one",
        "ERROR run-line-two",
    ];
    for output in outputs {
        assert_eq!(
            node_bodies.iter().filter(|body| **body == output).count(),
            1,
            "`{output}` is exported once, by its node log: {node_bodies:#?}"
        );
    }

    // The launch log: each entry is its own, exported by it, or one a node
    // log holds and exports. It labels each line of a build with its node.
    let entry_in_launch_log = |record: &log_export::LogRecord| match record.identity.kind {
        LogKind::Build => format!("[scripted_node:v1] {}", record.body),
        _ => record.body.clone(),
    };
    let relayed: Vec<String> = records
        .iter()
        .filter(|record| !matches!(record.identity.kind, LogKind::Launch { .. }))
        .map(entry_in_launch_log)
        .collect();
    let launch_bodies = &bodies_by_file[result.log_path.as_path()];
    let launch_entries = log_entries(&result.log_path);
    let own: Vec<&str> = launch_entries
        .iter()
        .filter(|entry| !relayed.contains(entry))
        .map(String::as_str)
        .collect();
    assert_eq!(
        &own, launch_bodies,
        "the launch log exports its own entries"
    );
    for record in records
        .iter()
        .filter(|record| outputs.contains(&record.body.as_str()))
    {
        let entry = entry_in_launch_log(record);
        assert!(
            launch_entries.contains(&entry),
            "the launch log holds `{entry}`, which it relays: {launch_entries:#?}"
        );
    }
    for body in launch_bodies {
        assert!(
            !node_bodies.contains(body),
            "`{body}` is exported by the launch log and by a node log"
        );
    }
}

/// With the minimum severity at info, the run log holds each line its node printed, and
/// exports the ones at info and above and the one that prints no level.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_line_below_the_minimum_severity_stays_in_the_run_log() {
    use daemon_config::peppy_config::{OtlpMinSeverity, Severity};
    use log_export::{LineOrigin, LogKind};

    let (_result, records, _core_node) = launch_scripted_node_above(
        Some(OtlpMinSeverity::Info),
        "true",
        "printf 'DEBUG %s\\n' tick; printf 'INFO %s\\n' ready; printf 'ERROR %s\\n' stopped; \
         printf 'plain %s\\n' line; exit 3",
    )
    .await;

    let run_records: Vec<&log_export::LogRecord> = records
        .iter()
        .filter(|record| record.identity.kind == LogKind::Run)
        .collect();
    let run_log = &run_records
        .first()
        .expect("the run log is exported")
        .identity
        .file_path;
    let printed = ["DEBUG tick", "INFO ready", "ERROR stopped", "plain line"];
    let entries = log_entries(run_log);
    for line in printed {
        assert!(entries.iter().any(|entry| entry == line), "{entries:#?}");
    }
    let exported: Vec<(&str, Option<Severity>)> = run_records
        .iter()
        .filter(|record| matches!(record.origin, LineOrigin::Captured(_)))
        .map(|record| (record.body.as_str(), record.severity))
        .collect();
    assert_eq!(
        exported,
        [
            ("INFO ready", Some(Severity::Info)),
            ("ERROR stopped", Some(Severity::Error)),
            ("plain line", None),
        ]
    );
}
