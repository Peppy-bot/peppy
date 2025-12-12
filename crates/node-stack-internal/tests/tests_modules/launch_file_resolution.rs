use std::collections::{BTreeMap, BTreeSet};

use crate::helpers::config_common::master_node_config;
use crate::helpers::config_common::{node_config, write_config_str};
use crate::helpers::resolver::StaticResolver;
use config::node::NodeConfig;
use config::test_helpers;
use node_stack::NodeStackError as Error;
use node_stack::{DeploymentPlanner, NodeStack};
use tempfile::TempDir;
use tempfile::tempdir;

// TODO: Create a test that makes sure that each `deployment` has at least one instance. Creating a deployment with 0 instance should result in an error.

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
fn optional_dependency_from_launcher_missing_is_unresolved() {
    let temp_dir = tempdir().expect("temp dir");

    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "1.0.0",
                    source: { repo: "https://example.com/repo.git" },
                    optional: true,
                    instances: [{ instance_id: "beta_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);

    let resolver = StaticResolver::new(vec![alpha_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        2,
        "missing optional deployment should still surface as unresolved when required",
    );

    let root = graph.root_index();
    let deployment_map = graph.get(root).expect("root node");
    assert_eq!(deployment_map.deployment().name, "alpha");
    assert!(!deployment_map.is_resolved());
    let root_error = deployment_map
        .error()
        .expect("alpha should carry resolution error");
    assert!(matches!(root_error, Error::MissingDependency { .. }));

    let beta_map = graph
        .children(root)
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "beta")
        .expect("beta dependency should be present even when optional");

    assert!(!beta_map.is_resolved());
    let error = beta_map
        .error()
        .expect("beta should carry resolution error");
    assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
}

#[test]
fn optional_dependency_from_launcher_with_wrong_tag_is_unresolved() {
    let temp_dir = tempdir().expect("temp dir");

    // The deployment exists on disk, but the tag does not
    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let beta_source = format!("file://{}/beta", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "2.0.0",
                    source: "$BETA_SOURCE",
                    optional: true,
                    instances: [{ instance_id: "beta_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "2.0.0")]);
    let beta_node = node_config("beta", "1.0.0", &[]);

    let resolver = StaticResolver::new(vec![alpha_node.clone(), beta_node]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        2,
        "optional deployment with mismatched tag should surface as unresolved",
    );

    let root = graph.root_index();
    let root_map = graph.get(root).expect("root node");
    assert_eq!(root_map.deployment().name, "alpha");
    assert!(!root_map.is_resolved());
    let root_error = root_map
        .error()
        .expect("alpha should carry resolution error");
    assert!(matches!(root_error, Error::MissingDependency { .. }));

    let beta_map = graph
        .children(root)
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "beta")
        .expect("beta dependency should be present even when unresolved");

    assert!(!beta_map.is_resolved());
    let error: &Error = beta_map
        .error()
        .expect("beta should carry resolution error");
    assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
}

#[test]
fn optional_dependency_resolved_allows_dependant_to_resolve() {
    let temp_dir = tempdir().expect("temp dir");

    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let beta_source = format!("file://{}/beta", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "1.0.0",
                    source: "$BETA_SOURCE",
                    optional: true,
                    instances: [{ instance_id: "beta_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);
    let beta_node = node_config("beta", "1.0.0", &[]);

    let resolver = StaticResolver::new(vec![alpha_node.clone(), beta_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(graph.len(), 2);

    let alpha_index = graph
        .indices()
        .into_iter()
        .find(|idx| {
            graph
                .get(*idx)
                .map(|m| m.deployment().name == "alpha")
                .unwrap_or(false)
        })
        .expect("alpha should be present");
    let alpha_map = graph.get(alpha_index).expect("alpha map");
    assert!(alpha_map.is_resolved());

    let beta_map = graph
        .children(alpha_index)
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "beta")
        .expect("beta dependency should be present");

    assert!(beta_map.is_resolved());
}

#[test]
fn optional_dependency_unresolved_causes_dependant_error() {
    let temp_dir = tempdir().expect("temp dir");

    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "1.0.0",
                    source: { repo: "https://example.com/repo.git" },
                    optional: true,
                    instances: [{ instance_id: "beta_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);
    let resolver = StaticResolver::new(vec![alpha_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(graph.len(), 2);

    let root = graph.root_index();
    let alpha_map = graph.get(root).expect("root node");
    assert_eq!(alpha_map.deployment().name, "alpha");
    assert!(!alpha_map.is_resolved());
    let error = alpha_map
        .error()
        .expect("alpha should carry resolution error");
    assert!(matches!(error, Error::MissingDependency { .. }));
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

#[test]
fn required_optional_dependency_surfaces_error() {
    let temp_dir = tempdir().expect("temp dir");

    // Alpha cannot be optional here since beta depends on it and it itself non-optional
    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let beta_source = format!("file://{}/beta", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    optional: true,
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "2.0.0",
                    source: "$BETA_SOURCE",
                    instances: [{ instance_id: "beta_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source),
    );

    let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

    let resolver = StaticResolver::new(vec![beta_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        2,
        "optional dependency should surface as unresolved when required by a non-optional deployment",
    );

    let root = graph.root_index();
    let root_map = graph.get(root).expect("root node");
    assert_eq!(root_map.deployment().name, "beta");
    assert!(!root_map.is_resolved());
    let root_error = root_map.error().expect("beta should have error");
    assert!(matches!(root_error, Error::MissingDependency { .. }));

    let alpha_map = graph
        .children(root)
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "alpha")
        .expect("alpha dependency should be present as unresolved");

    assert!(!alpha_map.is_resolved());
    let error = alpha_map
        .error()
        .expect("alpha should carry resolution error");

    assert!(matches!(error, Error::DeploymentNotResolvable(_, _)));
}

#[test]
fn unresolved_deployments_remain_in_graph() {
    let temp_dir = tempdir().expect("temp dir");

    // Alpha cannot be optional here since beta depends on it and is itself non-optional
    // gamma version 3.0.0 does not exist
    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let beta_source = format!("file://{}/beta", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    optional: true,
                    instances: [{ instance_id: "alpha_1" }]
                },
                {
                    name: "beta",
                    tag: "2.0.0",
                    source: "$BETA_SOURCE",
                    instances: [{ instance_id: "beta_1" }]
                },
                {
                    name: "gamma",
                    tag: "3.0.0",
                    source: "$BETA_SOURCE",
                    instances: [{ instance_id: "gamma_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source),
    );

    let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

    let resolver = StaticResolver::new(vec![beta_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        3,
        "entire deployment list should be represented"
    );

    let unresolved: Vec<_> = graph
        .indices()
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .filter(|map| !map.is_resolved())
        .collect();

    let mut unresolved_names: Vec<_> = unresolved
        .iter()
        .map(|map| map.deployment().name.clone())
        .collect();
    unresolved_names.sort();
    assert_eq!(
        unresolved_names.len(),
        3,
        "three deployments should contain errors"
    );

    assert_eq!(
        unresolved_names,
        vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()]
    );

    let unresolved_errors: Vec<_> = unresolved
        .iter()
        .map(|map| {
            map.error()
                .expect("unresolved deployment should carry error")
        })
        .collect();

    assert!(
        unresolved_errors.iter().all(|error| matches!(
            error,
            Error::DeploymentNotResolvable(_, _) | Error::MissingDependency { .. }
        )),
        "unexpected unresolved deployment error kind",
    );

    let beta_map = graph
        .indices()
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "beta")
        .expect("beta deployment should be present");

    assert!(!beta_map.is_resolved());
}

#[test]
fn missing_dependency_becomes_unresolved_node() {
    let temp_dir = tempdir().expect("temp dir");

    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "alpha",
                    tag: "1.0.0",
                    source: "$ALPHA_SOURCE",
                    instances: [{ instance_id: "alpha_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("delta", "1.0.0")]);
    let resolver = StaticResolver::new(vec![alpha_node.clone()]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        2,
        "missing dependency should be inserted as unresolved"
    );

    let delta_map = graph
        .indices()
        .into_iter()
        .filter_map(|index| graph.get(index))
        .find(|map| map.deployment().name == "delta")
        .expect("graph should contain unresolved delta dependency");

    assert!(!delta_map.is_resolved());
    let error = delta_map.error().expect("delta should carry error");
    let message = error.to_string();
    assert!(
        message.contains("dependency declared but missing"),
        "unexpected error message: {message}"
    );
}

#[test]
fn dependant_fails_when_dependency_missing_topic_interface() {
    let temp_dir = tempdir().expect("temp dir");

    let brain_source = format!("file://{}/brain", temp_dir.path().display());
    let lidar_source = format!("file://{}/lidar", temp_dir.path().display());
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        &r#"{
            deployments: [
                {
                    name: "brain",
                    tag: "1.0.0",
                    source: "$BRAIN_SOURCE",
                    instances: [{ instance_id: "brain_1" }]
                },
                {
                    name: "lidar",
                    tag: "1.0.0",
                    source: "$LIDAR_SOURCE",
                    instances: [{ instance_id: "lidar_1" }]
                }
            ]
        }"#
        .replace("$BRAIN_SOURCE", &brain_source)
        .replace("$LIDAR_SOURCE", &lidar_source),
    );

    let brain_node = node_config("brain", "1.0.0", &[("lidar", "1.0.0")]);
    let lidar_node: NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { name: "lidar", tag: "1.0.0" }
        }"#,
    )
    .expect("valid lidar node without exposes");

    let resolver = StaticResolver::new(vec![brain_node.clone(), lidar_node]);

    let planner = DeploymentPlanner::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("planner")
    .with_resolver(resolver);

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        2,
        "both deployments should remain in the graph"
    );

    // Find brain deployment - it should be unresolved because lidar doesn't expose the required topic
    let brain_map = graph
        .indices()
        .into_iter()
        .filter_map(|index| graph.get(index))
        .find(|map| map.deployment().name == "brain")
        .expect("brain deployment present");
    assert!(
        !brain_map.is_resolved(),
        "brain should fail to resolve without exposed lidar topic"
    );
    let error = brain_map.error().expect("brain error surfaces");
    let Error::MissingInterface {
        dependant,
        dependency,
        interface_kind,
        interface_name,
        ..
    } = error
    else {
        panic!("unexpected error type: {error:?}");
    };
    assert_eq!(dependant, "brain");
    assert_eq!(dependency, "lidar");
    assert_eq!(interface_kind, "topic");
    assert_eq!(interface_name, "lidar_topic");
}
