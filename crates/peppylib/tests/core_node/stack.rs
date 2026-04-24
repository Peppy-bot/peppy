use std::time::{Duration, Instant};

use core_node_api::encoding::{StackListRequest, StackListResponse};
use core_node_api::names;
use core_node_api::{
    InstanceState, NodeStage, SerializedEdge, SerializedInstance, SerializedNode,
    SerializedNodeGraph,
};
use peppylib::messaging::{MessengerHandle, ServiceMessenger};
use peppylib::runtime::{NodeRunner, Processor, StandaloneConfig};
use peppylib::stack_list;
use pmi::{ZenohAdapter, ZenohdInstance};
use tempfile::TempDir;

const CORE_NODE: &str = "standalone-core";
const CLIENT_INSTANCE: &str = "test_caller";
const SERVER_INSTANCE: &str = "test_server";

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
}

/// Polls `is_reachable` until the stub listener responds, bounded by a
/// deadline. Replaces a fixed sleep: fast when zenoh discovery completes
/// quickly, and fails loudly with a clear panic if it never does.
async fn wait_until_reachable(client: &MessengerHandle) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if ServiceMessenger::is_reachable(
            client,
            CORE_NODE,
            CLIENT_INSTANCE,
            CORE_NODE,
            names::STACK_LIST,
            Some(CORE_NODE),
            None,
        )
        .await
        .expect("reachability check should succeed")
        {
            return;
        }
        if Instant::now() >= deadline {
            panic!("stack_list stub did not become reachable within 5s");
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

/// Writes a minimal `peppy.json5` into `dir` suitable for
/// `Processor::new_standalone`.
fn write_standalone_peppy_config(dir: &TempDir) -> std::path::PathBuf {
    let path = dir.path().join("peppy.json5");
    std::fs::write(
        &path,
        r#"{
            schema_version: 1,
            manifest: { name: "test_node", tag: "0.1.0" },
            execution: { language: "rust", run_cmd: ["./target/debug/test_node"] },
        }"#,
    )
    .expect("peppy config should be written");
    path
}

/// Starts a router, spawns the stub listener for `graph`, builds a
/// `NodeRunner` pointed at the router, and waits for reachability. The router
/// and temp dir are returned so callers hold them for the duration of the
/// test — dropping them tears down the messaging fabric / config file.
async fn setup_stub(
    graph: SerializedNodeGraph,
    dot_graph: &str,
) -> (ZenohdInstance, TempDir, NodeRunner) {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");
    let server = MessengerHandle::from_host_port(&router.host, router.port)
        .await
        .expect("server handle");

    let temp_dir = TempDir::new().expect("temp dir should be created");
    let peppy_config_path = write_standalone_peppy_config(&temp_dir);
    let standalone_config = StandaloneConfig::new()
        .with_messaging(&router.host, router.port)
        .with_instance_id(CLIENT_INSTANCE);
    let processor = Processor::new_standalone(&peppy_config_path, &standalone_config)
        .expect("standalone processor");
    let node_runner = NodeRunner::new(processor).await.expect("node runner");

    spawn_stub_listener(server, graph, dot_graph).await;
    wait_until_reachable(node_runner.messenger()).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_list_parses_graph_and_includes_dot_graph_when_requested() {
    let brain = SerializedNode {
        name: "brain".to_string(),
        tag: "0.1.0".to_string(),
        config_path: "/tmp/brain.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Ready),
        instances: vec![SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
        }],
        variant_name: Some("default".to_string()),
    };
    let sensor = SerializedNode {
        name: "sensor".to_string(),
        tag: "0.1.0".to_string(),
        config_path: "/tmp/sensor.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Added),
        instances: vec![],
        variant_name: None,
    };
    let graph = SerializedNodeGraph {
        nodes: vec![brain.clone(), sensor.clone()],
        edges: vec![SerializedEdge {
            from: brain,
            to: sensor,
        }],
    };

    let (_router, _temp_dir, node_runner) = setup_stub(graph.clone(), "digraph {}").await;

    let result = stack_list(&node_runner, true, Duration::from_secs(3))
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
        tag: "0.1.0".to_string(),
        config_path: "/tmp/brain.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Ready),
        instances: vec![SerializedInstance {
            instance_id: "i1".to_string(),
            state: InstanceState::Running,
        }],
        variant_name: Some("default".to_string()),
    };
    let sensor = SerializedNode {
        name: "sensor".to_string(),
        tag: "0.1.0".to_string(),
        config_path: "/tmp/sensor.json5".to_string(),
        artifact_path: None,
        stage: Some(NodeStage::Added),
        instances: vec![],
        variant_name: None,
    };
    let graph = SerializedNodeGraph {
        nodes: vec![brain.clone(), sensor.clone()],
        edges: vec![SerializedEdge {
            from: brain,
            to: sensor,
        }],
    };

    let (_router, _temp_dir, node_runner) = setup_stub(graph.clone(), "digraph {}").await;

    let result = stack_list(&node_runner, false, Duration::from_secs(3))
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
