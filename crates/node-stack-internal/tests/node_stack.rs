use std::collections::{BTreeMap, BTreeSet};

use httptest::{Expectation, Server, matchers::request, responders::status_code};
use node_stack::{LocalNodeStackBuilder, NodeStackError};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

#[path = "./helpers/mod.rs"]
mod helpers;

/// Uses the following nodes:
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
fn test_local_stack_example_builds_dependencies() {
    // Create a local git repo that host 2 different nodes (only uvc_camera and lidar_sensor will be pulled in this test)
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    // The directory in which the peppy config lives will contain a `.peppy/nodes` folder where cached nodes will be pulled from a local git repo
    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();
    // Only pull uvc_camera and lidar_sensor from git
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let lidar_remote = format!("nodes/{}", helpers::LIDAR_SENSOR_NODE_NAME);
    let uvc_remote = format!("nodes/{}", helpers::UVC_CAMERA_NODE_NAME);

    let launch_file = helpers::render_peppy_config_template(
        &root_temp_dir,
        helpers::PeppyConfigTemplateExample1 {
            lidar_sensor_node_name: helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote.as_str(),
            lidar_sensor_github_tag: "0.1.0",
            uvc_camera_node_name: helpers::UVC_CAMERA_NODE_NAME,
            uvc_camera_github_repo: &git_repo_path,
            uvc_camera_github_repo_path: uvc_remote.as_str(),
            web_video_stream_node_name: helpers::WEB_VIDEO_STREAM_NODE_NAME,
            web_video_stream_optional: false,
            brain_node_name: helpers::BRAIN_NODE_NAME,
            controller_node_name: helpers::CONTROLLER_NODE_NAME,
        },
    );

    let node_path = |name: &str| root.join(name).join("peppy.json5");

    // Add web_video_stream locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::WEB_VIDEO_STREAM_NODE_NAME),
        helpers::WebStreamVideoStreamNodeTemplate::new(
            helpers::WEB_VIDEO_STREAM_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
        ),
    );

    // Add brain locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::BRAIN_NODE_NAME),
        helpers::BrainNodeTemplate::new(
            helpers::BRAIN_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
            helpers::LIDAR_SENSOR_NODE_NAME,
            helpers::CONTROLLER_NODE_NAME,
        ),
    );

    // Add controller locally to the node_stack
    helpers::add_local_web_video_stream(
        node_path(helpers::CONTROLLER_NODE_NAME),
        helpers::ControllerNodeTemplate::new(helpers::CONTROLLER_NODE_NAME),
    );

    // DO NOT add lidar_sensor and uvc_camera to the node stack, they will be automatically pulled fromt the local github repo

    let mapper = LocalNodeStackBuilder::from_launch_file(launch_file, None).unwrap();
    let planner = mapper.build().unwrap();

    // Supposed to contain the local nodes stacked in the project directory
    assert_eq!(planner.node_stack().len(), 3);

    // Now take care of the deployments (git pull etc...)
    let deployment_tree = planner.map_deployments_to_nodes();

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );
    assert!(
        helpers::cached_node_exists(&nodes_cache_dir, helpers::UVC_CAMERA_NODE_NAME),
        "uvc_camera should be cached under {:?}",
        nodes_cache_dir
    );
    assert!(
        helpers::cached_node_exists(&nodes_cache_dir, helpers::LIDAR_SENSOR_NODE_NAME),
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
                        .clone()
                })
                .collect();

            (map.deployment().name.clone(), deps)
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
        actual(helpers::BRAIN_NODE_NAME),
        expected(&[
            helpers::CONTROLLER_NODE_NAME,
            helpers::LIDAR_SENSOR_NODE_NAME,
            helpers::UVC_CAMERA_NODE_NAME,
        ]),
        "brain dependencies"
    );
    assert_eq!(
        actual(helpers::CONTROLLER_NODE_NAME),
        expected(&[]),
        "controller dependencies"
    );
    assert_eq!(
        actual(helpers::WEB_VIDEO_STREAM_NODE_NAME),
        expected(&[helpers::UVC_CAMERA_NODE_NAME]),
        "web_video_stream dependencies"
    );

    helpers::print_dependency_summary(&deps_by_name);
}

