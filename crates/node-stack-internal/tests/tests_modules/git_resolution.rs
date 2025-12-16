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
        .find_deployment_by_name("uvc_camera")
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
        .find_deployment_by_name(test_helpers::LIDAR_SENSOR_NODE_NAME)
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
        .find_deployment_by_name("uvc_camera")
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
        &[(config::consts::PEPPY_NODE_CONFIG_FILE, manifest_v2)],
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
        .find_deployment_by_name("uvc_camera")
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
