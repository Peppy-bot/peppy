mod common;

use common::{
    AbortOnDrop, CALLER_INSTANCE_ID, NodeRunTestTimeouts, real_build_and_spawn_instance,
    send_node_add_then_build, send_node_run_and_wait, start_core_node_with_mock_messenger,
    write_peppy_json5,
};
use config::runtime::Name;
use core_node_api::encoding::{NodeInfo, NodeInfoRequest, NodeInfoResponse};
use core_node_api::{InstanceState, NodeStage};
use peppylib::core_node::transport::poll as transport_poll;
use peppylib::messaging::MessengerHandle;
use peppylib::services::health::listen_for_node_health;
use peppylib::services::ready::listen_for_node_ready;
use std::sync::Arc;
use std::time::Duration;

/// Sends a `NODE_INFO` poll request and returns the raw response enum.
///
/// Most tests assert on the `Found` body and use `poll_node_info_found`
/// below; the negative-lookup test uses this variant directly.
async fn poll_node_info_raw(
    started_core_node: &common::StartedCoreNode,
    request: &NodeInfoRequest,
    timeout: Duration,
) -> peppylib::PeppyResult<NodeInfoResponse> {
    transport_poll(
        request,
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        CALLER_INSTANCE_ID,
        &started_core_node.core_node_name,
        timeout,
    )
    .await
}

/// Sends a `NODE_INFO` poll request and unwraps the `Found` body. Panics
/// if the daemon answers `NotInStack`; happy-path tests expect the node
/// they set up to be present.
async fn poll_node_info(
    started_core_node: &common::StartedCoreNode,
    request: &NodeInfoRequest,
    timeout: Duration,
) -> peppylib::PeppyResult<NodeInfo> {
    match poll_node_info_raw(started_core_node, request, timeout).await? {
        NodeInfoResponse::Found(info) => Ok(*info),
        NodeInfoResponse::NotInStack => panic!(
            "node_info unexpectedly reported `{}:{}` as not in the stack",
            request.node_name, request.node_tag
        ),
    }
}

