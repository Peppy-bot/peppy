use crate::helpers::config_common::master_node_config;
use crate::helpers::config_common::{node_config, write_config_str};
use crate::helpers::git::create_simple_git_repo;
use crate::helpers::http::create_http_bundle;
use config::node::NodeConfig;
use config::test_helpers;
use httptest::{Expectation, Server, matchers::request, responders::status_code};
use node_stack::NodeStackError as Error;
use node_stack::{LaunchPlan, NodeStack};
use std::collections::{BTreeMap, BTreeSet};
use tempfile::TempDir;
use tempfile::tempdir;

fn deps(names: &[&str]) -> BTreeSet<String> {
    names.iter().map(|name| (*name).to_string()).collect()
}

fn deps_of(name: &str, deps_by_name: &BTreeMap<String, Vec<String>>) -> BTreeSet<String> {
    deps_by_name
        .get(name)
        .cloned()
        .unwrap_or_else(|| panic!("{} node should be present", name))
        .into_iter()
        .collect()
}

fn assert_deployment_not_resolvable(
    plan: &LaunchPlan,
    deployment_name: &str,
    expected_identifier: &str,
    expected_reason_substring: &str,
) {
    let planned = plan
        .report()
        .find_deployment_by_name(deployment_name)
        .unwrap_or_else(|| panic!("deployment '{deployment_name}' not found"));

    assert!(
        !planned.is_resolved(),
        "{deployment_name} should be unresolved"
    );

    let error = planned
        .error()
        .expect("unresolved deployment should carry error");
    let Error::DeploymentNotResolvable(identifier, reason) = error else {
        panic!("expected DeploymentNotResolvable, got: {error:?}");
    };

    assert_eq!(identifier, expected_identifier);
    assert!(
        reason.contains(expected_reason_substring),
        "reason '{reason}' should contain '{expected_reason_substring}'"
    );
}

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

    let launch_content = r#"{
      deployments: [
        {
          name: "$LIDAR_SENSOR",
          source: {
            repo: "$GIT_REPO",
            path: "$LIDAR_REMOTE"
          },
          tag: "0.1.0",
          instances: [
            {
              instance_id: "lidar_1",
              parameters: {
                device: {
                  physical: "/dev/lidar1",
                  sim: "mujoco:lidar1",
                  priority: "sim"
                },
                lidar_point: {
                  x: 12.34, // meters, X coordinate in 3D space
                  y: -7.56, // meters, Y coordinate in 3D space
                  z: 1.23, // meters, Z coordinate in 3D space (height)
                  intensity: 0.85, // normalized intensity of return signal (0 to 1)
                  return_type: 1, // e.g. 1 = first return, 2 = last return
                  classification: 2, // e.g. 2 = ground, 5 = vegetation
                  timestamp: 1696285145999, // Unix timestamp in milliseconds
                }
              }
            }
          ]
        },
        {
          name: "$UVC_CAMERA",
          source: {
            repo: "$GIT_REPO",
            path: "$UVC_REMOTE"
          },
          tag: "0.1.0",
          instances: [
            {
              instance_id: "camera_right",
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
              instance_id: "camera_left",
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
        // `web_video_stream` depends on `uvc_camera`
        {
          // The test will add the web_video_stream to the local stack
          name: "$WEB_VIDEO_STREAM",
          tag: "0.1.0",
          // Since it's optional, if the node cannot be found, it will be ignored
          optional: false,
          instances: [
            {
              instance_id: "stream_1",
              parameters: {
                http: {
                  host: "0.0.0.0",
                  port: 8083,
                  cors_enabled: false,
                  cors_origins: "*",
                  max_connections: 2000,
                  request_timeout_ms: 3000,
                },
                video_stream: {
                  format: "mjpeg",
                  quality: 3,
                  max_fps: 30,
                },
              }
            }
          ]
        },
        {
          name: "$BRAIN",
          // The test will add the brain_node to the local stack
          tag: "0.1.0",
          instances: [
            {
              instance_id: "the_brain",
              parameters: {}
            }
          ],
        },
        {
          name: "$CONTROLLER",
          // The test will add the controller_node to the local stack
          tag: "0.1.0",
          instances: [
            {
              instance_id: "the_nervous_system",
              parameters: {}
            }
          ]
        },
      ],
      logging: {
        min_level: "info",
        format: "text"
      }
    }"#
    .replace("$LIDAR_SENSOR", test_helpers::LIDAR_SENSOR_NODE_NAME)
    .replace("$GIT_REPO", &git_repo_path)
    .replace("$LIDAR_REMOTE", &lidar_remote)
    .replace("$UVC_CAMERA", test_helpers::UVC_CAMERA_NODE_NAME)
    .replace("$UVC_REMOTE", &uvc_remote)
    .replace(
        "$WEB_VIDEO_STREAM",
        test_helpers::WEB_VIDEO_STREAM_NODE_NAME,
    )
    .replace("$BRAIN", test_helpers::BRAIN_NODE_NAME)
    .replace("$CONTROLLER", test_helpers::CONTROLLER_NODE_NAME);

    let launch_file = write_config_str(project_dir.join("peppy_launcher.json5"), &launch_content);

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

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();

    assert_eq!(
        stack.len(),
        6,
        "stack should contain master + all launcher deployments"
    );
    assert!(
        plan.report().dependency_errors().is_empty(),
        "expected no dependency errors, got: {:?}",
        plan.report().dependency_errors()
    );
    for deployment in plan.report().deployments() {
        assert!(
            deployment.is_resolved(),
            "deployment {}:{} should resolve, got {:?}",
            deployment.deployment().name.as_str(),
            deployment.deployment().tag,
            deployment.error()
        );
    }

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

    let deps_by_name: BTreeMap<String, Vec<String>> = stack
        .snapshot()
        .into_iter()
        .filter(|entity| entity.config().manifest.name.as_str() != "master")
        .map(|entity| {
            let name = entity.config().manifest.name.as_str().to_string();
            let tag = entity.config().manifest.tag.clone();
            let deps = stack
                .dependencies_of(&name, &tag)
                .into_iter()
                .map(|dependency| dependency.config().manifest.name.as_str().to_string())
                .collect();
            (name, deps)
        })
        .collect();

    assert_eq!(
        deps_of(test_helpers::BRAIN_NODE_NAME, &deps_by_name),
        deps(&[
            test_helpers::CONTROLLER_NODE_NAME,
            test_helpers::LIDAR_SENSOR_NODE_NAME,
            test_helpers::UVC_CAMERA_NODE_NAME,
        ]),
        "brain dependencies"
    );
    assert_eq!(
        deps_of(test_helpers::CONTROLLER_NODE_NAME, &deps_by_name),
        deps(&[]),
        "controller dependencies"
    );
    assert_eq!(
        deps_of(test_helpers::WEB_VIDEO_STREAM_NODE_NAME, &deps_by_name),
        deps(&[test_helpers::UVC_CAMERA_NODE_NAME]),
        "web_video_stream dependencies"
    );

    test_helpers::print_dependency_summary(&deps_by_name);
}

#[test]
fn launcher_deployment_with_zero_instances_is_unresolved() {
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
                    // This should not work
                    instances: []
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source),
    );

    let plan = LaunchPlan::with_nodes(
        &launch_file,
        None,
        NodeStack::new(master_node_config(), None),
    )
    .expect("plan");

    assert_eq!(plan.node_stack().len(), 1, "only master should be present");

    let alpha = plan
        .report()
        .find_deployment_by_name("alpha")
        .expect("alpha deployment should be present");
    assert!(!alpha.is_resolved(), "alpha should be unresolved");

    let error = alpha.error().expect("alpha should carry an error");
    let Error::DeploymentNotResolvable(_, reason) = error else {
        panic!("expected DeploymentNotResolvable, got {error:?}");
    };
    assert!(
        reason.contains("at least one instance"),
        "unexpected reason: {reason}"
    );
}

