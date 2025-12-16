mod common;

use common::{CALLER_INSTANCE_ID, setup_test_master_node};
use master_node::encoding::LauncherRequest;
use std::collections::BTreeSet;
use std::path::Path;
use std::time::Duration;
use tempfile::TempDir;

fn write_minimal_peppy_nodes(working_dir: &Path, node_names: &[&str]) {
    for node_name in node_names {
        let dir = working_dir.join(node_name);
        std::fs::create_dir(&dir).expect("failed to create node directory");
        std::fs::write(
            dir.join("peppy.json5"),
            format!(
                r#"{{
              schema_version: 1,
              manifest: {{
                name: "{node_name}",
                tag: "0.1.0"
              }}
            }}
            "#
            ),
        )
        .expect("failed to write peppy.json5");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_request() {
    let (client, server) = setup_test_master_node().await;

    let working_dir = TempDir::new().expect("failed to create temp directory");

    // Create a working directory with local nodes (peppy.json5) under it.
    write_minimal_peppy_nodes(working_dir.path(), &["uvc_camera", "esp32_board"]);

    let launcher_config = r#"{
      deployments: [
        {
          name: "uvc_camera",
          tag: "0.1.0",
          instances: [
            { instance_id: "camera_front" },
            { instance_id: "camera_rear" }
          ]
        },
        {
          name: "web_video_stream",
          tag: "0.1.0",
          optional: true,
          instances: [
            { instance_id: "video_stream1" }
          ]
        },
        {
          name: "esp32_board",
          tag: "0.1.0",
          instances: [
            { instance_id: "esp32_1" }
          ]
        }
      ]
    }"#;

    let response = LauncherRequest::new(launcher_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(
        response.success,
        "launcher request should succeed, got error: {}",
        response.error_message
    );

    assert_eq!(server.node_stack.len(), 3);
    assert!(server.node_stack.contains("uvc_camera", "0.1.0"));
    assert!(server.node_stack.contains("esp32_board", "0.1.0"));
    assert!(
        !server.node_stack.contains("web_video_stream", "0.1.0"),
        "unresolvable optional deployment should not be inserted"
    );

    let uvc_entity = server
        .node_stack
        .find("uvc_camera", "0.1.0")
        .expect("uvc_camera should exist");
    let uvc_instances: BTreeSet<String> = uvc_entity
        .instances()
        .iter()
        .map(|instance| instance.instance_id().as_str().to_owned())
        .collect();
    assert_eq!(
        uvc_instances,
        BTreeSet::from(["camera_front".to_string(), "camera_rear".to_string()])
    );

    let esp_entity = server
        .node_stack
        .find("esp32_board", "0.1.0")
        .expect("esp32_board should exist");
    let esp_instances: BTreeSet<String> = esp_entity
        .instances()
        .iter()
        .map(|instance| instance.instance_id().as_str().to_owned())
        .collect();
    assert_eq!(esp_instances, BTreeSet::from(["esp32_1".to_string()]));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_invalid_json5_returns_error_and_does_not_mutate_stack() {
    let (client, _server) = setup_test_master_node().await;

    let working_dir = TempDir::new().expect("failed to create temp directory");

    let invalid_launcher_config = r#"{ deployments: [ }"#;
    let response = LauncherRequest::new(invalid_launcher_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!response.success);
    assert!(
        response
            .error_message
            .contains("invalid peppy_launcher_json5")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_nodes_directory_must_be_a_directory() {
    let (client, server) = setup_test_master_node().await;
    assert_eq!(server.node_stack.len(), 1);

    let working_dir = TempDir::new().expect("failed to create temp directory");
    let nodes_directory_file = working_dir.path().join("not_a_dir");
    std::fs::write(&nodes_directory_file, "x").expect("failed to write file");

    let launcher_config = r#"{}"#;
    let response = LauncherRequest::new(launcher_config, &nodes_directory_file)
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!response.success);
    assert!(
        response
            .error_message
            .contains("nodes_directory is not a directory")
    );
    assert_eq!(server.node_stack.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_missing_required_deployment_does_not_apply_partial_plan() {
    let (client, server) = setup_test_master_node().await;
    assert_eq!(server.node_stack.len(), 1);

    let working_dir = TempDir::new().expect("failed to create temp directory");

    // The node is missing from the disk
    write_minimal_peppy_nodes(working_dir.path(), &["uvc_camera"]);

    let launcher_config = r#"{
      deployments: [
        {
          name: "uvc_camera",
          tag: "0.1.0",
          instances: [{ instance_id: "camera_1" }]
        },
        {
          name: "missing_node",
          tag: "0.1.0",
          instances: [{ instance_id: "missing_1" }]
        }
      ]
    }"#;

    let response = LauncherRequest::new(launcher_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!response.success);
    assert!(
        response.error_message.contains("missing_node"),
        "expected error to mention the missing deployment, got: {}",
        response.error_message
    );

    assert_eq!(
        server.node_stack.len(),
        1,
        "node stack should not change when the plan is invalid"
    );
    assert!(
        !server.node_stack.contains("uvc_camera", "0.1.0"),
        "service should not apply a partial plan"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_dependency_errors_are_rejected() {
    let (client, server) = setup_test_master_node().await;
    assert_eq!(server.node_stack.len(), 1);

    let working_dir = TempDir::new().expect("failed to create temp directory");

    let alpha_dir = working_dir.path().join("alpha");
    std::fs::create_dir(&alpha_dir).expect("failed to create alpha node directory");
    std::fs::write(
        alpha_dir.join("peppy.json5"),
        r#"{
        schema_version: 1,
        manifest: {
          name: "alpha",
          tag: "0.1.0"
        },
        interfaces: {
          subscribes_to: {
            topics: [
              { 
                id: "beta_stream", 
                node: "beta", 
                name: "stream", 
                tag: "0.1.0" 
              }
            ]
          }
        }
      }
      "#,
    )
    .expect("failed to write alpha peppy.json5");

    let launcher_config = r#"{
      deployments: [
        {
          name: "alpha",
          tag: "0.1.0",
          instances: [
            { 
              instance_id: "alpha_1" 
            }
          ]
        }
      ]
    }"#;

    let response = LauncherRequest::new(launcher_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");

    assert!(!response.success);
    assert!(
        response.error_message.contains("depends on") && response.error_message.contains("beta"),
        "expected missing dependency error, got: {}",
        response.error_message
    );
    assert_eq!(server.node_stack.len(), 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_launch_config_second_request_replaces_existing_stack() {
    let (client, server) = setup_test_master_node().await;
    assert_eq!(server.node_stack.len(), 1);

    let working_dir = TempDir::new().expect("failed to create temp directory");
    write_minimal_peppy_nodes(
        working_dir.path(),
        &["uvc_camera", "esp32_board", "lidar_sensor"],
    );

    let first_config = r#"{
      deployments: [
        {
          name: "uvc_camera",
          tag: "0.1.0",
          instances: [{ instance_id: "camera_front" }]
        },
        {
          name: "esp32_board",
          tag: "0.1.0",
          instances: [{ instance_id: "esp32_1" }]
        }
      ]
    }"#;

    let response = LauncherRequest::new(first_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");
    assert!(response.success, "first config should succeed");

    assert_eq!(server.node_stack.len(), 3);
    assert!(server.node_stack.contains("uvc_camera", "0.1.0"));
    assert!(server.node_stack.contains("esp32_board", "0.1.0"));

    let second_config = r#"{
      deployments: [
        {
          name: "lidar_sensor",
          tag: "0.1.0",
          instances: [{ instance_id: "lidar_only" }]
        }
      ]
    }"#;

    let response = LauncherRequest::new(second_config, working_dir.path())
        .poll(
            &client.caller_handle,
            &client.master_node_name,
            CALLER_INSTANCE_ID,
            &client.master_node_name,
            Some(&client.instance_id),
            Duration::from_secs(2),
        )
        .await
        .expect("poll should succeed");
    assert!(response.success, "second config should succeed");

    assert_eq!(
        server.node_stack.len(),
        2,
        "stack should be replaced (master + lidar_sensor)"
    );
    assert!(server.node_stack.contains("lidar_sensor", "0.1.0"));
    assert!(
        !server.node_stack.contains("esp32_board", "0.1.0"),
        "nodes absent from the new plan should be removed"
    );

    let lidar_entity = server
        .node_stack
        .find("lidar_sensor", "0.1.0")
        .expect("lidar_sensor should exist");
    let lidar_instances: BTreeSet<String> = lidar_entity
        .instances()
        .iter()
        .map(|instance| instance.instance_id().as_str().to_owned())
        .collect();
    assert_eq!(lidar_instances, BTreeSet::from(["lidar_only".to_string()]));
}
