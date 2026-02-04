mod common;

use common::{CALLER_INSTANCE_ID, start_master_node_with_mock_messenger, write_peppy_json5};
use config::node::InterfaceKind;
use master_node::encoding::{NodeIntegrityRequest, NodeIntegrityResponse, NodeSource};
use master_node::names;
use peppylib::ServiceMessenger;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_integrity_success() {
    let started_master = start_master_node_with_mock_messenger().await;

    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "integrity_node",
            tag: "0.1.0",
            language: "rust",
            start_cmd: ["sleep", "10"]
        },
        interfaces: {
            exposes: {
                topics: [
                    {
                        name: "camera_feed",
                        qos_profile: "sensor_data",
                        message_format: {
                            timestamp: "time",
                            frame: "bytes"
                        }
                    }
                ],
                services: [
                    {
                        name: "capture",
                        request_message_format: { mode: "string" },
                        response_message_format: { success: "bool" }
                    }
                ],
                actions: [
                    {
                        name: "record"
                    }
                ]
            }
        }
    }"#;
    write_peppy_json5(node_dir.path(), peppy_json5);

    let request = NodeIntegrityRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INTEGRITY,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("node_integrity request should succeed");

    let integrity_response = NodeIntegrityResponse::decode(&response.payload().to_bytes())
        .expect("decode should succeed");

    assert_eq!(
        integrity_response.interfaces_integrity.len(),
        3,
        "should have 3 interface integrity entries (1 topic + 1 service + 1 action)"
    );

    let topic = integrity_response
        .interfaces_integrity
        .iter()
        .find(|i| i.interface_kind == InterfaceKind::Topic)
        .expect("should have a topic integrity entry");
    assert_eq!(topic.name, "camera_feed");
    assert!(!topic.sha256.is_empty(), "topic sha256 should not be empty");

    let service = integrity_response
        .interfaces_integrity
        .iter()
        .find(|i| i.interface_kind == InterfaceKind::Service)
        .expect("should have a service integrity entry");
    assert_eq!(service.name, "capture");
    assert!(
        !service.sha256.is_empty(),
        "service sha256 should not be empty"
    );

    let action = integrity_response
        .interfaces_integrity
        .iter()
        .find(|i| i.interface_kind == InterfaceKind::Action)
        .expect("should have an action integrity entry");
    assert_eq!(action.name, "record");
    assert!(
        !action.sha256.is_empty(),
        "action sha256 should not be empty"
    );

    // config_integrity should be a valid SHA256 hex string (64 chars)
    assert_eq!(
        integrity_response.config_integrity.len(),
        64,
        "config_integrity should be a 64-character hex SHA256 hash"
    );

    // Verify that the same request produces the same hashes (deterministic)
    let request2 = NodeIntegrityRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload2 = request2.encode().expect("encode should succeed");

    let response2 = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INTEGRITY,
        Some(&started_master.master_node_name),
        None,
        request_payload2,
        Duration::from_secs(5),
    )
    .await
    .expect("second node_integrity request should succeed");

    let integrity_response2 = NodeIntegrityResponse::decode(&response2.payload().to_bytes())
        .expect("decode should succeed");

    assert_eq!(
        integrity_response.config_integrity, integrity_response2.config_integrity,
        "config_integrity should be deterministic"
    );
    assert_eq!(
        integrity_response.interfaces_integrity, integrity_response2.interfaces_integrity,
        "interfaces_integrity should be deterministic"
    );
}
