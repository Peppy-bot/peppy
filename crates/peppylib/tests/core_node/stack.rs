use std::time::Duration;

use core_node_api::encoding::{StackListRequest, StackListResponse};
use core_node_api::names;
use core_node_api::{SerializedEdge, SerializedInstance, SerializedNode, SerializedNodeGraph};
use peppylib::core_node::stack::stack_list;
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use pmi::ZenohAdapter;

const CORE_NODE: &str = "test_core";
const CLIENT_INSTANCE: &str = "test_caller";
const SERVER_INSTANCE: &str = "test_server";

fn fixture_graph() -> SerializedNodeGraph {
    let brain = SerializedNode {
        name: "brain".to_string(),
        tag: "0.1.0".to_string(),
        config_path: "/tmp/brain.json5".to_string(),
        artifact_path: None,
        instance_ids: vec!["i1".to_string()],
        stage: Some("Ready".to_string()),
        instances: vec![SerializedInstance {
            instance_id: "i1".to_string(),
            state: "running".to_string(),
        }],
        variant_name: Some("default".to_string()),
    };
    let sensor = SerializedNode {
        name: "sensor".to_string(),
        tag: "0.1.0".to_string(),
        config_path: "/tmp/sensor.json5".to_string(),
        artifact_path: None,
        instance_ids: vec![],
        stage: Some("Added".to_string()),
        instances: vec![],
        variant_name: None,
    };
    SerializedNodeGraph {
        nodes: vec![brain.clone(), sensor.clone()],
        edges: vec![SerializedEdge {
            from: brain,
            to: sensor,
        }],
    }
}

/// Spins up a single-shot `STACK_LIST` listener that returns `graph` serialized
/// as JSON, and `dot_graph` only when the inbound request asked for it.
async fn spawn_stub_listener(server: MessengerHandle, graph: SerializedNodeGraph, dot_graph: &str) {
    let dot_graph = dot_graph.to_string();
    let mut endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        CORE_NODE,
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

    // Allow the listener to propagate before the client polls.
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_parses_graph_and_includes_dot_graph_when_requested() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");
    let server = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("server handle");
    let client = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("client handle");

    let graph = fixture_graph();
    spawn_stub_listener(server, graph.clone(), "digraph {}").await;

    let result = stack_list(
        &StackListRequest::new(true),
        &client,
        CORE_NODE,
        CLIENT_INSTANCE,
        CORE_NODE,
        Duration::from_secs(2),
    )
    .await
    .expect("stack_list should succeed");

    assert_eq!(result.graph, graph);
    assert_eq!(result.dot_graph.as_deref(), Some("digraph {}"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_returns_none_dot_graph_when_not_requested() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");
    let server = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("server handle");
    let client = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("client handle");

    let graph = fixture_graph();
    spawn_stub_listener(server, graph.clone(), "digraph {}").await;

    let result = stack_list(
        &StackListRequest::new(false),
        &client,
        CORE_NODE,
        CLIENT_INSTANCE,
        CORE_NODE,
        Duration::from_secs(2),
    )
    .await
    .expect("stack_list should succeed");

    assert_eq!(result.graph, graph);
    assert!(result.dot_graph.is_none());
}
