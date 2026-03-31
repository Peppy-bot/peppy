mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, create_tar_zst_from_dir, start_core_node_with_mock_messenger,
    write_peppy_json5,
};
use common::{NodeStartTestTimeouts, send_node_add_and_wait, send_node_start_and_wait};
use config::consts::NODE_CONFIG_FILE;
use config::launcher::Name as InstanceName;
use config::node::{Name, PeppygenLanguage};
use config::runtime::{NodeInstance, RuntimeConfig};
use config::test_helpers;
use core_node::encoding::{NodeInfoRequest, NodeInfoResponse, NodeSource};
use core_node::names;
use gix_url::Url as GitUrl;
use peppylib::PeppyError;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use sha2::{Digest, Sha256};
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;

/// Sends a `NODE_INFO` poll request to the given core node and returns the raw result.
async fn poll_node_info(
    started_core_node: &common::StartedCoreNode,
    request: &NodeInfoRequest,
    timeout: Duration,
) -> core_node::Result<NodeInfoResponse> {
    request
        .poll(
            &started_core_node.caller_handle,
            &started_core_node.core_node_name,
            CALLER_INSTANCE_ID,
            &started_core_node.core_node_name,
            timeout,
        )
        .await
}
use {
    httptest::Expectation, httptest::Server, httptest::matchers::request,
    httptest::responders::status_code,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_on_fs_node_success() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "0.1.0";
    const TARGET_INSTANCE_ID: &str = "fs_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(node_dir.path(), &peppy_json5);

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info request should succeed");

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

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info request should succeed");

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

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let request = NodeInfoRequest::new(NodeSource::Git {
        repo_url,
        repo_path: TARGET_REPO_PATH.to_owned(),
        repo_ref: Some(TARGET_NODE_TAG.to_owned()),
    });

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(10))
        .await
        .expect("node_info request should succeed");

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

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(10))
        .await
        .expect("node_info request should succeed");

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

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);

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
    let bundle_sha256 = format!("{:x}", Sha256::digest(&bundle_bytes));

    let server = Server::run();
    server.expect(
        Expectation::matching(request::method_path("GET", "/bundles/http_node.tar.zst"))
            .times(2)
            .respond_with(status_code(200).body(bundle_bytes)),
    );
    let url = url::Url::parse(&server.url("/bundles/http_node.tar.zst").to_string())
        .expect("http bundle url should parse");

    let request = NodeInfoRequest::new(NodeSource::Http {
        url: url.clone(),
        sha256: Some(bundle_sha256.clone()),
    });

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(10))
        .await
        .expect("node_info request should succeed");

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

    let request = NodeInfoRequest::new(NodeSource::Http {
        url,
        sha256: Some(bundle_sha256),
    });

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(10))
        .await
        .expect("node_info request should succeed");

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

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(source_dir.path(), &peppy_json5);

    let add_result = send_node_add_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
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
    let node_handle = MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready_task_1 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_1,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_ready service (instance 1)"),
    );
    let _health_task_1 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_1,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_health service (instance 1)"),
    );

    let _ready_task_2 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_2,
            TARGET_NODE_NAME,
        )
        .await
        .expect("failed to start node_ready service (instance 2)"),
    );
    let _health_task_2 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_core_node.core_node_name,
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
        &started_core_node.core_node_name,
    )
    .expect("runtime config should be valid");
    let runtime_config_json5_1 =
        serde_json5::to_string(&runtime_config_1).expect("runtime config should serialize");

    let start_response_1 = send_node_start_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
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
        &started_core_node.core_node_name,
    )
    .expect("runtime config should be valid");
    let runtime_config_json5_2 =
        serde_json5::to_string(&runtime_config_2).expect("runtime config should serialize");

    let start_response_2 = send_node_start_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
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

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info request should succeed");

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
async fn listen_for_node_info_recovers_after_invalid_request() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // First, send a request that is guaranteed to fail quickly without performing I/O.
    let bad_url = url::Url::parse("https://example.com/bad_url")
        .expect("bad test URL should parse as a valid URL");
    let bad_request = NodeInfoRequest::new(NodeSource::Http {
        url: bad_url,
        sha256: None,
    });

    let err = poll_node_info(&started_core_node, &bad_request, Duration::from_secs(2))
        .await
        .expect_err("node_info should return an error for invalid HTTP source");

    let core_node::Error::Peppylib(PeppyError::ServiceError {
        service_name,
        reason,
        ..
    }) = err
    else {
        panic!("expected ServiceError, got: {err:?}");
    };

    assert_eq!(service_name, names::NODE_INFO);
    assert!(
        reason.contains("tar.zst") || reason.contains("tar.zstd") || reason.contains(".tzst"),
        "error reason should mention supported archive types; got: {reason}"
    );

    // Then send a valid request to ensure the node_info listener is still alive.
    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = r#"{
            schema_version: 1,
            manifest: {
                name: "{TARGET_NODE_NAME}",
                tag: "{TARGET_NODE_TAG}",
            },
            execution: {
                language: "rust",
                start_cmd: ["sleep", "10"]
            }
        }"#
    .replace("{TARGET_NODE_NAME}", TARGET_NODE_NAME)
    .replace("{TARGET_NODE_TAG}", TARGET_NODE_TAG);
    write_peppy_json5(node_dir.path(), &peppy_json5);

    let request = NodeInfoRequest::new(NodeSource::Fs(node_dir.path().to_path_buf()));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info should still work after a failed request");
    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_with_fs_variant_success() {
    const ROOT_NODE_NAME: &str = "variant_root";
    const ROOT_NODE_TAG: &str = "0.2.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // Create root node directory with a variant declared in the manifest.
    let root_dir = tempfile::tempdir().expect("failed to create temp root dir");
    let root_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "variant_root",
            tag: "0.2.0",
            variants: [
                { name: "mock", source: { local: "./mock" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(root_dir.path(), root_peppy_json5);

    // Create the variant subdirectory with its own peppy.json5 (different language).
    let mock_dir = root_dir.path().join("mock");
    std::fs::create_dir_all(&mock_dir).expect("failed to create mock dir");
    std::fs::write(
        mock_dir.join(NODE_CONFIG_FILE),
        r#"{
            schema_version: 1,
            execution: {
                language: "python",
                start_cmd: ["python", "main.py"],
                parameters: {
                    mode: "string"
                }
            }
        }"#,
    )
    .expect("failed to write mock variant config");

    // Request with variant
    let request = NodeInfoRequest::new(NodeSource::Fs(root_dir.path().to_path_buf()))
        .with_variant(NodeSource::Fs(std::path::PathBuf::from("mock")));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info with variant should succeed");

    // Manifest comes from root
    assert_eq!(info_response.config.manifest.name.as_str(), ROOT_NODE_NAME);
    assert_eq!(info_response.config.manifest.tag, ROOT_NODE_TAG);

    // Runtime comes from variant
    assert_eq!(
        info_response.config.execution.language,
        PeppygenLanguage::Python
    );
    assert_eq!(
        info_response.config.execution.start_cmd.as_deref(),
        Some(&["python".to_string(), "main.py".to_string()][..])
    );
    assert!(
        info_response
            .config
            .execution
            .parameters
            .contains_key("mode"),
        "variant parameters should be present"
    );

    // Variant name should be reported
    assert_eq!(info_response.variant_name.as_deref(), Some("mock"));
}

/// Verifies that variant resolution from a filesystem archive (`.tar.zst`) uses the
/// archived root directory, not the host filesystem. A decoy variant directory is placed
/// on the host to catch incorrect path resolution.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_with_fs_archive_variant_uses_archived_root() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    let bundle_dir = tempfile::tempdir().expect("failed to create temp bundle dir");
    let archived_root_dir = bundle_dir.path().join("archived_root");
    let archived_variant_dir = archived_root_dir.join("mock_node");
    std::fs::create_dir_all(&archived_variant_dir).expect("failed to create archived variant dir");

    let root_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "archive_variant_root",
            tag: "0.3.0",
            variants: [
                { name: "mock", source: { local: "./mock_node" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(&archived_root_dir, root_peppy_json5);

    write_peppy_json5(
        &archived_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "python",
                start_cmd: ["python", "from_archive.py"]
            }
        }"#,
    );

    let host_decoy_variant_dir = bundle_dir.path().join("mock_node");
    std::fs::create_dir_all(&host_decoy_variant_dir)
        .expect("failed to create host decoy variant dir");
    write_peppy_json5(
        &host_decoy_variant_dir,
        r#"{
            schema_version: 1,
            execution: {
                language: "python",
                start_cmd: ["python", "from_host_dir.py"]
            }
        }"#,
    );

    let bundle_path = bundle_dir.path().join("archive_variant_root.tar.zst");
    create_tar_zst_from_dir(&archived_root_dir, &bundle_path, "root_node");

    let request = NodeInfoRequest::new(NodeSource::Fs(bundle_path))
        .with_variant(NodeSource::Fs(std::path::PathBuf::from("mock")));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info with archive variant should succeed");

    assert_eq!(
        info_response.config.manifest.name.as_str(),
        "archive_variant_root"
    );
    assert_eq!(info_response.config.manifest.tag, "0.3.0");
    assert_eq!(
        info_response.config.execution.language,
        PeppygenLanguage::Python
    );
    assert_eq!(
        info_response.config.execution.start_cmd.as_deref(),
        Some(&["python".to_string(), "from_archive.py".to_string()][..])
    );
    assert_eq!(info_response.variant_name.as_deref(), Some("mock"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_with_unknown_variant_fails() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    // Create root node with a declared variant that differs from the requested one.
    let root_dir = tempfile::tempdir().expect("failed to create temp root dir");
    let root_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "has_variants_node",
            tag: "0.1.0",
            variants: [
                { name: "existing", source: { local: "./existing" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(root_dir.path(), root_peppy_json5);

    let request = NodeInfoRequest::new(NodeSource::Fs(root_dir.path().to_path_buf()))
        .with_variant(NodeSource::Fs(std::path::PathBuf::from("nonexistent")));

    let err = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect_err("node_info with unknown variant should fail");

    let core_node::Error::Peppylib(PeppyError::ServiceError { reason, .. }) = err else {
        panic!("expected ServiceError, got: {err:?}");
    };

    assert!(
        reason.contains("not found in manifest"),
        "error should mention variant not found in manifest; got: {reason}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_without_variant_shows_available_variants() {
    let started_core_node = start_core_node_with_mock_messenger().await;

    // Create root node with variants declared in manifest.
    let root_dir = tempfile::tempdir().expect("failed to create temp root dir");
    let root_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "multi_variant_node",
            tag: "1.0.0",
            variants: [
                { name: "mock", source: { local: "./mock" } },
                { name: "sim", source: { local: "./sim" } }
            ]
        },
        execution: {
            language: "rust",
            start_cmd: ["sleep", "10"]
        }
    }"#;
    write_peppy_json5(root_dir.path(), root_peppy_json5);

    // Request WITHOUT variant
    let request = NodeInfoRequest::new(NodeSource::Fs(root_dir.path().to_path_buf()));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info should succeed");

    // No variant applied
    assert!(
        info_response.variant_name.is_none(),
        "variant_name should be None when no variant requested"
    );

    // Manifest should contain variant declarations
    let variants = info_response
        .config
        .manifest
        .variants
        .as_ref()
        .expect("manifest should contain variants");
    let variant_names: Vec<&str> = variants.iter().map(|v| v.name.as_str()).collect();
    assert_eq!(variant_names, vec!["mock", "sim"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn listen_for_node_info_auto_resolves_default_variant() {
    const ROOT_NODE_NAME: &str = "default_variant_node";
    const ROOT_NODE_TAG: &str = "0.1.0";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // Create root node with a "default" variant and NO execution in root config.
    let root_dir = tempfile::tempdir().expect("failed to create temp root dir");
    let root_peppy_json5 = r#"{
        schema_version: 1,
        manifest: {
            name: "default_variant_node",
            tag: "0.1.0",
            variants: [
                { name: "default", source: { local: "./default_variant" } }
            ]
        },
        interfaces: {
            topics: {
                emits: [{ name: "sensor_data" }]
            }
        }
    }"#;
    write_peppy_json5(root_dir.path(), root_peppy_json5);

    // Create the default variant subdirectory with execution.
    let variant_dir = root_dir.path().join("default_variant");
    std::fs::create_dir_all(&variant_dir).expect("failed to create default variant dir");
    std::fs::write(
        variant_dir.join(NODE_CONFIG_FILE),
        r#"{
            schema_version: 1,
            execution: {
                language: "python",
                start_cmd: ["python", "main.py"]
            }
        }"#,
    )
    .expect("failed to write default variant config");

    // Request WITHOUT specifying a variant — should auto-resolve "default".
    let request = NodeInfoRequest::new(NodeSource::Fs(root_dir.path().to_path_buf()));

    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info should auto-resolve default variant without panic");

    // Manifest comes from root
    assert_eq!(info_response.config.manifest.name.as_str(), ROOT_NODE_NAME);
    assert_eq!(info_response.config.manifest.tag, ROOT_NODE_TAG);

    // Interfaces inherited from root
    assert!(
        info_response.config.interfaces.topics.is_some(),
        "interfaces should be inherited from root"
    );

    // Execution comes from the default variant
    assert_eq!(
        info_response.config.execution.language,
        PeppygenLanguage::Python
    );
    assert_eq!(
        info_response.config.execution.start_cmd.as_deref(),
        Some(&["python".to_string(), "main.py".to_string()][..])
    );

    // Variant name should be reported as "default"
    assert_eq!(
        info_response.variant_name.as_deref(),
        Some("default"),
        "auto-resolved default variant name should be reported"
    );
}