#[test]
fn test_optional_node_ignored() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let uvc_remote = format!("nodes/{}", helpers::UVC_CAMERA_NODE_NAME);
    let web_remote = format!("nodes/{}", helpers::WEB_VIDEO_STREAM_NODE_NAME);

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
          namespace: "/camera/right",
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
          namespace: "/camera/stream/right",
          parameters: {{
            cameras_namespaces: [
              "/camera/right",
              "/camera/left"
            ],
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
        uvc = helpers::UVC_CAMERA_NODE_NAME,
        repo = git_repo_path,
        uvc_remote = uvc_remote,
        web = helpers::WEB_VIDEO_STREAM_NODE_NAME,
        web_remote = web_remote,
    );

    let launch_file = root.join("peppy_launcher.json5");
    std::fs::write(&launch_file, launch_content).expect("failed to write launch config");

    let mapper = LocalNodeStackBuilder::from_launch_file(&launch_file, None).unwrap();
    let planner = mapper.build().unwrap();

    assert!(
        planner.node_stack().is_empty(),
        "optional node test config should rely on remote nodes only"
    );

    let graph = planner.map_deployments_to_nodes();
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
        helpers::UVC_CAMERA_NODE_NAME,
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
                .clone()
        })
        .collect();
    assert!(
        !present_names.contains(helpers::WEB_VIDEO_STREAM_NODE_NAME),
        "optional deployment should not appear when it fails to resolve"
    );

    let nodes_cache_dir = root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should exist",
        nodes_cache_dir
    );
    assert!(
        helpers::cached_node_exists(&nodes_cache_dir, helpers::UVC_CAMERA_NODE_NAME),
        "required node should be cached under {:?}",
        nodes_cache_dir
    );
}

/// Uses config example 2 where the lidar sensor requests tag `v2.0` but the
/// repository only exposes `v1.0`. The deployment must remain unresolved when
/// the requested tag differs from what is available.
#[test]
fn test_remote_git_tag_mismatch_is_unresolvable() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let lidar_remote_path = format!("nodes/{}", helpers::LIDAR_SENSOR_NODE_NAME);
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let launch_file = helpers::render_peppy_config_template(
        &root_temp_dir,
        helpers::PeppyConfigTemplateExample2 {
            lidar_sensor_node_name: helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote_path.as_str(),
        },
    );

    let mapper = LocalNodeStackBuilder::from_launch_file(launch_file, None).unwrap();
    let planner = mapper.build().unwrap();

    assert!(
        planner.node_stack().is_empty(),
        "example 2 config should not include local nodes"
    );

    let graph = planner.map_deployments_to_nodes();
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
        helpers::LIDAR_SENSOR_NODE_NAME,
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
fn test_remote_bundle_manifest_tag_mismatch_is_unresolvable() {
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
        helpers::LIDAR_SENSOR_NODE_NAME
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
    let launch_file = helpers::render_peppy_config_template(
        &root_temp_dir,
        helpers::PeppyConfigTemplateExample3 {
            lidar_sensor_node_name: helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_url: bundle_url.as_str(),
            lidar_sensor_sha256: checksum.as_str(),
        },
    );

    let mapper = LocalNodeStackBuilder::from_launch_file(launch_file, None).unwrap();
    let planner = mapper.build().unwrap();

    assert!(
        planner.node_stack().is_empty(),
        "example 3 config should not include local nodes"
    );

    let graph = planner.map_deployments_to_nodes();
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
        helpers::LIDAR_SENSOR_NODE_NAME,
        lidar_deployment.deployment().tag
    );
    assert_eq!(identifier, &expected_identifier);
    assert!(
        reason.contains(helpers::LIDAR_SENSOR_NODE_NAME),
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
fn test_remote_git_parameter_mismatch_is_rejected() {
    let git_repo_temp_dir = TempDir::new().unwrap();
    let git_repo_path = helpers::create_git_repo(&git_repo_temp_dir);

    let root_temp_dir = TempDir::new().unwrap();
    let root = root_temp_dir.path();

    let lidar_remote_path = format!("nodes/{}", helpers::LIDAR_SENSOR_NODE_NAME);
    let git_repo_path = git_repo_path.to_str().unwrap().to_owned();
    let launch_file = helpers::render_peppy_config_template(
        &root_temp_dir,
        helpers::PeppyConfigTemplateExample4 {
            lidar_sensor_node_name: helpers::LIDAR_SENSOR_NODE_NAME,
            lidar_sensor_github_repo: &git_repo_path,
            lidar_sensor_github_repo_path: lidar_remote_path.as_str(),
        },
    );

    let mapper = LocalNodeStackBuilder::from_launch_file(launch_file, None).unwrap();
    let planner = mapper.build().unwrap();

    assert!(
        planner.node_stack().is_empty(),
        "example 4 config should not include local nodes"
    );

    let graph = planner.map_deployments_to_nodes();
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
        helpers::LIDAR_SENSOR_NODE_NAME,
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
