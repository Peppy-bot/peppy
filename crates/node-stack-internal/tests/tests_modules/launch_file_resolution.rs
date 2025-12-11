use std::collections::{BTreeMap, BTreeSet};

use config::test_helpers;
use httptest::{Expectation, Server, matchers::request, responders::status_code};
use node_stack::{DeploymentPlanner, NodeStackError};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

use crate::helpers::config_common::master_node_config;

/// Launches the following nodes:
/// - brain
/// - controller
/// - lidar_sensor
/// - uvc_camera
/// - web_video_stream
///
/// With the following dependencies:
/// - `brain` depends on `lidar_sensor`, `uvc_camera`, and `controller`
///   (`subscribes_to.topics` and `subscribes_to.actions` properties)
/// - `controller` does not declare dependencies in this example
/// - `web_video_stream` depends on `uvc_camera` (`subscribes_to.topics` property)
#[test]
fn launcher_file_resolves_dependency_graph() {
    // Create a local git repo that host 2 different nodes (only uvc_camera and lidar_sensor will be pulled in this test)
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    // The directory in which the peppy config lives will contain a `.peppy/nodes` folder where cached nodes will be pulled from a local git repo
    let project_dir = TempDir::new().unwrap();
    let project_dir = project_dir.path();
    // Only pull uvc_camera and lidar_sensor from git
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let lidar_remote = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let uvc_remote = format!("nodes/{}", test_helpers::UVC_CAMERA_NODE_NAME);

    let launch_file = test_helpers::render_peppy_config_template(
        &project_dir,
        test_helpers::PeppyConfigTemplateExample1 {
            lidar_sensor_node_name: test_helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote.as_str(),
            lidar_sensor_github_tag: "0.1.0",
            uvc_camera_node_name: test_helpers::UVC_CAMERA_NODE_NAME,
            uvc_camera_github_repo: &git_repo_path,
            uvc_camera_github_repo_path: uvc_remote.as_str(),
            web_video_stream_node_name: test_helpers::WEB_VIDEO_STREAM_NODE_NAME,
            web_video_stream_optional: false,
            brain_node_name: test_helpers::BRAIN_NODE_NAME,
            controller_node_name: test_helpers::CONTROLLER_NODE_NAME,
        },
    );
    // Verify the rendered launch file content
    let launch_content = std::fs::read_to_string(&launch_file).expect("failed to read launch file");
    let expected_launch_content = format!(
        r#"{{
  deployments: [
    {{
      name: "{lidar_sensor}",
      source: {{
        repo: "{git_repo}",
        path: "{lidar_remote}"
      }},
      tag: "0.1.0",
      instances: [
        {{
          instance_id: "lidar_1",
          parameters: {{
            device: {{
              physical: "/dev/lidar1",
              sim: "mujoco:lidar1",
              priority: "sim"
            }},
            lidar_point: {{
              x: 12.34, // meters, X coordinate in 3D space
              y: -7.56, // meters, Y coordinate in 3D space
              z: 1.23, // meters, Z coordinate in 3D space (height)
              intensity: 0.85, // normalized intensity of return signal (0 to 1)
              return_type: 1, // e.g. 1 = first return, 2 = last return
              classification: 2, // e.g. 2 = ground, 5 = vegetation
              timestamp: 1696285145999, // Unix timestamp in milliseconds
            }}
          }}
        }}
      ]
    }},
    {{
      name: "{uvc_camera}",
      source: {{
        repo: "{git_repo}",
        path: "{uvc_remote}"
      }},
      tag: "0.1.0",
      instances: [
        {{
          instance_id: "camera_right",
          parameters: {{
            device: {{
              physical: "/dev/video_right",
              sim: "mujoco:camera_right",
              priority: "physical"
            }},
            video: {{
              frame_rate: 30,
              resolution: {{
                width: 1920,
                height: 1080,
              }},
              encoding: "yuyv",
            }},
          }}
        }},
        {{
          instance_id: "camera_left",
          parameters: {{
            device: {{
              physical: "/dev/video_left",
              sim: "mujoco:camera_left",
              priority: "physical"
            }},
            video: {{
              frame_rate: 30,
              resolution: {{
                width: 1920,
                height: 1080,
              }},
              encoding: "yuyv",
            }},
          }}
        }}
      ]
    }},
    // `web_video_stream` depends on `uvc_camera`
    {{
      // The test will add the web_video_stream to the local stack
      name: "{web_video_stream}",
      tag: "0.1.0",
      // Since it's optional, if the node cannot be found, it will be ignored
      optional: false,
      instances: [
        {{
          instance_id: "stream_1",
          parameters: {{
            http: {{
              host: "0.0.0.0",
              port: 8083,
              cors_enabled: false,
              cors_origins: "*",
              max_connections: "2000",
              request_timeout_ms: "3000",
            }},
            video_stream: {{
              format: "mjpeg",
              quality: 3,
              max_fps: 30,
            }},
          }}
        }}
      ]
    }},
    {{
      name: "{brain}",
      // The test will add the brain_node to the local stack
      tag: "0.1.0",
      instances: [
        {{
          instance_id: "the_brain",
          parameters: {{}}
        }}
      ],
    }},
    {{
      name: "{controller}",
      // The test will add the controller_node to the local stack
      tag: "0.1.0",
      instances: [
        {{
          instance_id: "the_nervous_system",
          parameters: {{}}
        }}
      ]
    }},
  ],
  logging: {{
    min_level: "info",
    format: "text"
  }}
}}"#,
        lidar_sensor = test_helpers::LIDAR_SENSOR_NODE_NAME,
        git_repo = git_repo_path,
        lidar_remote = lidar_remote,
        uvc_camera = test_helpers::UVC_CAMERA_NODE_NAME,
        uvc_remote = uvc_remote,
        web_video_stream = test_helpers::WEB_VIDEO_STREAM_NODE_NAME,
        brain = test_helpers::BRAIN_NODE_NAME,
        controller = test_helpers::CONTROLLER_NODE_NAME,
    );
    assert_eq!(
        launch_content, expected_launch_content,
        "rendered launch file content should match expected json5"
    );

    let node_path = |name: &str| project_dir.join(name).join("peppy.json5");

    // Add web_video_stream to a child folder where launch_file is located
    test_helpers::add_local_web_video_stream(
        node_path(test_helpers::WEB_VIDEO_STREAM_NODE_NAME),
        test_helpers::WebStreamVideoStreamNodeTemplate::new(
            test_helpers::WEB_VIDEO_STREAM_NODE_NAME,
            test_helpers::UVC_CAMERA_NODE_NAME,
        ),
    );

    // Add brain to a child folder where launch_file is located
    test_helpers::add_local_web_video_stream(
        node_path(test_helpers::BRAIN_NODE_NAME),
        test_helpers::BrainNodeTemplate::new(
            test_helpers::BRAIN_NODE_NAME,
            test_helpers::UVC_CAMERA_NODE_NAME,
            test_helpers::LIDAR_SENSOR_NODE_NAME,
            test_helpers::CONTROLLER_NODE_NAME,
        ),
    );

    // Add controller to a child folder where launch_file is located
    test_helpers::add_local_web_video_stream(
        node_path(test_helpers::CONTROLLER_NODE_NAME),
        test_helpers::ControllerNodeTemplate::new(test_helpers::CONTROLLER_NODE_NAME),
    );

    // DO NOT add lidar_sensor and uvc_camera to project_dir, they will be automatically pulled fromt the local github repo

    let planner =
        DeploymentPlanner::from_launch_file(master_node_config(), launch_file, None).unwrap();

    // Supposed to contain the master node + brain + controller + web_video_stream stacked in the project directory
    assert_eq!(planner.node_stack().len(), 4);

    // Now take care of the deployments (git pull etc...)
    let deployment_tree = planner.create_deployment_graph();

    let nodes_cache_dir = project_dir.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );
    assert!(
        test_helpers::cached_node_exists(&nodes_cache_dir, test_helpers::UVC_CAMERA_NODE_NAME),
        "uvc_camera should be cached under {:?}",
        nodes_cache_dir
    );
    assert!(
        test_helpers::cached_node_exists(&nodes_cache_dir, test_helpers::LIDAR_SENSOR_NODE_NAME),
        "lidar_sensor should be cached under {:?}",
        nodes_cache_dir
    );

    assert!(
        deployment_tree.len() >= 5,
        "deployment graph should contain all nodes"
    );

    let deps_by_name: BTreeMap<String, Vec<String>> = deployment_tree
        .indices()
        .into_iter()
        .map(|index| {
            let map = deployment_tree
                .get(index)
                .expect("deployment graph should return node for index");

            assert!(
                map.is_resolved(),
                "deployment {}:{} must resolve",
                map.deployment().name,
                map.deployment().tag
            );

            let deps = deployment_tree
                .children(index)
                .into_iter()
                .map(|child| {
                    deployment_tree
                        .get(child)
                        .expect("dependency node must exist")
                        .deployment()
                        .name
                        .to_string()
                })
                .collect();

            (map.deployment().name.to_string(), deps)
        })
        .collect();

    let expected = |names: &[&str]| -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    };
    let actual = |name: &str| -> BTreeSet<String> {
        deps_by_name
            .get(name)
            .cloned()
            .unwrap_or_else(|| panic!("{} node should be present", name))
            .into_iter()
            .collect()
    };

    assert_eq!(
        actual(test_helpers::BRAIN_NODE_NAME),
        expected(&[
            test_helpers::CONTROLLER_NODE_NAME,
            test_helpers::LIDAR_SENSOR_NODE_NAME,
            test_helpers::UVC_CAMERA_NODE_NAME,
        ]),
        "brain dependencies"
    );
    assert_eq!(
        actual(test_helpers::CONTROLLER_NODE_NAME),
        expected(&[]),
        "controller dependencies"
    );
    assert_eq!(
        actual(test_helpers::WEB_VIDEO_STREAM_NODE_NAME),
        expected(&[test_helpers::UVC_CAMERA_NODE_NAME]),
        "web_video_stream dependencies"
    );

    test_helpers::print_dependency_summary(&deps_by_name);
}