/// Mismatched manifest fields (name or tag) in downloaded node vs launcher config makes remote deployment unresolvable.
#[test]
fn remote_deployment_manifest_mismatch_is_unresolved() {
    fn assert_mismatch(manifest_name: &str, manifest_tag: &str, expected_reason_substring: &str) {
        let expected_identifier = "uvc_camera:1.2.3";
        let manifest_content = format!(
            r#"{{
                schema_version: 1,
                manifest: {{ name: "{manifest_name}", tag: "{manifest_tag}" }}
            }}"#
        );

        // Test with git source
        {
            let temp_dir = tempdir().expect("temp dir");
            let remote = create_simple_git_repo(&manifest_content, "1.2.3");

            let launch_file = write_config_str(
                temp_dir.path().join("peppy_launcher.json5"),
                &r#"{
                    deployments: [
                        {
                            name: "uvc_camera",
                            source: { repo: "$GIT_REPO" },
                            tag: "1.2.3",
                            instances: [{ instance_id: "uvc_camera_1", parameters: {} }]
                        }
                    ]
                }"#
                .replace("$GIT_REPO", &remote.path().to_string_lossy()),
            );

            let plan = LaunchPlan::from_launch_file(master_node_config(), &launch_file, None)
                .expect("plan");
            assert_eq!(plan.node_stack().len(), 1, "only master should be present");
            assert_deployment_not_resolvable(
                &plan,
                "uvc_camera",
                expected_identifier,
                expected_reason_substring,
            );
        }

        // Test with http source
        {
            let temp_dir = tempdir().expect("temp dir");
            let bundle_dir = tempdir().expect("bundle dir");
            let server = Server::run();

            let bundle_bytes =
                create_http_bundle(bundle_dir.path(), "uvc_camera.tar.zst", &manifest_content);
            server.expect(
                Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
                    .respond_with(status_code(200).body(bundle_bytes)),
            );

            let url = server.url("/bundles/uvc_camera.tar.zst");
            let launch_file = write_config_str(
                temp_dir.path().join("peppy_launcher.json5"),
                &r#"{
                    deployments: [
                        {
                            name: "uvc_camera",
                            tag: "1.2.3",
                            source: { bundle_url: "$URL" },
                            instances: [{ instance_id: "uvc_camera_1", parameters: {} }]
                        }
                    ]
                }"#
                .replace("$URL", &url.to_string()),
            );

            let plan = LaunchPlan::from_launch_file(master_node_config(), &launch_file, None)
                .expect("plan");
            assert_eq!(plan.node_stack().len(), 1, "only master should be present");
            assert_deployment_not_resolvable(
                &plan,
                "uvc_camera",
                expected_identifier,
                expected_reason_substring,
            );
        }
    }

    // Name mismatch: manifest has wrong name
    assert_mismatch("uvc_camera_wrong", "1.2.3", "node name");

    // Tag mismatch: manifest has wrong tag
    assert_mismatch("uvc_camera", "9.9.9", "tag");
}

