mod common;

use common::{
    AbortOnDrop, TIMEOUTS, answer_readiness, poll_core_node, poll_node_info,
    real_build_and_spawn_instance, send_node_add_then_build, send_node_run_and_wait,
    start_core_node_with_mock_messenger, write_peppy_json5,
};
use config::runtime::Name;
use core_node_api::encoding::{NodeInfoRequest, NodeInfoResponse};
use core_node_api::{InstanceState, NodeStage};
use peppylib::messaging::MessengerHandle;
use std::sync::Arc;

/// Happy path: add a node + start an instance, then `node info` reports the
/// stage, instance list, run logs, and (after it's set) the add log path.
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
                run_cmd: ["sleep", "300"]
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
    let info_response = poll_node_info(&started_core_node, &request).await;

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
    let run_log = &info_response.run_log_paths[0];
    assert_eq!(
        run_log.parent(),
        Some(started_core_node.peppy_dirs.logs_dir_run().as_path())
    );
    let file_name = run_log
        .file_name()
        .and_then(|name| name.to_str())
        .expect("a run log file name");
    assert!(
        file_name.starts_with(&format!("{TARGET_INSTANCE_ID}_")) && file_name.ends_with(".log"),
        "the run log is the instance's timestamped file: {file_name}"
    );
    assert!(
        run_log.is_file(),
        "the run log exists: {}",
        run_log.display()
    );
    assert!(
        info_response.add_log_path.is_none(),
        "a node pushed straight onto the stack records no add log"
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
    )
    .await;
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
    )
    .await;
    assert!(
        !info_response.instances[0].healthy,
        "node info should report the instance unhealthy after set_healthy(false)"
    );
}

/// Full add-then-build-then-run-then-info: verifies that after two instances
/// are spawned through the real goal pipeline, `node info` reports both of
/// them with their per-instance state.
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
                run_cmd: ["sleep", "300"]
            }
        }"#;
    write_peppy_json5(source_dir.path(), peppy_json5);

    send_node_add_then_build(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        source_dir.path(),
    )
    .await
    .expect("node_add should complete");

    // Simulate each instance exposing ready/health services so the node_run
    // action can proceed against the mock messenger (we start `sleep` rather
    // than a real node).
    let mut responders =
        answer_readiness(&started_core_node, TARGET_NODE_NAME, TARGET_INSTANCE_ID_1).await;
    responders
        .extend(answer_readiness(&started_core_node, TARGET_NODE_NAME, TARGET_INSTANCE_ID_2).await);

    let instance_plan_1 = common::default_instance_plan(TARGET_INSTANCE_ID_1);
    let start_response_1 = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        instance_plan_1,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &TIMEOUTS,
        None,
    )
    .await
    .expect("node_run (instance 1) should complete");
    start_response_1.end.completed();

    let instance_plan_2 = common::default_instance_plan(TARGET_INSTANCE_ID_2);
    let start_response_2 = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        instance_plan_2,
        TARGET_NODE_NAME,
        TARGET_NODE_TAG,
        &TIMEOUTS,
        None,
    )
    .await
    .expect("node_run (instance 2) should complete");
    start_response_2.end.completed();

    let info_response = poll_node_info(
        &started_core_node,
        &NodeInfoRequest::new(TARGET_NODE_NAME, TARGET_NODE_TAG),
    )
    .await;

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
/// `NodeInfoResponse::NotInStack` rather than a protocol-level error. The
/// listener must also remain healthy after a negative lookup and serve a
/// follow-up valid request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_reports_not_in_stack_and_recovers() {
    const TARGET_NODE_NAME: &str = "fs_node";
    const TARGET_NODE_TAG: &str = "v1";

    let started_core_node = start_core_node_with_mock_messenger().await;

    // First: an unknown node; the daemon answers `NotInStack` on the
    // successful response channel. No error log, no transport fault.
    let response = poll_core_node(
        &started_core_node,
        &NodeInfoRequest::new("ghost_node", "v999"),
    )
    .await;
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
                run_cmd: ["sleep", "300"]
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
    )
    .await;
    assert_eq!(
        info_response.config.manifest.name.as_str(),
        TARGET_NODE_NAME
    );
    assert_eq!(info_response.config.manifest.tag, TARGET_NODE_TAG);
}

/// `node info` carries the endpoints a running instance serves, as the
/// start sequence expanded them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn node_info_carries_the_endpoints_of_a_running_instance() {
    const NODE_NAME: &str = "info_panel_node";
    const INSTANCE_ID: &str = "info_panel_instance";

    let started_core_node = start_core_node_with_mock_messenger().await;
    let node_dir = tempfile::tempdir().expect("failed to create temp node dir");
    write_peppy_json5(
        node_dir.path(),
        &r#"{
            peppy_schema: "node/v1",
            manifest: { name: "{NODE_NAME}", tag: "v1" },
            execution: {
                language: "rust",
                run_cmd: ["sleep", "300"],
                endpoints: {
                    viewer: { kind: "page", description: "The viewer page." },
                },
            }
        }"#
        .replace("{NODE_NAME}", NODE_NAME),
    );
    send_node_add_then_build(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        node_dir.path(),
    )
    .await
    .expect("node_add should succeed");

    let node_messenger =
        MessengerHandle::from_shared(Arc::clone(&started_core_node.shared_messenger));
    let _responders = answer_readiness(&started_core_node, NODE_NAME, INSTANCE_ID).await;
    let _endpoints = AbortOnDrop(
        peppylib::services::endpoints::listen_for_node_endpoints(
            &node_messenger,
            &started_core_node.core_node_name,
            INSTANCE_ID,
            common::test_node_target(NODE_NAME),
            vec![peppylib::runtime::AnnouncedEndpoint {
                label: "viewer".to_string(),
                binding: peppylib::runtime::EndpointBinding {
                    scheme: "https".to_string(),
                    address: "127.0.0.1:8080".parse().expect("a socket address"),
                    path: "/".to_string(),
                },
            }],
        )
        .await
        .expect("node endpoints service should start"),
    );

    let start_response = send_node_run_and_wait(
        &started_core_node.caller_handle,
        &started_core_node.core_node_name,
        common::default_instance_plan(INSTANCE_ID),
        NODE_NAME,
        "v1",
        &TIMEOUTS,
        None,
    )
    .await
    .expect("node_run action should complete");
    start_response.end.completed();

    let info = poll_node_info(&started_core_node, &NodeInfoRequest::new(NODE_NAME, "v1")).await;
    assert_eq!(info.instances.len(), 1);
    assert_eq!(
        info.instances[0].endpoints,
        vec![core_node_api::InstanceEndpoint {
            label: "viewer".to_string(),
            kind: config::node::EndpointKind::Page,
            urls: vec!["https://127.0.0.1:8080/".to_string()],
        }],
        "a literal binding yields the one URL, carried with its declared kind"
    );
}
