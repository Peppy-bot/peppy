use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use config::peppy_config::{DeploymentNodeSource, HttpRemoteSpec, PeppyLauncher};
use httptest::{
    Expectation, Server,
    matchers::request,
    responders::{cycle, status_code},
};
use node_stack::{LaunchPlan, NodeStackError};
use sha2::{Digest, Sha256};
use std::io::Write;
use tempfile::tempdir;

use crate::helpers::config_common::{deployment, master_node_config, write_config};

fn sha256_checksum(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

pub fn create_http_bundle(temp_dir: &Path, bundle_name: &str, manifest_content: &str) -> Vec<u8> {
    let manifest_path = temp_dir.join("peppy.json5");
    fs::write(&manifest_path, manifest_content).expect("write manifest");

    let mut tar_data = Vec::new();
    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);
        tar_builder
            .append_path_with_name(&manifest_path, "peppy.json5")
            .expect("append manifest");
        tar_builder.finish().expect("finish tar");
    }

    let bundle_path = temp_dir.join(bundle_name);
    let bundle_file = fs::File::create(&bundle_path).expect("create bundle");
    let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("create zstd encoder");
    encoder
        .write_all(&tar_data)
        .expect("write compressed bundle");
    encoder.finish().expect("finish encoder");

    fs::read(&bundle_path).expect("read bundle")
}

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
    assert!(
        deployment.is_resolved(),
        "http deployment should be resolved"
    );

    let node = deployment.node().expect("resolved node config");
    assert_eq!(node.manifest.tag, "1.2.3");
    assert_eq!(node.manifest.name.as_str(), "uvc_camera");
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
fn http_bundle_is_cloned_and_same_tag_updates_code() {
    let temp_dir = tempdir().expect("temp dir");
    let server = Server::run();

    let manifest_v1 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v1"] }
        }"#;
    let bundle_bytes_v1 = create_http_bundle(temp_dir.path(), "uvc_camera.tar.zst", manifest_v1);
    let checksum_v1 = sha256_checksum(&bundle_bytes_v1);

    let manifest_v2 = r#"{
            schema_version: 1,
            manifest: { name: "uvc_camera", tag: "1.0.0", launch_cmd: ["run_v2"] }
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

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
    let planned = plan
        .report()
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(planned.is_resolved(), "http deployment should resolve");
    let launch_cmd_v1 = planned
        .node()
        .expect("resolved node config")
        .manifest
        .launch_cmd
        .clone()
        .expect("launch command present");
    assert_eq!(launch_cmd_v1, vec!["run_v1".to_string()]);

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

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    assert_eq!(plan.node_stack().len(), 2, "master + uvc_camera");
    let planned = plan
        .report()
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "uvc_camera")
        .expect("uvc_camera planned");
    assert!(
        planned.is_resolved(),
        "http deployment should still resolve"
    );
    let launch_cmd_v2 = planned
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
fn http_bundle_invalid_parameters_rejected() {
    let temp_dir = tempdir().expect("temp dir");
    let bundle_dir = tempdir().expect("bundle dir");
    let server = Server::run();

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
    let bundle_bytes = create_http_bundle(
        bundle_dir.path(),
        "lidar_sensor.tar.zst",
        peppy_json_content,
    );
    let checksum = sha256_checksum(&bundle_bytes);
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/lidar_sensor.tar.zst"))
            .respond_with(status_code(200).body(bundle_bytes)),
    );

    let url = server.url("/bundles/lidar_sensor.tar.zst");
    let launcher_content = r#"{
      deployments: [
        {
          name: "lidar_sensor",
          tag: "0.1.0",
          source: {
            bundle_url: "$URL_PLACEHOLDER",
            checksum: "$CHECKSUM_PLACEHOLDER"
          },
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
    }"#
    .replace("$URL_PLACEHOLDER", &url.to_string())
    .replace("$CHECKSUM_PLACEHOLDER", &checksum);

    let launch_file = temp_dir.path().join("peppy_launcher.json5");
    std::fs::write(&launch_file, launcher_content).expect("write launcher config");

    let plan =
        LaunchPlan::from_launch_file(master_node_config(), &launch_file, None).expect("plan");
    let stack = plan.node_stack();
    let report = plan.report();

    assert_eq!(stack.len(), 1, "config should only have the master node");

    let lidar_deployment = report
        .deployments()
        .iter()
        .find(|deployment| deployment.deployment().name.as_str() == "lidar_sensor")
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

    let expected_identifier = format!("{}:{}", "lidar_sensor", lidar_deployment.deployment().tag);
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

    let nodes_cache_dir = temp_dir.path().join(".peppy").join("nodes");
    assert!(
        nodes_cache_dir.is_dir(),
        "nodes cache dir {:?} should be created even on failure",
        nodes_cache_dir
    );
}

#[test]
fn http_bundle_correct_parameters_with_invalid_type_rejected() {
    todo!(
        "A bit similar to `http_bundle_invalid_parameters_rejected` but the `fps` parameter is of type `string` instead of `i32`"
    )
}