/// Uses the example where lidar parameters reference fields unsupported by the
/// node manifest. The deployment should surface a `WrongInputParameters` error.
#[test]
fn deployment_with_invalid_parameters_is_unresolved() {
    let temp_dir = tempdir().expect("temp dir");
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        r#"{
          deployments: [
            {
              name: "lidar_sensor",
              tag: "0.1.0",
              source: "file://./lidar_sensor",
              instances: [
                {
                  instance_id: "lidar_1",
                  parameters: {
                    device: {
                      physical: "/dev/lidar1",
                      sim: "mujoco:lidar1",
                      priority: "sim"
                    },
                  lidar_point: {
                    fps: 30
                  }
                }
              }
            ]
          }
        ]
    }"#,
    );

    let lidar_node: NodeConfig = serde_json5::from_str(
        r#"{
        schema_version: 1,
        manifest: {
          name: "lidar_sensor",
          tag: "0.1.0"
        },
        parameters: {
          device: {
            physical: "string",
            sim: "string",
            priority: "string"
          },
          lidar_point: {
            x: "f32",
            y: "f32",
            z: "f32",
            intensity: "f32",
            return_type: "u8",
            classification: "u8",
            timestamp: "time"
          }
        }
    }"#,
    )
    .expect("valid lidar node config");

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&lidar_node, None, true)
        .expect("lidar node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");

    assert_eq!(plan.node_stack().len(), 1, "only master should be present");

    let lidar = plan
        .report()
        .find_deployment_by_name("lidar_sensor")
        .expect("lidar planned");
    assert!(!lidar.is_resolved(), "lidar should be unresolved");

    let error = lidar.error().expect("lidar should carry an error");
    let Error::WrongInputParameters {
        deployment,
        expected,
        unexpected,
    } = error
    else {
        panic!("unexpected error variant: {error:?}");
    };

    assert_eq!(deployment, "lidar_sensor:0.1.0");

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
}

