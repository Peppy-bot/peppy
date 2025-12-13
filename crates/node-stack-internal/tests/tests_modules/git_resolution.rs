use std::collections::BTreeSet;
use std::fs;

use config::peppy_config::{DeploymentNodeSource, GitRemoteSpec, PeppyLauncher};
use config::test_helpers;
use git2::{ObjectType, Repository, Signature};
use node_stack::{LaunchPlan, NodeStackError};
use tempfile::{TempDir, tempdir};

use crate::helpers::config_common::{
    deployment, master_node_config, write_config, write_config_str,
};
use crate::helpers::git::{create_simple_git_repo, push_git_commit};

#[test]
fn git_repo_is_cloned_and_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" }
        }"#;
    let remote = create_simple_git_repo(manifest_content, "1.2.3");

    let spec = GitRemoteSpec {
        repo: remote.path().to_string_lossy().to_string(),
        path: None,
    };

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Git(spec)),
        false,
    )];

    let launcher_config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 2, "master + uvc_camera");
    assert!(stack.contains("uvc_camera", "1.2.3"));

    let deployment = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(deployment.is_resolved());

    let node = deployment.node().expect("resolved node config");
    assert_eq!(node.manifest.tag, "1.2.3");
    assert_eq!(node.manifest.name.as_str(), "uvc_camera");
}

/// The lidar sensor requests tag `v2.0` but the repository is only tagged `0.1.0` (and `v1.0`).
/// The deployment must remain unresolved when the requested tag is missing.
#[test]
fn git_repo_missing_tag_is_unresolvable() {
    let git_repo_dir = TempDir::new().expect("git repo temp dir");
    let git_repo_path = test_helpers::create_nodes_git_repo(git_repo_dir.path());
    let git_repo_path = git_repo_path.to_str().expect("utf-8 git repo path");

    let project_dir = TempDir::new().expect("project temp dir");
    let project_root = project_dir.path();

    let lidar_repo_path = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let launcher_content = r#"{
      deployments: [
        {
          name: "$LIDAR_NODE_NAME",
          source: {
            repo: "$GIT_REPO",
            path: "$LIDAR_REPO_PATH"
          },
          // The test repo is tagged `0.1.0` (and also `v1.0`); request `v2.0` to exercise the missing-tag path.
          tag: "v2.0",
          instances: [
            { instance_id: "lidar_1", parameters: {} }
          ]
        },
      ],
    }"#
    .replace("$LIDAR_NODE_NAME", test_helpers::LIDAR_SENSOR_NODE_NAME)
    .replace("$GIT_REPO", git_repo_path)
    .replace("$LIDAR_REPO_PATH", &lidar_repo_path);

    let launch_file =
        write_config_str(project_root.join("peppy_launcher.json5"), &launcher_content);

    let plan = LaunchPlan::from_launch_file(master_node_config(), launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(
        stack.len(),
        1,
        "node stack should contain only the master node"
    );

    let lidar_deployment = report
        .deployments()
        .iter()
        .find(|deployment| {
            deployment.deployment().name.as_str() == test_helpers::LIDAR_SENSOR_NODE_NAME
        })
        .expect("lidar planned");

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

    let nodes_cache_dir = project_root.join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        nodes_cache_dir
    );
}

#[test]
fn git_repo_is_cloned_and_name_not_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera_wrong", tag: "1.2.3" }
        }"#;
    let remote = create_simple_git_repo(manifest_content, "1.2.3");

    let spec = GitRemoteSpec {
        repo: remote.path().to_string_lossy().to_string(),
        path: None,
    };

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Git(spec)),
        false,
    )];

    let launcher_config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 1, "only master should be present");

    let deployment = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        !deployment.is_resolved(),
        "manifest name mismatch should fail resolution"
    );
    let error = deployment
        .error()
        .expect("unresolved deployment should carry error");
    let NodeStackError::DeploymentNotResolvable(identifier, reason) = error else {
        panic!("unexpected error variant: {error:?}");
    };
    assert_eq!(identifier, "uvc_camera:1.2.3");
    assert!(
        reason.contains("node name"),
        "unexpected error reason: {reason}"
    );
}

#[test]
fn git_repo_is_cloned_and_tag_not_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "9.9.9" }
        }"#;
    let remote = create_simple_git_repo(manifest_content, "1.2.3");

    let spec = GitRemoteSpec {
        repo: remote.path().to_string_lossy().to_string(),
        path: None,
    };

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Git(spec)),
        false,
    )];

    let launcher_config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 1, "only master should be present");

    let deployment = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        !deployment.is_resolved(),
        "manifest tag mismatch should fail resolution"
    );
    let error = deployment
        .error()
        .expect("unresolved deployment should carry error");
    let NodeStackError::DeploymentNotResolvable(identifier, reason) = error else {
        panic!("unexpected error variant: {error:?}");
    };
    assert_eq!(identifier, "uvc_camera:1.2.3");
    assert!(reason.contains("tag"), "unexpected error reason: {reason}");
}