#[test]
fn optional_node_excluded_when_unresolvable() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let uvc_remote = format!("nodes/{}", test_helpers::UVC_CAMERA_NODE_NAME);
    let web_remote = format!("nodes/{}", test_helpers::WEB_VIDEO_STREAM_NODE_NAME);

    let launch_content = format!(
        r#"{{
  deployments: [
    {{
      name: "{uvc}",
      source: {{
        repo: "{repo}",
        path: "{uvc_remote}"
      }},
      tag: "0.1.0",
      instances: [
        {{
          instance_id: "camera_right",
          parameters: {{
            device: {{
              physical: "/dev/video_right",
              sim: "mujoco:camera_right",
              priority: "physical"
            }},
            video: {{
              frame_rate: 30,
              resolution: {{
                width: 1920,
                height: 1080,
              }},
              encoding: "yuyv",
            }},
          }}
        }}
      ]
    }},
    {{
      name: "{web}",
      source: {{
        repo: "{repo}",
        path: "{web_remote}"
      }},
      optional: true,
      tag: "9.9.9",
      instances: [
        {{
          instance_id: "video_stream1",
          parameters: {{
            http: {{
              host: "0.0.0.0",
              port: 8083,
              cors_enabled: false,
              cors_origins: "*",
              max_connections: "2000",
              request_timeout_ms: "3000",
            }},
            video_stream: {{
              format: "mjpeg",
              quality: 3,
              max_fps: 30,
            }},
          }}
        }}
      ]
    }}
  ],
  logging: {{
    min_level: "info",
    format: "text"
  }}
}}"#,
        uvc = test_helpers::UVC_CAMERA_NODE_NAME,
        repo = git_repo_path,
        uvc_remote = uvc_remote,
        web = test_helpers::WEB_VIDEO_STREAM_NODE_NAME,
        web_remote = web_remote,
    );

    let launch_file = root.join("peppy_launcher.json5");
    std::fs::write(&launch_file, launch_content).expect("failed to write launch config");

    let planner =
        DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None).unwrap();

    assert_eq!(
        planner.node_stack().len(),
        1,
        "optional node test config should only have root node (no local nodes)"
    );

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        1,
        "optional deployment should be ignored when it cannot resolve"
    );

    let root_index = graph.root_index();
    let required = graph
        .get(root_index)
        .expect("graph should contain the required deployment");
    assert!(required.is_resolved(), "required deployment must resolve");
    assert_eq!(
        required.deployment().name,
        test_helpers::UVC_CAMERA_NODE_NAME,
        "only the required deployment should remain in the graph"
    );

    let present_names: BTreeSet<String> = graph
        .indices()
        .into_iter()
        .map(|index| {
            graph
                .get(index)
                .expect("deployment must exist")
                .deployment()
                .name
                .to_string()
        })
        .collect();
    assert!(
        !present_names.contains(test_helpers::WEB_VIDEO_STREAM_NODE_NAME),
        "optional deployment should not appear when it fails to resolve"
    );

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );
    assert!(
        test_helpers::cached_node_exists(&nodes_cache_dir, test_helpers::UVC_CAMERA_NODE_NAME),
        "required node should be cached under {:?}",
        nodes_cache_dir
    );
}