/// Uses a scenario where a deployment instance provides a parameter at a valid path
/// but with an incorrect type. The deployment should surface a `WrongParameterType` error.
#[test]
fn deployment_parameters_with_invalid_type_is_unresolved() {
    let temp_dir = tempdir().expect("temp dir");
    let launch_file = write_config_str(
        temp_dir.path().join("peppy_launcher.json5"),
        r#"{
            deployments: [
                {
                    name: "sensor",
                    tag: "1.0.0",
                    source: "file://./sensor",
                    instances: [
                        {
                            instance_id: "sensor_1",
                            parameters: {
                                enabled: "yes",
                                sample_rate: 100
                            }
                        }
                    ]
                }
            ]
        }"#,
    );

    // Node manifest declares `enabled` as bool and `sample_rate` as f32
    let sensor_node: NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: {
                name: "sensor",
                tag: "1.0.0"
            },
            parameters: {
                enabled: "bool",
                sample_rate: "f32"
            }
        }"#,
    )
    .expect("valid sensor node config");

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&sensor_node, None, true)
        .expect("sensor node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");

    assert_eq!(plan.node_stack().len(), 1, "only master should be present");

    let sensor = plan
        .report()
        .find_deployment_by_name("sensor")
        .expect("sensor planned");
    assert!(!sensor.is_resolved(), "sensor should be unresolved");

    let error = sensor.error().expect("sensor should carry an error");
    let Error::WrongParameterType {
        deployment,
        path,
        expected,
        actual,
    } = error
    else {
        panic!("unexpected error variant: {error:?}");
    };

    assert_eq!(deployment, "sensor:1.0.0");
    assert_eq!(path, "enabled");
    assert_eq!(expected, "bool");
    assert_eq!(actual, "string");
}

/// Verifies that instance IDs explicitly defined in deployment configurations
/// are preserved in the resulting node stack, rather than being auto-generated.
#[test]
fn build_node_stack_uses_deployment_instance_ids() {
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
                    instances: [
                        { instance_id: "alpha_1", parameters: {} },
                        { instance_id: "alpha_2", parameters: {} }
                    ]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[]);
    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();

    assert_eq!(
        stack.len(),
        2,
        "node stack should contain the master node + alpha"
    );

    let alpha = stack
        .find("alpha", "1.0.0")
        .expect("alpha should be in the stack");

    let instance_ids: BTreeSet<_> = alpha
        .instances()
        .iter()
        .map(|instance| instance.instance_id().as_str().to_owned())
        .collect();

    assert_eq!(
        instance_ids,
        BTreeSet::from(["alpha_1".to_string(), "alpha_2".to_string()]),
        "alpha instance IDs should match the deployment"
    );
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

    let launch_content = r#"{
  deployments: [
    {
      name: "$UVC",
      source: {
        repo: "$REPO",
        path: "$UVC_REMOTE"
      },
      tag: "0.1.0",
      instances: [
        {
          instance_id: "camera_right",
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
        }
      ]
    },
    {
      name: "$WEB",
      source: {
        repo: "$REPO",
        path: "$WEB_REMOTE"
      },
      optional: true,
      tag: "9.9.9",
      instances: [
        {
          instance_id: "video_stream1",
          parameters: {
            http: {
              host: "0.0.0.0",
              port: 8083,
              cors_enabled: false,
              cors_origins: "*",
              max_connections: "2000",
              request_timeout_ms: "3000",
            },
            video_stream: {
              format: "mjpeg",
              quality: 3,
              max_fps: 30,
            },
          }
        }
      ]
    }
  ],
  logging: {
    min_level: "info",
    format: "text"
  }
}"#
    .replace("$UVC", test_helpers::UVC_CAMERA_NODE_NAME)
    .replace("$REPO", &git_repo_path)
    .replace("$UVC_REMOTE", &uvc_remote)
    .replace("$WEB", test_helpers::WEB_VIDEO_STREAM_NODE_NAME)
    .replace("$WEB_REMOTE", &web_remote);

    let launch_file = root.join("peppy_launcher.json5");
    std::fs::write(&launch_file, launch_content).expect("failed to write launch config");

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(
        stack.len(),
        2,
        "stack should contain master + required deployment"
    );
    assert!(stack.contains(test_helpers::UVC_CAMERA_NODE_NAME, "0.1.0"));
    assert!(
        !stack.contains(test_helpers::WEB_VIDEO_STREAM_NODE_NAME, "9.9.9"),
        "unresolvable optional deployment should not be inserted"
    );
    assert!(
        report.dependency_errors().is_empty(),
        "expected no dependency errors, got: {:?}",
        report.dependency_errors()
    );

    let required = report
        .find_deployment_by_name(test_helpers::UVC_CAMERA_NODE_NAME)
        .expect("required deployment planned");
    assert!(required.is_resolved(), "required deployment must resolve");

    let optional = report
        .find_deployment_by_name(test_helpers::WEB_VIDEO_STREAM_NODE_NAME)
        .expect("optional deployment planned");
    assert!(
        !optional.is_resolved(),
        "optional deployment should be unresolved"
    );

    let present_names: BTreeSet<String> = stack
        .snapshot()
        .into_iter()
        .map(|entity| entity.config().manifest.name.as_str().to_owned())
        .collect();
    assert!(
        !present_names.contains(test_helpers::WEB_VIDEO_STREAM_NODE_NAME),
        "optional deployment should not appear in the stack when it fails to resolve"
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
fn optional_dependency_from_launcher_missing_is_unresolved() {
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "1.0.0"));

    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .find_deployment_by_name("beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    assert!(matches!(
        beta.error().expect("beta error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    assert!(
        !report.dependency_errors().is_empty(),
        "expected dependency errors but got none"
    );
    assert!(
        report.dependency_errors().iter().any(|error| matches!(
            error,
            Error::MissingDependency { dependant, dependency, .. }
                if dependant == "alpha" && dependency == "beta"
        )),
        "expected alpha -> beta missing dependency error, got: {:?}",
        report.dependency_errors()
    );
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");
    input_stack
        .push_config(&beta_node, None, true)
        .expect("beta v1 inserted (but deployment expects v2)");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "2.0.0"));

    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .find_deployment_by_name("beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    assert!(matches!(
        beta.error().expect("beta error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    assert!(
        !report.dependency_errors().is_empty(),
        "expected dependency errors but got none"
    );
    assert!(
        report.dependency_errors().iter().any(|error| matches!(
            error,
            Error::MissingDependency { dependant, dependency, dependency_tag, .. }
                if dependant == "alpha" && dependency == "beta" && dependency_tag == "2.0.0"
        )),
        "expected alpha -> beta:2.0.0 missing dependency error, got: {:?}",
        report.dependency_errors()
    );
}