#[test]
fn git_repo_is_cloned_and_same_tag_updates_code() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v1"] }
        }"#;
    let remote = create_simple_git_repo(manifest_v1, "1.0.0");

    let spec = GitRemoteSpec {
        repo: remote.path().to_string_lossy().to_string(),
        path: None,
    };

    let deployments = vec![deployment(
        "uvc_camera",
        "1.0.0",
        Some(DeploymentNodeSource::Git(spec.clone())),
        false,
    )];

    let launcher_config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
    let deployment = plan
        .report()
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(deployment.is_resolved(), "git deployment should resolve");
    let launch_cmd_v1 = deployment
        .node()
        .expect("resolved node config")
        .manifest
        .launch_cmd
        .clone()
        .expect("launch command present");
    assert_eq!(launch_cmd_v1, vec!["run_v1".to_string()]);

    // Update the remote repository keeping the same tag but new contents.
    let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v2"] }
        }"#;

    let commit_id = push_git_commit(
        remote.path(),
        &[("peppy.json5", manifest_v2)],
        "update manifest",
    );

    let repo = Repository::open(remote.path()).expect("open remote repo");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");

    let commit = repo
        .find_object(commit_id, Some(ObjectType::Commit))
        .expect("find updated commit");
    repo.tag("1.0.0", &commit, &signature, "tag", true)
        .expect("retag commit");

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
    let deployment = plan
        .report()
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        deployment.is_resolved(),
        "git deployment should still resolve"
    );
    let launch_cmd_v2 = deployment
        .node()
        .expect("resolved node config after update")
        .manifest
        .launch_cmd
        .clone()
        .expect("launch command present after update");

    assert_eq!(launch_cmd_v2, vec!["run_v2".to_string()]);
    assert_ne!(launch_cmd_v1, launch_cmd_v2);
}

/// Uses the example where lidar parameters reference fields unsupported by the
/// node manifest. The deployment should surface a `WrongInputParameters` error.
#[test]
fn git_repo_invalid_parameters_rejected() {
    let git_repo_temp_dir = TempDir::new().expect("git repo temp dir");
    let repo = Repository::init(git_repo_temp_dir.path()).expect("init git repo");

    let peppy_json_content = r#"{
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
          timestamp: "time",
        }
      }
    }"#;

    let lidar_remote_path = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let manifest_path = git_repo_temp_dir
        .path()
        .join(&lidar_remote_path)
        .join("peppy.json5");
    fs::create_dir_all(manifest_path.parent().expect("manifest parent"))
        .expect("create manifest directories");
    fs::write(&manifest_path, peppy_json_content).expect("write manifest");

    let rel_manifest_path = manifest_path
        .strip_prefix(git_repo_temp_dir.path())
        .expect("relative manifest path");
    let mut index = repo.index().expect("repo index");
    index
        .add_path(rel_manifest_path)
        .expect("add manifest to index");
    index.write().expect("write index");

    let tree_id = index.write_tree().expect("write tree");
    let tree = repo.find_tree(tree_id).expect("find tree");
    let signature = Signature::now("Peppy", "peppy@example.com").expect("signature");
    let commit_id = repo
        .commit(
            Some("HEAD"),
            &signature,
            &signature,
            "initial commit",
            &tree,
            &[],
        )
        .expect("create commit");
    let commit = repo
        .find_object(commit_id, Some(ObjectType::Commit))
        .expect("find commit object");
    repo.tag("0.1.0", &commit, &signature, "tag", false)
        .expect("create tag");

    let root_temp_dir = TempDir::new().expect("project temp dir");
    let root = root_temp_dir.path();

    let git_repo_path = git_repo_temp_dir.path().to_string_lossy().to_string();
    let launcher_content = r#"{
      deployments: [
        {
          name: "$LIDAR_NODE_NAME",
          source: {
            repo: "$GIT_REPO",
            path: "$LIDAR_REPO_PATH"
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
                  fps: 30
                }
              }
            }
          ]
        },
      ],
    }"#
    .replace("$LIDAR_NODE_NAME", test_helpers::LIDAR_SENSOR_NODE_NAME)
    .replace("$GIT_REPO", &git_repo_path)
    .replace("$LIDAR_REPO_PATH", &lidar_remote_path);
    let launch_file = write_config_str(
        root_temp_dir.path().join("peppy_launcher.json5"),
        &launcher_content,
    );

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 1, "config should only have the master node");

    let lidar_deployment = report
        .deployments()
        .iter()
        .find(|deployment| {
            deployment.deployment().name.as_str() == test_helpers::LIDAR_SENSOR_NODE_NAME
        })
        .expect("lidar deployment should be planned");

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

#[test]
fn git_repo_correct_parameters_with_invalid_type_rejected() {
    todo!(
        "A bit similar to `git_repo_invalid_parameters_rejected` but the `fps` parameter is of type `string` instead of `i32`"
    )
}