/// Uses config example 2 where the lidar sensor requests tag `v2.0` but the
/// repository only exposes `v1.0`. The deployment must remain unresolved when
/// the requested tag differs from what is available.
#[test]
fn remote_git_tag_not_found_is_unresolvable() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let lidar_remote_path = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let launch_file = test_helpers::render_peppy_config_template(
        &root_temp_dir,
        test_helpers::PeppyConfigTemplateExample2 {
            lidar_sensor_node_name: test_helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote_path.as_str(),
        },
    );

    let planner =
        DeploymentPlanner::from_launch_file(master_node_config(), launch_file, None).unwrap();

    assert_eq!(
        planner.node_stack().len(),
        1,
        "example 2 config should only have root node (no local nodes)"
    );

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        1,
        "only the lidar deployment should be present"
    );

    let root_index = graph.root_index();
    let lidar_deployment = graph
        .get(root_index)
        .expect("deployment graph should contain the lidar node");

    assert!(
        !lidar_deployment.is_resolved(),
        "lidar deployment should fail to resolve when tag differs"
    );

    let error = lidar_deployment
        .error()
        .expect("deployment must report the resolution failure");

    let NodeStackError::DeploymentNotResolvable(deployment, reason) = error else {
        panic!("unexpected error type: {error:?}");
    };

    let expected = format!(
        "{}:{}",
        test_helpers::LIDAR_SENSOR_NODE_NAME,
        lidar_deployment.deployment().tag
    );
    assert_eq!(deployment, &expected);
    assert!(
        reason.contains("Cannot find the node"),
        "expected missing node reason, got: {}",
        reason
    );

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        nodes_cache_dir
    );
}