#[test]
fn dependant_resolves_when_optional_dependency_resolves() {
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");
    input_stack
        .push_config(&beta_node, None, true)
        .expect("beta node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(stack.contains("beta", "1.0.0"));
    assert!(
        report.dependency_errors().is_empty(),
        "expected no dependency errors, got: {:?}",
        report.dependency_errors()
    );

    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved());

    let beta = report
        .find_deployment_by_name("beta")
        .expect("beta planned");
    assert!(beta.is_resolved());

    let deps = stack.dependencies_of("alpha", "1.0.0");
    assert!(
        deps.iter()
            .any(|entity| entity.config().manifest.name.as_str() == "beta"),
        "alpha should depend on beta in the stack"
    );
}

#[test]
fn dependant_errors_when_optional_dependency_unresolved() {
    // alpha: non-optional, no dependencies
    // beta: optional, cannot resolve (no instances)
    // gamma: non-optional, depends on beta
    //
    // Expected: gamma cannot resolve because beta (its dependency) is unresolved.
    // Even though beta is optional, it must have at least one instance to be valid.
    let temp_dir = tempdir().expect("temp dir");

    let alpha_source = format!("file://{}/alpha", temp_dir.path().display());
    let beta_source = format!("file://{}/beta", temp_dir.path().display());
    let gamma_source = format!("file://{}/gamma", temp_dir.path().display());
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
                    instances: []
                },
                {
                    name: "gamma",
                    tag: "1.0.0",
                    source: "$GAMMA_SOURCE",
                    instances: [{ instance_id: "gamma_1" }]
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source)
        .replace("$GAMMA_SOURCE", &gamma_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[]);
    let gamma_node = node_config("gamma", "1.0.0", &[("beta", "1.0.0")]);

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");
    input_stack
        .push_config(&gamma_node, None, true)
        .expect("gamma node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "1.0.0"));
    assert!(stack.contains("gamma", "1.0.0"));

    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .find_deployment_by_name("beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    let Error::DeploymentNotResolvable(_, reason) = beta.error().expect("beta error") else {
        panic!("expected DeploymentNotResolvable for beta");
    };
    assert!(
        reason.contains("at least one instance"),
        "unexpected beta reason: {reason}"
    );

    let gamma = report
        .find_deployment_by_name("gamma")
        .expect("gamma planned");
    assert!(gamma.is_resolved(), "gamma node config should resolve");

    assert!(
        !report.dependency_errors().is_empty(),
        "expected dependency errors but got none"
    );
    assert!(
        report.dependency_errors().iter().any(|error| matches!(
            error,
            Error::MissingDependency { dependant, dependency, .. }
                if dependant == "gamma" && dependency == "beta"
        )),
        "expected gamma -> beta missing dependency error, got: {:?}",
        report.dependency_errors()
    );
}

