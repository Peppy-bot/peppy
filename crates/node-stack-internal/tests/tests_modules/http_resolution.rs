use config::peppy_config::{DeploymentNodeSource, HttpRemoteSpec, PeppyLauncher};
use config::test_helpers;
use httptest::{Expectation, Server, matchers::request, responders::status_code};
use node_stack::DeploymentPlanner;
use node_stack::NodeStackError;
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};

use crate::helpers::config_common::{
    create_http_bundle, deployment, master_node_config, write_config,
};

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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

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

    let planner = DeploymentPlanner::from_launch_file(master_node_config(), &launch_file, None)
        .expect("planner");

    let graph = planner.create_deployment_graph();

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
    let NodeStackError::DeploymentNotResolvable(identifier, reason) = error else {
        panic!("unexpected error variant: {error:?}");
    };
    assert_eq!(identifier, "uvc_camera:1.2.3");
    assert!(reason.contains("tag"), "unexpected error reason: {reason}");
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
