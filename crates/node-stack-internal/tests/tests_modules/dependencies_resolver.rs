use config::{
    node::NodeConfig,
    peppy_config::{DeploymentNodeSource, GitRemoteSpec, HttpRemoteSpec, PeppyLauncher},
};
use git2::{ObjectType, Repository, Signature};
use httptest::{Expectation, Server, matchers::request, responders::status_code};
use node_stack::NodeStackError as Error;
use node_stack::{LocalNodeStackBuilder, NodeStack};
use std::path::PathBuf;
use tempfile::tempdir;

use crate::helpers::config_common::{
    create_http_bundle, deployment, master_node_config, node_config, write_config,
};
use crate::helpers::git::{create_git_repository, push_git_commit};
use crate::helpers::resolver::StaticResolver;

#[test]
fn http_bundle_is_downloaded_and_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" }
        }"#;
    let bundle_bytes = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );

    let url = server.url("/bundles/uvc_camera.tar.zst");
    let http_spec = HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Http(http_spec)),
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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        1,
        "http deployment should resolve to single node"
    );
    let root = graph.root_index();
    let node_map = graph.get(root).expect("root node map");
    assert!(node_map.is_resolved(), "http deployment should be resolved");
    assert_eq!(node_map.deployment().name, "uvc_camera");
    assert_eq!(node_map.node_source().node().manifest.tag, "1.2.3");
    assert_eq!(
        node_map.node_source().node().manifest.name.as_str(),
        "uvc_camera"
    );
}