/// Tests that an optional deployment becomes required when a non-optional deployment depends on it,
/// and verifies that unresolved deployments remain visible in the plan report.
///
/// Scenario:
/// - "alpha" is declared as `optional: true` in the launch file
/// - "beta" is non-optional and depends on "alpha"
/// - "gamma" is non-optional but has no matching node config (version 3.0.0 doesn't exist)
/// - "alpha" cannot be resolved (no node config provided to the resolver)
///
/// Expected behavior:
/// - The plan report should include all three deployments (alpha and gamma unresolved, beta resolved)
/// - The node stack should only contain resolved deployments (master + beta)
/// - The report should include a `MissingDependency` error for beta -> alpha
///
/// This ensures that the optionality of a deployment is overridden when another
/// non-optional deployment has a hard dependency on it.
#[test]
fn required_optional_dependency_surfaces_error() {
    let temp_dir = tempdir().expect("temp dir");

    // Alpha cannot be optional here since beta depends on it and it itself non-optional
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&beta_node, None, true)
        .expect("beta node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(report.deployments().len(), 3, "three deployments planned");

    // Verify unresolved deployments remain visible in the report
    let unresolved_names: BTreeSet<_> = report
        .deployments()
        .iter()
        .filter(|deployment| !deployment.is_resolved())
        .map(|deployment| deployment.deployment().name.as_str().to_owned())
        .collect();
    assert_eq!(
        unresolved_names,
        BTreeSet::from(["alpha".to_string(), "gamma".to_string()])
    );

    // Verify alpha is unresolved with expected error
    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(!alpha.is_resolved());
    assert!(matches!(
        alpha.error().expect("alpha error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    // Verify gamma is unresolved with expected error
    let gamma = report
        .find_deployment_by_name("gamma")
        .expect("gamma planned");
    assert!(matches!(
        gamma.error().expect("gamma error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    // Verify beta resolves successfully
    let beta = report
        .find_deployment_by_name("beta")
        .expect("beta planned");
    assert!(beta.is_resolved(), "beta node config should resolve");

    // Verify node stack only contains resolved deployments
    assert!(stack.contains("beta", "2.0.0"));
    assert!(!stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("gamma", "3.0.0"));

    // Verify dependency error is reported
    assert!(
        !report.dependency_errors().is_empty(),
        "expected dependency errors but got none"
    );
    assert!(
        report.dependency_errors().iter().any(|error| matches!(
            error,
            Error::MissingDependency { dependant, dependency, .. }
                if dependant == "beta" && dependency == "alpha"
        )),
        "expected beta -> alpha missing dependency error, got: {:?}",
        report.dependency_errors()
    );
}

#[test]
fn unlisted_dependency_reports_missing_error() {
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&alpha_node, None, true)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("delta", "1.0.0"));

    let alpha = report
        .find_deployment_by_name("alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    assert!(
        !report.dependency_errors().is_empty(),
        "expected dependency errors but got none"
    );
    assert!(
        report.dependency_errors().iter().any(|error| matches!(
            error,
            Error::MissingDependency { dependant, dependency, dependency_tag, .. }
                if dependant == "alpha" && dependency == "delta" && dependency_tag == "1.0.0"
        )),
        "expected alpha -> delta:1.0.0 missing dependency error, got: {:?}",
        report.dependency_errors()
    );
}

#[test]
fn missing_interface_on_dependency_is_reported() {
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config(&brain_node, None, true)
        .expect("brain node inserted");
    input_stack
        .push_config(&lidar_node, None, true)
        .expect("lidar node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("brain", "1.0.0"));
    assert!(stack.contains("lidar", "1.0.0"));

    let missing_interface = report
        .dependency_errors()
        .iter()
        .find(|error| matches!(error, Error::MissingInterface { dependant, dependency, .. } if dependant == "brain" && dependency == "lidar"))
        .expect("missing interface error should be reported");

    let Error::MissingInterface {
        dependant,
        dependency,
        interface_kind,
        interface_name,
        ..
    } = missing_interface
    else {
        panic!("expected MissingInterface error");
    };

    assert_eq!(dependant, "brain");
    assert_eq!(dependency, "lidar");
    assert_eq!(interface_kind, "topic");
    assert_eq!(interface_name, "lidar_topic");
}
