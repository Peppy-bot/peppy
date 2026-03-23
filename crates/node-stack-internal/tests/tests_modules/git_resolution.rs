use config::peppy_config::{DeploymentGitSource, DeploymentSource, PeppyLauncher};
use config::test_helpers;
use git2::{ObjectType, Repository, Signature};
use node_stack::{LaunchPlan, NodeStackError};
use tempfile::{TempDir, tempdir};

use crate::helpers::config_common::{
    core_node_config, deployment, init_test_data_dir, write_config, write_config_str,
};
use crate::helpers::git::{create_simple_git_repo, push_git_commit};

#[test]
fn git_repo_is_cloned_and_resolved() {
    let (_data_dir, peppy_dirs) = init_test_data_dir();
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" },

            codegen: { language: "rust" },
            process: { start_cmd: ["uvc_camera"] }
        }"#;
    let remote = create_simple_git_repo(manifest_content, "1.2.3");

    let source = DeploymentSource::Git(DeploymentGitSource {
        repo: remote.path().to_string_lossy().to_string(),
        path: ".".to_string(),
        ref_: "1.2.3".to_string(),
    });
    let deployments = vec![deployment(source)];

    let launcher_config = PeppyLauncher { deployments };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(core_node_config(), &launch_file, &peppy_dirs).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 2, "core node + uvc_camera");
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
    let (_data_dir, peppy_dirs) = init_test_data_dir();
    let git_repo_dir = TempDir::new().expect("git repo temp dir");
    let git_repo_path = test_helpers::create_nodes_git_repo(git_repo_dir.path());
    let git_repo_path = git_repo_path.to_str().expect("utf-8 git repo path");

    let project_dir = TempDir::new().expect("project temp dir");
    let project_root = project_dir.path();

    let lidar_repo_path = format!("nodes/{}", test_helpers::LIDAR_SENSOR_NODE_NAME);
    let launcher_content = r#"{
      deployments: [
        {
          source: {
            repo: "$GIT_REPO",
            path: "$LIDAR_REPO_PATH",
            ref: "v2.0"
          },
          instances: [
            { instance_id: "lidar_1", arguments: {} }
          ]
        },
      ],
    }"#
    .replace("$GIT_REPO", git_repo_path)
    .replace("$LIDAR_REPO_PATH", &lidar_repo_path);

    let launch_file =
        write_config_str(project_root.join("peppy_launcher.json5"), &launcher_content);

    let plan =
        LaunchPlan::from_launch_file(core_node_config(), launch_file, &peppy_dirs).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(
        stack.len(),
        1,
        "node stack should contain only the core node"
    );

    let lidar_deployment = report
        .deployments()
        .iter()
        .find(|deployment| !deployment.is_resolved())
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

    let expected = format!("git:{}::{}@v2.0", git_repo_path, lidar_repo_path);
    assert_eq!(deployment, &expected);
    let reason_lower = reason.to_lowercase();
    assert!(
        reason.contains("v2.0") && reason_lower.contains("not found"),
        "expected surfaced git missing-tag reason, got: {}",
        reason
    );

    let added_nodes_dir = peppy_dirs.added_nodes_dir();
    assert!(
        added_nodes_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        added_nodes_dir
    );
}

#[test]
fn git_repo_is_cloned_and_same_tag_updates_code() {
    let (_data_dir, peppy_dirs) = init_test_data_dir();
    let temp_dir = tempdir().expect("temp dir");
    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0" },

            codegen: { language: "rust" },
            process: { start_cmd: ["run_v1"] }
        }"#;
    let remote = create_simple_git_repo(manifest_v1, "1.0.0");

    let source = DeploymentSource::Git(DeploymentGitSource {
        repo: remote.path().to_string_lossy().to_string(),
        path: ".".to_string(),
        ref_: "1.0.0".to_string(),
    });
    let deployments = vec![deployment(source)];

    let launcher_config = PeppyLauncher { deployments };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan =
        LaunchPlan::from_launch_file(core_node_config(), &launch_file, &peppy_dirs).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "core node + uvc_camera");
    let deployment = plan
        .report()
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(deployment.is_resolved(), "git deployment should resolve");
    let start_cmd_v1 = deployment
        .node()
        .expect("resolved node config")
        .process
        .as_ref()
        .unwrap()
        .start_cmd
        .clone();
    assert_eq!(start_cmd_v1, vec!["run_v1".to_string()]);

    // Update the remote repository keeping the same tag but new contents.
    let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0" },

            codegen: { language: "rust" },
            process: { start_cmd: ["run_v2"] }
        }"#;

    let commit_id = push_git_commit(
        remote.path(),
        &[(config::consts::NODE_CONFIG_FILE, manifest_v2)],
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
        LaunchPlan::from_launch_file(core_node_config(), &launch_file, &peppy_dirs).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "core node + uvc_camera");
    let deployment = plan
        .report()
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        deployment.is_resolved(),
        "git deployment should still resolve"
    );
    let start_cmd_v2 = deployment
        .node()
        .expect("resolved node config after update")
        .process
        .as_ref()
        .unwrap()
        .start_cmd
        .clone();

    assert_eq!(start_cmd_v2, vec!["run_v2".to_string()]);
    assert_ne!(start_cmd_v1, start_cmd_v2);
}
