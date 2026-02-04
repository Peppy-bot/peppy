mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, start_master_node_with_mock_messenger, write_peppy_json5,
};
use common::{NodeStartTestTimeouts, send_node_add_and_wait, send_node_start_and_wait};
use config::consts::NODE_CONFIG_FILE;
use config::node::{InterfaceKind, Name};
use config::peppy_config::Name as InstanceName;
use config::runtime::{NodeInstance, RuntimeConfig};
use config::test_helpers;
use gix_url::Url as GitUrl;
use master_node::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use master_node::names;
use peppylib::ServiceMessenger;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use {
    httptest::Expectation, httptest::Server, httptest::matchers::request,
    httptest::responders::status_code,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_on_fs_node_success() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "fs_instance";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );
    write_peppy_json5(node_dir.path(), &peppy_json5);

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
    assert!(
        !info_response.is_in_node_stack,
        "node should not yet be in the node stack"
    );
    assert!(
        info_response.instances_names.is_empty(),
        "no instances should be reported when node is not in stack"
    );

    // Node has no interfaces, so integrity should be empty but config_integrity should be set
    assert!(
        info_response.interfaces_integrity.is_empty(),
        "node with no interfaces should have empty interfaces_integrity"
    );
    assert_eq!(
        info_response.config_integrity.len(),
        64,
        "config_integrity should be a 64-character hex SHA256 hash"
    );

    node_stack
        .push_config(info_response.config.clone(), false, node_dir.path())
        .expect("push_config should succeed");
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id), None)
        .expect("add_instance should succeed");

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert!(info_response.is_in_node_stack, "node should be in stack");
    assert_eq!(
        info_response.instances_names,
        vec![TARGET_INSTANCE_ID.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_on_git_node_success() {
    const TARGET_NODE_NAME: &str = "uvc_camera";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_REPO_PATH: &str = "nodes/uvc_camera";
    const TARGET_INSTANCE_ID: &str = "git_instance";

    let git_repo_temp_dir = TempDir::new().expect("failed to create temp dir for git repo");
    let git_repo_path = test_helpers::create_nodes_git_repo(&git_repo_temp_dir);
    let repo_url = GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse");

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let request = NodeInfoRequest::new(NodeSource::Git {
        repo_url,
        repo_path: TARGET_REPO_PATH.to_owned(),
        repo_ref: Some(TARGET_NODE_TAG.to_owned()),
    });
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
    assert!(
        !info_response.is_in_node_stack,
        "node should not yet be in the node stack"
    );
    assert!(
        info_response.instances_names.is_empty(),
        "no instances should be reported when node is not in stack"
    );

    node_stack
        .push_config(
            info_response.config.clone(),
            false,
            git_repo_path.join(TARGET_REPO_PATH),
        )
        .expect("push_config should succeed");
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id), None)
        .expect("add_instance should succeed");

    let request = NodeInfoRequest::new(NodeSource::Git {
        repo_url: GitUrl::try_from(git_repo_path.as_path()).expect("git repo path should parse"),
        repo_path: TARGET_REPO_PATH.to_owned(),
        repo_ref: Some(TARGET_NODE_TAG.to_owned()),
    });
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert!(info_response.is_in_node_stack, "node should be in stack");
    assert_eq!(
        info_response.instances_names,
        vec![TARGET_INSTANCE_ID.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_on_http_node_success() {
    const TARGET_NODE_NAME: &str = "http_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "http_instance";

    let started_master = start_master_node_with_mock_messenger().await;
    let node_stack = started_master.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }}
        }}"#
    );

    let manifest_path = bundle_dir.path().join(NODE_CONFIG_FILE);
    std::fs::write(&manifest_path, &peppy_json5).expect("failed to write manifest");

    let mut tar_data = Vec::new();
    {
        let mut tar_builder = tar::Builder::new(&mut tar_data);
        tar_builder
            .append_path_with_name(&manifest_path, NODE_CONFIG_FILE)
            .expect("failed to append manifest to tar");
        tar_builder.finish().expect("failed to finish tar");
    }

    let bundle_path = bundle_dir.path().join("http_node.tar.zst");
    let bundle_file = std::fs::File::create(&bundle_path).expect("failed to create bundle file");
    let mut encoder = zstd::Encoder::new(bundle_file, 0).expect("failed to create zstd encoder");
    encoder
        .write_all(&tar_data)
        .expect("failed to write compressed bundle");
    encoder.finish().expect("failed to finish encoder");
    let bundle_bytes = std::fs::read(&bundle_path).expect("failed to read bundle");

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/http_node.tar.zst"))
            .times(2)
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/http_node.tar.zst").to_string())
        .expect("http bundle url should parse");

    let request = NodeInfoRequest::new(NodeSource::Http { url: url.clone() });
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
    assert!(
        !info_response.is_in_node_stack,
        "node should not yet be in the node stack"
    );
    assert!(
        info_response.instances_names.is_empty(),
        "no instances should be reported when node is not in stack"
    );

    node_stack
        .push_config(info_response.config.clone(), false, bundle_dir.path())
        .expect("push_config should succeed");
    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    node_stack
        .add_instance(TARGET_NODE_NAME, TARGET_NODE_TAG, Some(&instance_id), None)
        .expect("add_instance should succeed");

    let request = NodeInfoRequest::new(NodeSource::Http { url });
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(10),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert!(info_response.is_in_node_stack, "node should be in stack");
    assert_eq!(
        info_response.instances_names,
        vec![TARGET_INSTANCE_ID.to_string()]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_has_instance_ids() {
    const TARGET_NODE_NAME: &str = "instance_ids_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID_1: &str = "instance_one";
    const TARGET_INSTANCE_ID_2: &str = "instance_two";

    let started_master = start_master_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = format!(
        r#"{{
            schema_version: 1,
            manifest: {{
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
                language: "rust",
                start_cmd: ["sleep", "10"]
            }},
            parameters: {{}}
        }}"#
    );
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
        None,
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_result.success,
        "node_add should succeed, got error: {:?}",
        add_result.error_message
    );

    // Simulate the node exposing ready/health services for each instance so the node_start action
    // can proceed when using the mock messenger (we start `sleep` rather than an actual node).
    let node_handle = MessengerHandle::from_shared(Arc::clone(&started_master.shared_messenger));
    let _ready_task_1 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_master.master_node_name,
            TARGET_INSTANCE_ID_1,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_ready service (instance 1)"),
    );
    let _health_task_1 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_master.master_node_name,
            TARGET_INSTANCE_ID_1,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_health service (instance 1)"),
    );

    let _ready_task_2 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_master.master_node_name,
            TARGET_INSTANCE_ID_2,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_ready service (instance 2)"),
    );
    let _health_task_2 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_master.master_node_name,
            TARGET_INSTANCE_ID_2,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_health service (instance 2)"),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_1 = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
            instance_id: InstanceName::new(TARGET_INSTANCE_ID_1).expect("valid instance id"),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started_master.master_node_name,
    )
    .expect("runtime config should be valid");
    let runtime_config_json5_1 =
        serde_json5::to_string(&runtime_config_1).expect("runtime config should serialize");

    let start_response_1 = send_node_start_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &runtime_config_json5_1,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_start (instance 1) should complete");

    assert!(
        start_response_1.result.success,
        "node_start (instance 1) should succeed, got error: {:?}",
        start_response_1.result.error_message
    );

    let runtime_config_2 = RuntimeConfig::new(
        "127.0.0.1",
        config::consts::DEFAULT_MESSAGING_PORT,
        NodeInstance {
            instance_id: InstanceName::new(TARGET_INSTANCE_ID_2).expect("valid instance id"),
            arguments: Default::default(),
        },
        TARGET_NODE_NAME,
        &started_master.master_node_name,
    )
    .expect("runtime config should be valid");
    let runtime_config_json5_2 =
        serde_json5::to_string(&runtime_config_2).expect("runtime config should serialize");

    let start_response_2 = send_node_start_and_wait(
        &started_master.caller_handle,
        &started_master.master_node_name,
        &runtime_config_json5_2,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeStartTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_start (instance 2) should complete");

    assert!(
        start_response_2.result.success,
        "node_start (instance 2) should succeed, got error: {:?}",
        start_response_2.result.error_message
    );

    let request = NodeInfoRequest::new(NodeSource::Fs(add_result.snapshot_path.clone()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert!(info_response.is_in_node_stack, "node should be in stack");

    let mut instances = info_response.instances_names;
    instances.sort();
    assert_eq!(
        instances,
        vec![
            TARGET_INSTANCE_ID_1.to_string(),
            TARGET_INSTANCE_ID_2.to_string()
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_returns_integrity_fields() {
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

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload = request.encode().expect("encode should succeed");

    let response = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload,
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");

    let info_response =
        NodeInfoResponse::decode(&response.payload().to_bytes()).expect("decode should succeed");

    assert_eq!(
        info_response.interfaces_integrity.len(),
        3,
        "should have 3 interface integrity entries (1 topic + 1 service + 1 action)"
    );

    let topic = info_response
        .interfaces_integrity
        .iter()
        .find(|i| i.interface_kind == InterfaceKind::Topic)
        .expect("should have a topic integrity entry");
    assert_eq!(topic.name, "camera_feed");
    assert!(!topic.sha256.is_empty(), "topic sha256 should not be empty");

    let service = info_response
        .interfaces_integrity
        .iter()
        .find(|i| i.interface_kind == InterfaceKind::Service)
        .expect("should have a service integrity entry");
    assert_eq!(service.name, "capture");
    assert!(
        !service.sha256.is_empty(),
        "service sha256 should not be empty"
    );

    let action = info_response
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
        info_response.config_integrity.len(),
        64,
        "config_integrity should be a 64-character hex SHA256 hash"
    );

    // Verify determinism: same request should produce same hashes
    let request2 = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));
    let request_payload2 = request2.encode().expect("encode should succeed");

    let response2 = ServiceMessenger::poll(
        &started_master.caller_handle,
        &started_master.master_node_name,
        CALLER_INSTANCE_ID,
        &started_master.master_node_name,
        names::NODE_INFO,
        Some(&started_master.master_node_name),
        None,
        request_payload2,
        Duration::from_secs(5),
    )
    .await
    .expect("second node_info request should succeed");

    let info_response2 =
        NodeInfoResponse::decode(&response2.payload().to_bytes()).expect("decode should succeed");

    assert_eq!(
        info_response.config_integrity, info_response2.config_integrity,
        "config_integrity should be deterministic"
    );
    assert_eq!(
        info_response.interfaces_integrity, info_response2.interfaces_integrity,
        "interfaces_integrity should be deterministic"
    );
}