/// Uses the example where the lidar bundle is reachable but the manifest inside
/// advertises a different tag than the one requested in the deployment.
#[test]
fn remote_bundle_manifest_tag_mismatch_is_unresolvable() {
    const BUNDLE_PATH: &str = "/bundles/lidar_sensor.tar.zst";

    let server = Server::run();

    let build_bundle = |manifest: &str| -> Vec<u8> {
        let temp_dir = tempfile::tempdir().unwrap();
        let manifest_path = temp_dir.path().join("peppy.json5");
        std::fs::write(&manifest_path, manifest).expect("write manifest");

        let mut tar_data = Vec::new();
        {
            let mut tar_builder = tar::Builder::new(&mut tar_data);
            tar_builder
                .append_path_with_name(&manifest_path, "peppy.json5")
                .expect("append manifest to tar");
            tar_builder.finish().expect("finish tar");
        }

        let cursor = std::io::Cursor::new(tar_data);
        zstd::stream::encode_all(cursor, 0).expect("compress bundle")
    };

    let manifest_content = format!(
        "{{\n            schema_version: 1,\n            manifest: {{ name: \"{}\", tag: \"9.9.9\" }}\n        }}",
        test_helpers::LIDAR_SENSOR_NODE_NAME
    );
    let bundle_bytes = build_bundle(manifest_content.as_str());

    let mut hasher = Sha256::new();
    hasher.update(&bundle_bytes);
    let checksum = format!("sha256:{:x}", hasher.finalize());

    server.expect(
        Expectation::matching(request::method_path("GET", BUNDLE_PATH))
            .respond_with(status_code(200).body(bundle_bytes.clone())),
    );

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let bundle_url = server.url(BUNDLE_PATH).to_string();
    let launch_file = test_helpers::render_peppy_config_template(
        &root_temp_dir,
        test_helpers::PeppyConfigTemplateExample3 {
            lidar_sensor_node_name: test_helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_url: bundle_url.as_str(),
            lidar_sensor_sha256: checksum.as_str(),
        },
    );

    let planner =
        DeploymentPlanner::from_launch_file(master_node_config(), launch_file, None).unwrap();

    assert_eq!(
        planner.node_stack().len(),
        1,
        "example 3 config should only have root node (no local nodes)"
    );

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        1,
        "only the lidar deployment should be present"
    );

    let root_index = graph.root_index();
    let lidar_deployment = graph
        .get(root_index)
        .expect("deployment graph should contain the lidar node");

    assert!(
        !lidar_deployment.is_resolved(),
        "lidar deployment should fail to resolve when manifest tag differs"
    );

    let error = lidar_deployment
        .error()
        .expect("deployment must report the resolution failure");

    let NodeStackError::DeploymentNotResolvable(identifier, reason) = error else {
        panic!("unexpected error type: {error:?}");
    };

    let expected_identifier = format!(
        "{}:{}",
        test_helpers::LIDAR_SENSOR_NODE_NAME,
        lidar_deployment.deployment().tag
    );
    assert_eq!(identifier, &expected_identifier);
    assert!(
        reason.contains(test_helpers::LIDAR_SENSOR_NODE_NAME),
        "error reason should mention lidar sensor, got: {}",
        reason
    );
    assert!(
        reason.contains(lidar_deployment.deployment().tag.as_str()),
        "error reason should mention expected tag, got: {}",
        reason
    );

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        nodes_cache_dir
    );
}

