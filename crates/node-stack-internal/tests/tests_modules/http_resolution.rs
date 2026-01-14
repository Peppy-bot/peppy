use config::peppy_config::{DeploymentNodeSource, HttpRemoteSpec, PeppyLauncher};
use httptest::{
    Expectation, Server,
    matchers::request,
    responders::{cycle, status_code},
};
use node_stack::LaunchPlan;
use tempfile::tempdir;

use crate::helpers::config_common::{deployment, master_node_config, write_config};
use crate::helpers::http::{create_http_bundle, sha256_checksum};

#[test]
fn http_bundle_is_downloaded_and_resolved() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_content = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.2.3", start_cmd: ["uvc_camera"] }
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

    let plan = LaunchPlan::from_launch_file(master_node_config(), &launch_file).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 2, "master + uvc_camera");
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
fn http_bundle_is_cloned_and_same_tag_updates_code() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", start_cmd: ["run_v1"] }
        }"#;
    let bundle_bytes_v1 = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_v1);
    let checksum_v1 = sha256_checksum(&bundle_bytes_v1);

    let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", start_cmd: ["run_v2"] }
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

    let http_spec_v1 = HttpRemoteSpec::new(url.to_string(), Some(checksum_v1))
        .expect("valid http deployment spec");

    let deployments = vec![deployment(
        "uvc_camera",
        "1.0.0",
        Some(DeploymentNodeSource::Http(http_spec_v1)),
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

    let plan = LaunchPlan::from_launch_file(master_node_config(), &launch_file).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
    let planned = plan
        .report()
        .find_deployment_by_name("uvc_camera")
        .expect("uvc_camera planned");
    assert!(planned.is_resolved(), "http deployment should resolve");
    let start_cmd_v1 = planned
        .node()
        .expect("resolved node config")
        .manifest
        .start_cmd
        .clone();
    assert_eq!(start_cmd_v1, vec!["run_v1".to_string()]);

    let http_spec_v2 = HttpRemoteSpec::new(url.to_string(), Some(checksum_v2))
        .expect("valid http deployment spec");
    let deployments = vec![deployment(
        "uvc_camera",
        "1.0.0",
        Some(DeploymentNodeSource::Http(http_spec_v2)),
        false,
    )];
    let launcher_config = PeppyLauncher {
        deployments: Some(deployments),
        logging: None,
    };
    write_config(launch_file.clone(), launcher_config);

    let plan = LaunchPlan::from_launch_file(master_node_config(), &launch_file).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
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
        .manifest
        .start_cmd
        .clone();

    assert_eq!(start_cmd_v2, vec!["run_v2".to_string()]);
    assert_ne!(start_cmd_v1, start_cmd_v2);
}
