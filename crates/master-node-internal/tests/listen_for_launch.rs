mod common;

use common::CALLER_INSTANCE_ID;
use config::consts::{NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH};
use config::runtime::LauncherRuntimeConfig;
use master_node::encoding::LaunchGoal;
use std::fs;
use std::time::Duration;
use tempfile::tempdir;

use crate::common::start_master_node_with_real_messenger;

// In the example below
// 1. Create a local git repository in the test for `uvc_camera`
// 2. The `robot_brain:0.1.0` node is in a folder `robot_brain/peppy.json5` next to the `peppy_launcher.json5` launch configuration (where the content of `LAUNCHER_EXAMPLE1` is copied). It has `uvc_camera:0.1.0` as dependency
const LAUNCHER_EXAMPLE1: &str = r#"
{
  deployments: [
    {
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
          env_vars: {
            IS_MAIN_BRAIN: "true"
          },
          parameters: {}
        }
      ]
    },
  ]
}
"#;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_succeed() {
    const TARGET_NODE_NAME: &str = "example_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "example_instance";

    let started_master = start_master_node_with_real_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let nodes_dir = common::init_test_node_project(TARGET_NODE_NAME, TARGET_NODE_TAG, false);

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
    todo!("Finish, use LAUNCHER_EXAMPLE1")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack()
 {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_nodes_directory_must_be_a_directory() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_dependency_errors_are_rejected() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_launch_config_second_request_replaces_existing_stack() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_runs_generate_on_node_before_start() {
    todo!("Finish")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_launch_configuration_fails_when_one_node_never_becomes_healthy() {
    todo!("Finish")
}