/// Happy path: add a node + start an instance, then `node info` reports the
/// stage, instance list, run logs, and (after it's set) the add log path.
/// This is the rewrite of `listen_for_node_info_on_fs_node_success`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_reports_stage_instances_and_logs_for_stack_resident_node() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID: &str = "fs_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_stack = started_core_node.node_stack.clone();

    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "fs_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#;
    write_peppy_json5(node_dir.path(), peppy_json5);

    // Parse the config and push it directly; we don't need the full add
    // pipeline for an info-only test, and `real_build_and_spawn_instance`
    // takes it from there.
    let config = config::node::NodeConfigParser::from_path(node_dir.path().join("peppy.json5"))
        .expect("parse config");
    node_stack
        .push_config(config, false, node_dir.path())
        .expect("push_config should succeed");

    let instance_id = Name::new(TARGET_INSTANCE_ID).expect("valid instance id");
    let _running = real_build_and_spawn_instance(
        &started_core_node,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &instance_id,
    )
    .await;

    let request = NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG);
    let info_response = poll_node_info(&started_core_node, &request, Duration::from_secs(5))
        .await
        .expect("node_info request should succeed");

    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
    assert_eq!(
        info_response.config_integrity.len(),
        64,
        "config_integrity should be a 64-character hex SHA256 hash"
    );
    assert_eq!(
        info_response.stage,
        NodeStage::Ready,
        "stage should be Ready after build + spawn"
    );
    assert_eq!(info_response.instances.len(), 1);
    assert_eq!(info_response.instances[0].instance_id, TARGET_INSTANCE_ID);
    assert_eq!(info_response.instances[0].state, InstanceState::Running);
    assert!(
        info_response.instances[0].healthy,
        "a freshly-running instance should report healthy"
    );
    assert_eq!(info_response.run_log_paths.len(), 1);
    let expected_run_log = started_core_node
        .peppy_dirs
        .logs_dir_run()
        .join(format!("{}.log", TARGET_INSTANCE_ID));
    assert_eq!(info_response.run_log_paths[0], expected_run_log);
    assert!(
        info_response.add_log_path.is_none(),
        "force_built bypass does not record an add log"
    );

    // Record an add log path via the public setter and re-poll; the response
    // should surface it.
    let recorded_add_log = started_core_node
        .peppy_dirs
        .logs_dir_add()
        .join("recorded.log");
    node_stack.set_add_log_path(TARGET_NODE_NAME, TARGET_NODE_TAG, recorded_add_log.clone());
    let info_response = poll_node_info(
        &started_core_node,
        &NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");
    assert_eq!(
        info_response.add_log_path.as_deref(),
        Some(recorded_add_log.as_path())
    );

    // Flip the tracked instance unhealthy (as the health monitor does on a
    // failed `node_health` probe) and re-poll. `node info` reads the live flag
    // off the tracked instance, so the response must now report `unhealthy`;
    // this makes the handler's `healthy: instance.healthy()` wiring
    // load-bearing rather than a value a test never distinguishes from a
    // hardcoded `true`.
    node_stack
        .find_by_instance_id(&instance_id)
        .expect("running instance should be findable")
        .set_healthy(false);
    let info_response = poll_node_info(
        &started_core_node,
        &NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");
    assert!(
        !info_response.instances[0].healthy,
        "node info should report the instance unhealthy after set_healthy(false)"
    );
}

/// Full add-then-build-then-run-then-info: verifies that after two instances
/// are spawned through the real goal pipeline, `node info` reports both of
/// them with their per-instance state. Direct port of
/// `listen_for_node_info_has_instance_ids` with the request input swapped.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_has_instance_ids() {
    const TARGET_NODE_NAME: &str = "instance_ids_node";
    const TARGET_NODE_TAG: &str = "v1";
    const TARGET_INSTANCE_ID_1: &str = "instance_one";
    const TARGET_INSTANCE_ID_2: &str = "instance_two";

    let started_core_node = start_core_node_with_mock_messenger().await;

    let source_dir = tempfile::tempdir().expect("failed to create temp source dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "instance_ids_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    let add_result = send_node_add_then_build(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
        Duration::from_secs(5),
        Duration::from_secs(10),
    )
    .await
    .expect("node_add should complete");

    assert!(
        add_result.success,
        "node_build should succeed, got error: {:?}",
        add_result.error_message
    );

    // Simulate each instance exposing ready/health services so the node_run
    // action can proceed against the mock messenger (we start `sleep` rather
    // than a real node).
    let node_handle = MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _ready_task_1 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_1,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start node_ready service (instance 1)"),
    );
    let _health_task_1 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_1,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start node_health service (instance 1)"),
    );
    let _ready_task_2 = AbortOnDrop(
        listen_for_node_ready(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_2,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start node_ready service (instance 2)"),
    );
    let _health_task_2 = AbortOnDrop(
        listen_for_node_health(
            &node_handle,
            &started_core_node.core_node_name,
            TARGET_INSTANCE_ID_2,
            common::test_node_target(TARGET_NODE_NAME),
        )
        .await
        .expect("failed to start node_health service (instance 2)"),
    );

    tokio::time::sleep(Duration::from_millis(50)).await;

    let runtime_config_json5_1 = common::default_runtime_config_json5(
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID_1,
    );
    let start_response_1 = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &runtime_config_json5_1,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run (instance 1) should complete");
    assert!(
        start_response_1.result.success,
        "node_run (instance 1) should succeed, got error: {:?}",
        start_response_1.result.error_message
    );

    let runtime_config_json5_2 = common::default_runtime_config_json5(
        &started_core_node.core_node_name,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        TARGET_INSTANCE_ID_2,
    );
    let start_response_2 = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        &runtime_config_json5_2,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &NodeRunTestTimeouts {
            goal: Duration::from_secs(5),
            result: Duration::from_secs(10),
        },
        None,
    )
    .await
    .expect("node_run (instance 2) should complete");
    assert!(
        start_response_2.result.success,
        "node_run (instance 2) should succeed, got error: {:?}",
        start_response_2.result.error_message
    );

    let info_response = poll_node_info(
        &started_core_node,
        &NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
        Duration::from_secs(5),
    )
    .await
    .expect("node_info request should succeed");

    let mut instance_ids: Vec<String> = info_response
        .instances
        .iter()
        .map(|i| i.instance_id.clone())
        .collect();
    instance_ids.sort();
    assert_eq!(
        instance_ids,
        vec![
            TARGET_INSTANCE_ID_1.to_string(),
            TARGET_INSTANCE_ID_2.to_string()
        ]
    );
    for inst in &info_response.instances {
        assert_eq!(
            inst.state,
            InstanceState::Running,
            "instance {} should be running",
            inst.instance_id
        );
    }
}

/// An unknown `(name, tag)` should be reported as a *successful*
/// `NodeInfoResponse::NotInStack` rather than a protocol-level error; this
/// is the whole point of the breaking response-shape change. The listener
/// must also remain healthy after a negative lookup and serve a follow-up
/// valid request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_reports_not_in_stack_and_recovers() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // First: an unknown node; the daemon answers `NotInStack` on the
    // successful response channel. No error log, no transport fault.
    let response = poll_node_info_raw(
        &started_core_node,
        &NodeInfoRequest::new("ghost_node", "v999"),
        Duration::from_secs(2),
    )
    .await
    .expect("node_info should return a successful response for an unknown node");
    assert!(
        matches!(response, NodeInfoResponse::NotInStack),
        "expected NotInStack for an unknown `(name, tag)`, got: {response:?}"
    );

    // Then: after the negative lookup, a valid request against a
    // stack-resident node must still succeed, i.e., the info listener
    // is still alive.
    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    let peppy_json5 = r#"{
            peppy_schema: "node/v1",
            manifest: {
                name: "fs_node",
                tag: "v1",
            },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "10"]
            }
        }"#;
    write_peppy_json5(node_dir.path(), peppy_json5);
    let config = config::node::NodeConfigParser::from_path(node_dir.path().join("peppy.json5"))
        .expect("parse config");
    started_core_node
        .node_stack
        .push_config(config, false, node_dir.path())
        .expect("push_config should succeed");

    let info_response = poll_node_info(
        &started_core_node,
        &NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
        Duration::from_secs(5),
    )
    .await
    .expect("node_info should still work after a failed request");
    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
}
