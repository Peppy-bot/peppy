use std::collections::{BTreeMap, BTreeSet};

use crate::helpers::config_common::master_node_config;
use crate::helpers::config_common::{node_config, write_config_str};
use config::node::NodeConfig;
use config::test_helpers;
use node_stack::NodeStackError as Error;
use node_stack::{LaunchPlan, NodeStack};
use tempfile::TempDir;
use tempfile::tempdir;

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
fn deployment_with_zero_instances_is_unresolved() {
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
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
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
        .push_config_allow_missing(&alpha_node, None)
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
        .push_config_allow_missing(&alpha_node, None)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "1.0.0"));

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    assert!(matches!(
        beta.error().expect("beta error"),
        Error::DeploymentNotResolvable(_, _)
    ));

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
        .push_config_allow_missing(&alpha_node, None)
        .expect("alpha node inserted");
    input_stack
        .push_config_allow_missing(&beta_node, None)
        .expect("beta v1 inserted (but deployment expects v2)");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "2.0.0"));

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    assert!(matches!(
        beta.error().expect("beta error"),
        Error::DeploymentNotResolvable(_, _)
    ));

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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&alpha_node, None)
        .expect("alpha node inserted");
    input_stack
        .push_config_allow_missing(&beta_node, None)
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
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved());

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
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
fn optional_dependency_unresolved_causes_dependant_error() {
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
                    instances: []
                }
            ]
        }"#
        .replace("$ALPHA_SOURCE", &alpha_source)
        .replace("$BETA_SOURCE", &beta_source),
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&alpha_node, None)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("beta", "1.0.0"));

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
        .expect("beta planned");
    assert!(!beta.is_resolved(), "beta should be unresolved");
    let Error::DeploymentNotResolvable(_, reason) = beta.error().expect("beta error") else {
        panic!("expected DeploymentNotResolvable for beta");
    };
    assert!(
        reason.contains("at least one instance"),
        "unexpected beta reason: {reason}"
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
        .deployments()
        .iter()
        .find(|deployment| {
            deployment.deployment().name.as_str() == test_helpers::UVC_CAMERA_NODE_NAME
        })
        .expect("required deployment planned");
    assert!(required.is_resolved(), "required deployment must resolve");

    let optional = report
        .deployments()
        .iter()
        .find(|deployment| {
            deployment.deployment().name.as_str() == test_helpers::WEB_VIDEO_STREAM_NODE_NAME
        })
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

/// Tests that an optional deployment becomes required when a non-optional deployment depends on it.
///
/// Scenario:
/// - "alpha" is declared as `optional: true` in the launch file
/// - "beta" is non-optional and depends on "alpha"
/// - "alpha" cannot be resolved (no node config provided to the resolver)
///
/// Expected behavior:
/// - The plan report should include both deployments (alpha unresolved, beta resolved)
/// - The node stack should only contain resolved deployments
/// - The report should include a `MissingDependency` error for beta -> alpha
///
/// This ensures that the optionality of a deployment is overridden when another
/// non-optional deployment has a hard dependency on it.
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&beta_node, None)
        .expect("beta node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("beta", "2.0.0"));
    assert!(!stack.contains("alpha", "1.0.0"));

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(!alpha.is_resolved());
    assert!(matches!(
        alpha.error().expect("alpha error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
        .expect("beta planned");
    assert!(beta.is_resolved());

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

// TODO: Double check, unresolved OPTIONAL `deployments` should be fine, unresolved `NodeEntity` in a node stack is not
/// Verifies that unresolved deployments remain visible in the plan report.
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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&beta_node, None)
        .expect("beta node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(report.deployments().len(), 3, "three deployments planned");

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

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(matches!(
        alpha.error().expect("alpha error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    let gamma = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "gamma")
        .expect("gamma planned");
    assert!(matches!(
        gamma.error().expect("gamma error"),
        Error::DeploymentNotResolvable(_, _)
    ));

    let beta = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "beta")
        .expect("beta planned");
    assert!(beta.is_resolved(), "beta node config should resolve");

    assert!(stack.contains("beta", "2.0.0"));
    assert!(!stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("gamma", "3.0.0"));

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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&alpha_node, None)
        .expect("alpha node inserted");

    let plan = LaunchPlan::with_nodes(&launch_file, None, input_stack).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert!(stack.contains("alpha", "1.0.0"));
    assert!(!stack.contains("delta", "1.0.0"));

    let alpha = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "alpha")
        .expect("alpha planned");
    assert!(alpha.is_resolved(), "alpha node config should resolve");

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

    let input_stack = NodeStack::new(master_node_config(), None);
    input_stack
        .push_config_allow_missing(&brain_node, None)
        .expect("brain node inserted");
    input_stack
        .push_config_allow_missing(&lidar_node, None)
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