#[test]
fn http_bundle_is_downloaded_and_name_not_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera_wrong", tag: "1.2.3" }
        }"#;
    let bundle_bytes = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );

    let url = server.url("/bundles/uvc_camera.tar.zst");
    let http_spec = HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Http(http_spec)),
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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        1,
        "http deployment should be tracked even when unresolved"
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
    match error {
        Error::DeploymentNotResolvable(identifier, reason) => {
            assert_eq!(identifier, "uvc_camera:1.2.3");
            assert!(
                reason.contains("node name"),
                "unexpected error reason: {reason}"
            );
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn http_bundle_is_downloaded_and_tag_not_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "9.9.9" }
        }"#;
    let bundle_bytes = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );

    let url = server.url("/bundles/uvc_camera.tar.zst");
    let http_spec = HttpRemoteSpec::new(url.to_string(), None).expect("valid http deployment spec");

    let deployments = vec![deployment(
        "uvc_camera",
        "1.2.3",
        Some(DeploymentNodeSource::Http(http_spec)),
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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        1,
        "http deployment should be tracked even when unresolved"
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
    match error {
        Error::DeploymentNotResolvable(identifier, reason) => {
            assert_eq!(identifier, "uvc_camera:1.2.3");
            assert!(reason.contains("tag"), "unexpected error reason: {reason}");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn git_repo_is_cloned_and_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3" }
        }"#;
    let remote = create_git_repository(manifest_content, "1.2.3");

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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

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
    let remote = create_git_repository(manifest_content, "1.2.3");

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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

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
    match error {
        Error::DeploymentNotResolvable(identifier, reason) => {
            assert_eq!(identifier, "uvc_camera:1.2.3");
            assert!(
                reason.contains("node name"),
                "unexpected error reason: {reason}"
            );
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn git_repo_is_cloned_and_tag_not_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "9.9.9" }
        }"#;
    let remote = create_git_repository(manifest_content, "1.2.3");

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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

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
    match error {
        Error::DeploymentNotResolvable(identifier, reason) => {
            assert_eq!(identifier, "uvc_camera:1.2.3");
            assert!(reason.contains("tag"), "unexpected error reason: {reason}");
        }
        other => panic!("unexpected error variant: {other:?}"),
    }
}

#[test]
fn git_repo_is_cloned_and_same_tag_updates_code() {
    let temp_dir = tempdir().expect("temp dir");
    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v1"] }
        }"#;
    let remote = create_git_repository(manifest_v1, "1.0.0");

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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();

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

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(NodeStack::new(master_node_config()))
        .expect("planner");

    let graph = planner.map_deployments_to_nodes();
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

#[test]
fn optional_dependency_from_launcher_missing_is_unresolved() {
    let temp_dir = tempdir().expect("temp dir");

    let deployments = vec![
        deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            false,
        ),
        deployment(
            "beta",
            "1.0.0",
            Some(DeploymentNodeSource::Git(GitRemoteSpec {
                repo: "https://example.com/repo.git".to_string(),
                path: None,
            })),
            true,
        ),
    ];

    let launcher_config = PeppyLauncher {
        deployments: Some(deployments.clone()),
        logging: None,
    };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "1.0.0")]);

    let loader_nodes = vec![alpha_node.clone()];
    let resolver = StaticResolver::new(vec![alpha_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        2,
        "missing optional deployment should still surface as unresolved when required",
    );

    let root = graph.root_index();
    let deployment_map = graph.get(root).expect("root node");
    assert_eq!(deployment_map.deployment().name, "alpha");
    assert!(deployment_map.is_resolved());

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

    let deployments = vec![
        deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            false,
        ),
        deployment(
            "beta",
            "2.0.0", // The deployment exists on disk, but the tag does not
            Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
            true,
        ),
    ];

    let config = PeppyLauncher {
        deployments: Some(deployments.clone()),
        logging: None,
    };
    let launch_file = write_config(temp_dir.path().join("peppy_launcher.json5"), config);

    let alpha_node = node_config("alpha", "1.0.0", &[("beta", "2.0.0")]);
    let beta_node = node_config("beta", "1.0.0", &[]);

    let loader_nodes = vec![alpha_node.clone(), beta_node.clone()];
    let resolver = StaticResolver::new(vec![alpha_node, beta_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        2,
        "optional deployment with mismatched tag should surface as unresolved",
    );

    let root = graph.root_index();
    let root_map = graph.get(root).expect("root node");
    assert_eq!(root_map.deployment().name, "alpha");
    assert!(root_map.is_resolved());

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
fn required_optional_dependency_surfaces_error() {
    let temp_dir = tempdir().expect("temp dir");

    let deployments = vec![
        deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            true, // Alpha cannot be optional here since beta depends on it and it itself non-optional
        ),
        deployment(
            "beta",
            "2.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
            false,
        ),
    ];

    let config = PeppyLauncher {
        deployments: Some(deployments.clone()),
        logging: None,
    };
    let launch_file = write_config(temp_dir.path().join("peppy_launcher.json5"), config);

    let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

    let loader_nodes = vec![beta_node.clone()];
    let resolver = StaticResolver::new(vec![beta_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();

    assert_eq!(
        graph.len(),
        2,
        "optional dependency should surface as unresolved when required by a non-optional deployment",
    );

    let root = graph.root_index();
    let root_map = graph.get(root).expect("root node");
    assert_eq!(root_map.deployment().name, "beta");
    assert!(root_map.is_resolved());

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

    let deployments = vec![
        deployment(
            "alpha",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
            true, // Alpha cannot be optional here since beta depends on it and is itself non-optional
        ),
        deployment(
            "beta",
            "2.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
            false,
        ),
        deployment(
            "gamma",
            "3.0.0", // This version does not exist
            Some(DeploymentNodeSource::Local(PathBuf::from("./beta"))),
            false,
        ),
    ];

    let config = PeppyLauncher {
        deployments: Some(deployments.clone()),
        logging: None,
    };
    let launch_file = write_config(temp_dir.path().join("peppy_launcher.json5"), config);

    let beta_node = node_config("beta", "2.0.0", &[("alpha", "1.0.0")]);

    let loader_nodes = vec![beta_node.clone()];
    let resolver = StaticResolver::new(vec![beta_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();

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
        2,
        "only two deployments should contain errors"
    );

    assert_eq!(
        unresolved_names,
        vec!["alpha".to_string(), "gamma".to_string()]
    );

    let unresolved_errors: Vec<_> = unresolved
        .iter()
        .map(|map| {
            map.error()
                .expect("unresolved deployment should carry error")
        })
        .collect();

    assert!(
        unresolved_errors
            .iter()
            .all(|error| matches!(error, Error::DeploymentNotResolvable(_, _))),
        "unexpected unresolved deployment error kind",
    );

    let beta_map = graph
        .indices()
        .into_iter()
        .filter_map(|idx| graph.get(idx))
        .find(|map| map.deployment().name == "beta")
        .expect("beta deployment should be present");

    assert!(beta_map.is_resolved());
}

#[test]
fn missing_dependency_becomes_unresolved_node() {
    let temp_dir = tempdir().expect("temp dir");

    let deployments = vec![deployment(
        "alpha",
        "1.0.0",
        Some(DeploymentNodeSource::Local(PathBuf::from("./alpha"))),
        false,
    )];

    let config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    let launch_file = write_config(temp_dir.path().join("peppy_launcher.json5"), config);

    let alpha_node = node_config("alpha", "1.0.0", &[("delta", "1.0.0")]);
    let loader_nodes = vec![alpha_node.clone()];
    let resolver = StaticResolver::new(vec![alpha_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();

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

    let deployments = vec![
        deployment(
            "brain",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./brain"))),
            false,
        ),
        deployment(
            "lidar",
            "1.0.0",
            Some(DeploymentNodeSource::Local(PathBuf::from("./lidar"))),
            false,
        ),
    ];

    let config = PeppyLauncher {
        deployments: Some(deployments.clone()),
        logging: None,
    };
    let launch_file = write_config(temp_dir.path().join("peppy_launcher.json5"), config);

    let brain_node = node_config("brain", "1.0.0", &[("lidar", "1.0.0")]);
    let lidar_node: NodeConfig = serde_json5::from_str(
        r#"{
            schema_version: 1,
            manifest: { name: "lidar", tag: "1.0.0" }
        }"#,
    )
    .expect("valid lidar node without exposes");

    let loader_nodes = vec![brain_node.clone(), lidar_node.clone()];
    let resolver = StaticResolver::new(vec![brain_node, lidar_node]);

    let builder = LocalNodeStackBuilder::from_launch_file(&launch_file, None).expect("builder");
    let planner = builder
        .build_with_nodes(
            NodeStack::from_configs(loader_nodes).expect("node stack from loader nodes"),
        )
        .expect("planner")
        .with_resolver(resolver);

    let graph = planner.map_deployments_to_nodes();
    assert_eq!(
        graph.len(),
        2,
        "both deployments should remain in the graph"
    );

    let root = graph.root_index();
    let brain_map = graph.get(root).expect("brain deployment present");
    assert_eq!(brain_map.deployment().name, "brain");
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