/// Uses the example where lidar parameters reference fields unsupported by the
/// node manifest. The deployment should surface a `WrongInputParameters` error.
#[test]
fn remote_git_invalid_parameters_rejected() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let lidar_remote_path = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let launch_file = test_helpers::render_peppy_config_template(
        &root_temp_dir,
        test_helpers::PeppyConfigTemplateExample4 {
            lidar_sensor_node_name: test_helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote_path.as_str(),
        },
    );

    let planner =
        DeploymentPlanner::from_launch_file(master_node_config(), launch_file, None).unwrap();

    assert_eq!(
        planner.node_stack().len(),
        1,
        "example 4 config should only have root node (no local nodes)"
    );

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        1,
        "only the lidar deployment should be present"
    );

    let root_index = graph.root_index();
    let lidar_deployment = graph
        .get(root_index)
        .expect("deployment graph should contain the lidar node");

    assert!(
        !lidar_deployment.is_resolved(),
        "lidar deployment should fail to resolve when parameters mismatch"
    );

    let error = lidar_deployment
        .error()
        .expect("deployment must report the parameter validation failure");

    let NodeStackError::WrongInputParameters {
        deployment,
        expected,
        unexpected,
    } = error
    else {
        panic!("unexpected error type: {error:?}");
    };

    let expected_identifier = format!(
        "{}:{}",
        test_helpers::LIDAR_SENSOR_NODE_NAME,
        lidar_deployment.deployment().tag
    );
    assert_eq!(deployment, &expected_identifier);

    let expected_parameters: BTreeSet<String> = [
        "device.physical",
        "device.priority",
        "device.sim",
        "lidar_point.classification",
        "lidar_point.intensity",
        "lidar_point.return_type",
        "lidar_point.timestamp",
        "lidar_point.x",
        "lidar_point.y",
        "lidar_point.z",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    let actual_expected: BTreeSet<String> = expected.iter().cloned().collect();
    assert_eq!(
        actual_expected, expected_parameters,
        "expected parameters should list all manifest fields"
    );

    let actual_unexpected: BTreeSet<String> = unexpected.iter().cloned().collect();
    let unexpected_parameters: BTreeSet<String> =
        [String::from("lidar_point.fps")].into_iter().collect();
    assert_eq!(
        actual_unexpected, unexpected_parameters,
        "unexpected parameters should only include lidar_point.fps"
    );

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        nodes_cache_dir
    );
}
