use std::time::Duration;

use core_node_api::encoding::{StackListRequest, StackListResponse};
use core_node_api::names;
use core_node_api::{
    InstanceState, NodeStage, SerializedEdge, SerializedInstance, SerializedNode,
    SerializedNodeGraph,
};
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::NodeRunner;
use peppylib::stack;
use pmi::ZenohdInstance;
use tempfile::TempDir;

use super::common::{
    CORE_NODE, SERVER_INSTANCE, start_router_and_runner, test_node_target, wait_until_reachable,
};

/// Spins up a single-shot `STACK_LIST` listener that returns `graph` serialized
/// as JSON, and `dot_graph` only when the inbound request asked for it.
async fn spawn_stub_listener(server: MessengerHandle, graph: SerializedNodeGraph, dot_graph: &str) {
    let dot_graph = dot_graph.to_string();
    let mut endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        names::STACK_LIST,
    )
    .await
    .expect("listen should succeed");

    tokio::spawn(async move {
        endpoint
            .handle_next_request(|request| async move {
                let payload = request.message().payload();
                let inbound =
                    StackListRequest::decode(payload.as_ref()).expect("decode StackListRequest");
                let dot = if inbound.with_dot_graph() {
                    Some(dot_graph.clone())
                } else {
                    None
                };
                let graph_json =
                    serde_json::to_string(&graph).expect("serialize SerializedNodeGraph");
                Ok(StackListResponse::new(dot, graph_json)
                    .encode()
                    .expect("encode StackListResponse"))
            })
            .await
            .expect("handle_next_request should succeed");
    });
}

/// Spawns the stub listener for `graph` on a shared router/runner, and waits
/// for reachability. The router and temp dir are returned so callers hold
/// them for the duration of the test — dropping them tears down the messaging
/// fabric / config file.
async fn setup_stub(
    graph: SerializedNodeGraph,
    dot_graph: &str,
) -> (ZenohdInstance, TempDir, NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_stub_listener(server, graph, dot_graph).await;
    wait_until_reachable(node_runner.messenger(), names::STACK_LIST).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_parses_graph_and_includes_dot_graph_when_requested() {
    let brain = SerializedNode {
        name: "brain".to_string(),
        tag: "v1".to_string(),
        config_path: "/tmp/brain.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Ready),
        instances: vec![SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            slot_bindings: std::collections::BTreeMap::new(),
        }],
    };
    let sensor = SerializedNode {
        name: "sensor".to_string(),
        tag: "v1".to_string(),
        config_path: "/tmp/sensor.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Added),
        instances: vec![],
    };
    let graph = SerializedNodeGraph {
        nodes: vec![brain.clone(), sensor.clone()],
        edges: vec![SerializedEdge {
            from: brain,
            to: sensor,
            via_interface: None,
        }],
    };

    let (_router, _temp_dir, node_runner) = setup_stub(graph.clone(), "digraph {}").await;

    let result = stack::list(&node_runner, true, Duration::from_secs(3))
        .await
        .expect("stack_list should succeed");

    assert_eq!(result.graph, graph);
    let brain = result
        .graph
        .nodes
        .iter()
        .find(|n| n.name == "brain")
        .expect("brain node should be present in the returned stack");
    assert_eq!(brain.stage, Some(NodeStage::Ready));
    assert_eq!(brain.instances.len(), 1);
    assert_eq!(brain.instances[0].instance_id, "i1");
    assert_eq!(brain.instances[0].state, InstanceState::Running);
    assert_eq!(result.dot_graph.as_deref(), Some("digraph {}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_returns_none_dot_graph_when_not_requested() {
    let brain = SerializedNode {
        name: "brain".to_string(),
        tag: "v1".to_string(),
        config_path: "/tmp/brain.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Ready),
        instances: vec![SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
            healthy: true,
            slot_bindings: std::collections::BTreeMap::new(),
        }],
    };
    let sensor = SerializedNode {
        name: "sensor".to_string(),
        tag: "v1".to_string(),
        config_path: "/tmp/sensor.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Added),
        instances: vec![],
    };
    let graph = SerializedNodeGraph {
        nodes: vec![brain.clone(), sensor.clone()],
        edges: vec![SerializedEdge {
            from: brain,
            to: sensor,
            via_interface: None,
        }],
    };

    let (_router, _temp_dir, node_runner) = setup_stub(graph.clone(), "digraph {}").await;

    let result = stack::list(&node_runner, false, Duration::from_secs(3))
        .await
        .expect("stack_list should succeed");

    assert_eq!(result.graph, graph);
    let brain = result
        .graph
        .nodes
        .iter()
        .find(|n| n.name == "brain")
        .expect("brain node should be present in the returned stack");
    assert_eq!(brain.stage, Some(NodeStage::Ready));
    assert_eq!(brain.instances.len(), 1);
    assert_eq!(brain.instances[0].instance_id, "i1");
    assert_eq!(brain.instances[0].state, InstanceState::Running);
    assert!(result.dot_graph.is_none());
}
