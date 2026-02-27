use config::peppy_config::{DeploymentSource, DeploymentUrlSource, PeppyLauncher};
use httptest::{
    Expectation, Server,
    matchers::request,
    responders::{cycle, status_code},
};
use node_stack::LaunchPlan;
use tempfile::tempdir;

use crate::helpers::config_common::{
    daemon_node_config, deployment, init_test_data_dir, write_config,
};
use crate::helpers::http::{create_http_bundle, sha256_checksum};

#[test]
#[ignore = "requires binding a local HTTP server (may be blocked in sandboxed test environments)"]
fn http_bundle_is_downloaded_and_resolved() {
    let (_data_dir, peppy_dirs) = init_test_data_dir();
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3", language: "rust" },
            process: { start_cmd: ["uvc_camera"] }
        }"#;
    let bundle_bytes = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_content);
    let sha256 = sha256_checksum(&bundle_bytes);
    let sha256 = sha256.trim_start_matches("sha256:").to_string();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );

    let url = server.url("/bundles/uvc_camera.tar.zst");
    let source = DeploymentSource::Url(DeploymentUrlSource {
        url: url.to_string(),
        sha256,
    });

    let deployments = vec![deployment(source)];

    let launcher_config = PeppyLauncher { deployments };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan = LaunchPlan::from_launch_file(daemon_node_config(), &launch_file, &peppy_dirs)
        .expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 2, "daemon + uvc_camera");
    assert!(stack.contains("uvc_camera", "1.2.3"));

    let deployment = report
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        deployment.is_resolved(),
        "http deployment should be resolved"
    );

    let node = deployment.node().expect("resolved node config");
    assert_eq!(node.manifest.tag, "1.2.3");
    assert_eq!(node.manifest.name.as_str(), "uvc_camera");
}

#[test]
#[ignore = "requires binding a local HTTP server (may be blocked in sandboxed test environments)"]
fn http_bundle_is_cloned_and_same_tag_updates_code() {
    let (_data_dir, peppy_dirs) = init_test_data_dir();
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", language: "rust" },
            process: { start_cmd: ["run_v1"] }
        }"#;
    let bundle_bytes_v1 = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_v1);
    let checksum_v1 = sha256_checksum(&bundle_bytes_v1);

    let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", language: "rust" },
            process: { start_cmd: ["run_v2"] }
        }"#;
    let bundle_bytes_v2 = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_v2);
    let checksum_v2 = sha256_checksum(&bundle_bytes_v2);

    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/uvc_camera.tar.zst"))
            .times(2)
            .respond_with(cycle(vec![
                Box::new(status_code(200).body(bundle_bytes_v1)),
                Box::new(status_code(200).body(bundle_bytes_v2)),
            ])),
    );

    let url = server.url("/bundles/uvc_camera.tar.zst");

    let sha256_v1 = checksum_v1.trim_start_matches("sha256:").to_string();
    let deployments = vec![deployment(DeploymentSource::Url(DeploymentUrlSource {
        url: url.to_string(),
        sha256: sha256_v1,
    }))];

    let launcher_config = PeppyLauncher { deployments };
    let launch_file = write_config(
        temp_dir.path().join("peppy_launcher.json5"),
        launcher_config,
    );

    let plan = LaunchPlan::from_launch_file(daemon_node_config(), &launch_file, &peppy_dirs)
        .expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "daemon + uvc_camera");
    let planned = plan
        .report()
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(planned.is_resolved(), "http deployment should resolve");
    let start_cmd_v1 = planned
        .node()
        .expect("resolved node config")
        .process
        .as_ref()
        .unwrap()
        .start_cmd
        .clone();
    assert_eq!(start_cmd_v1, vec!["run_v1".to_string()]);

    let sha256_v2 = checksum_v2.trim_start_matches("sha256:").to_string();
    let deployments = vec![deployment(DeploymentSource::Url(DeploymentUrlSource {
        url: url.to_string(),
        sha256: sha256_v2,
    }))];
    let launcher_config = PeppyLauncher { deployments };
    write_config(launch_file.clone(), launcher_config);

    let plan = LaunchPlan::from_launch_file(daemon_node_config(), &launch_file, &peppy_dirs)
        .expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "daemon + uvc_camera");
    let planned = plan
        .report()
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        planned.is_resolved(),
        "http deployment should still resolve"
    );
    let start_cmd_v2 = planned
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
