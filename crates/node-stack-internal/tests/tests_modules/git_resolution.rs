use std::collections::BTreeSet;

use config::peppy_config::{DeploymentNodeSource, GitRemoteSpec, PeppyLauncher};
use config::test_helpers;
use git2::{ObjectType, Repository, Signature};
use node_stack::DeploymentPlanner;
use node_stack::NodeStackError;
use tempfile::{TempDir, tempdir};

use crate::helpers::config_common::{deployment, master_node_config, write_config};
use crate::helpers::git::{create_simple_git_repo, push_git_commit};

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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        1,
        "git deployment should resolve to single node"
    );

    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(node_map.is_resolved(), "git deployment should be resolved");
    assert_eq!(node_map.deployment().name, "uvc_camera");
    assert_eq!(node_map.node_source().node().manifest.tag, "1.2.3");
    assert_eq!(
        node_map.node_source().node().manifest.name.as_str(),
        "uvc_camera"
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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        1,
        "git deployment should be tracked even when unresolved"
    );
    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(
        !node_map.is_resolved(),
        "manifest name mismatch should fail resolution"
    );
    let error = node_map
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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        1,
        "git deployment should be tracked even when unresolved"
    );
    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(
        !node_map.is_resolved(),
        "manifest tag mismatch should fail resolution"
    );
    let error = node_map
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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

    assert_eq!(
        graph.len(),
        1,
        "git deployment should resolve to single node on first fetch"
    );

    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(node_map.is_resolved(), "git deployment should resolve");
    let launch_cmd_v1 = node_map
        .node_source()
        .node()
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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();
    assert_eq!(
        graph.len(),
        1,
        "git deployment should resolve to single node on subsequent fetch"
    );
    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(
        node_map.is_resolved(),
        "git deployment should still resolve"
    );
    let launch_cmd_v2 = node_map
        .node_source()
        .node()
        .manifest
        .launch_cmd
        .clone()
        .expect("launch command present after update");

    assert_eq!(launch_cmd_v2, vec!["run_v2".to_string()]);
    assert_ne!(launch_cmd_v1, launch_cmd_v2);
}
