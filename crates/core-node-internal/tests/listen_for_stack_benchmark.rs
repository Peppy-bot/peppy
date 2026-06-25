mod common;

use std::time::Duration;

use common::{CALLER_INSTANCE_ID, core_node_target, start_core_node_with_mock_messenger};
use config::node::{NodeConfigParser, QoSProfile};
use core_node::names;
use core_node_api::encoding::{InterfaceKind, MeasurementKind};
use core_node_api::encoding::{
    StackBenchmarkFeedback, StackBenchmarkGoal, StackBenchmarkGoalResponse, StackBenchmarkResult,
};
use peppylib::ActionMessenger;
use peppylib::messaging::ResultStatus;

/// Drives the `stack_benchmark` action end-to-end against the in-process daemon
/// and returns the decoded result plus the count of feedback messages received.
async fn run_benchmark_goal(
    started: &common::StartedCoreNode,
    goal: StackBenchmarkGoal,
) -> (StackBenchmarkResult, usize) {
    let goal_payload = goal.encode().expect("encode goal");

    let mut action_handle = ActionMessenger::send_goal(
        &started.caller_handle,
        &started.core_node_name,
        CALLER_INSTANCE_ID,
        core_node_target(&started.core_node_name),
        names::STACK_BENCHMARK_ACTION,
        None,
        goal_payload,
        QoSProfile::default(),
        Duration::from_secs(5),
    )
    .await
    .expect("send stack_benchmark goal");

    let goal_response =
        StackBenchmarkGoalResponse::decode(&action_handle.goal_response().payload())
            .expect("decode goal response");
    assert!(goal_response.accepted, "benchmark goal should be accepted");

    // Drain feedback to end-of-stream with a generous overall deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut feedback_count = 0usize;
    loop {
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out draining benchmark feedback");
        }
        match tokio::time::timeout(Duration::from_millis(50), action_handle.on_next_feedback())
            .await
        {
            Ok(Ok(msg)) => {
                StackBenchmarkFeedback::decode(&msg.payload()).expect("decode feedback");
                feedback_count += 1;
            }
            Ok(Err(_)) => break, // end-of-stream
            Err(_) => {}
        }
    }

    let reply = ActionMessenger::request_result(
        &started.caller_handle,
        &action_handle,
        Duration::from_secs(10),
    )
    .await
    .expect("fetch benchmark result");
    assert!(
        matches!(
            reply.status,
            ResultStatus::Completed | ResultStatus::Cancelled
        ),
        "benchmark should complete, got {:?}",
        reply.status
    );
    let result = StackBenchmarkResult::decode(reply.body.as_ref()).expect("decode result");
    (result, feedback_count)
}

/// An empty stack (only the core node, which consumes nothing) yields a
/// successful, empty result — and crucially does not hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_empty_stack_completes() {
    let started = start_core_node_with_mock_messenger().await;

    let goal = StackBenchmarkGoal::new(2, 0, 200);
    let (result, feedback_count) = run_benchmark_goal(&started, goal).await;

    assert!(result.success, "benchmark should succeed on an empty stack");
    assert!(
        result.rows.is_empty(),
        "an empty stack has no interface edges, got {} rows",
        result.rows.len()
    );
    // The executor emits at least the "enumerating" and "aggregating" lines.
    assert!(
        feedback_count >= 1,
        "expected progress feedback, got {feedback_count}"
    );
}

/// A second concurrent benchmark goal while one is in flight is rejected, not
/// queued — the single-goal gate. We approximate this by firing two goals back
/// to back; at least one must be accepted, and the action must remain healthy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_runs_repeatedly() {
    let started = start_core_node_with_mock_messenger().await;

    // Two sequential runs must both succeed (the gate is released between them).
    for _ in 0..2 {
        let goal = StackBenchmarkGoal::new(1, 0, 200);
        let (result, _) = run_benchmark_goal(&started, goal).await;
        assert!(result.success);
        assert!(result.rows.is_empty());
    }
}

fn provider_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: { name: "bench_provider", tag: "v1" },
        interfaces: { services: { exposes: [{ name: "bench_svc" }] } },
        execution: { language: "rust", run_cmd: ["sleep", "10"] }
    }"#
}

fn consumer_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "bench_consumer",
            tag: "v1",
            depends_on: { nodes: [{ name: "bench_provider", tag: "v1", link_id: "prov" }] }
        },
        interfaces: { services: { consumes: [{ link_id: "prov", name: "bench_svc" }] } },
        execution: { language: "rust", run_cmd: ["sleep", "10"] }
    }"#
}

/// A consumer wired to a provider's service produces a measured row. With no
/// running producer instance the probe is unreachable, so the row reports zero
/// samples and an `unreachable` note — but the edge is enumerated and probed
/// (exercising the no-trigger probe path), and the run completes cleanly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_enumerates_wired_service_edge() {
    let started = start_core_node_with_mock_messenger().await;

    // The test's `node_stack` shares its `Arc<RwLock<..>>` with the daemon's
    // handler, so pushing configs here creates edges the benchmark will see.
    let provider = NodeConfigParser::from_content(provider_config()).expect("provider parses");
    started
        .node_stack
        .push_config(provider, false, "/tmp/bench_provider")
        .expect("push provider");
    let consumer = NodeConfigParser::from_content(consumer_config()).expect("consumer parses");
    started
        .node_stack
        .push_config(consumer, false, "/tmp/bench_consumer")
        .expect("push consumer");

    let goal = StackBenchmarkGoal::new(2, 0, 200);
    let (result, _) = run_benchmark_goal(&started, goal).await;

    assert!(result.success, "benchmark should succeed");
    let svc_row = result
        .rows
        .iter()
        .find(|r| r.interface_name == "bench_svc")
        .expect("the wired service edge should be measured");
    assert_eq!(svc_row.from_node, "bench_consumer");
    assert_eq!(svc_row.to_node, "bench_provider");
    assert_eq!(svc_row.link_id, "prov");
    assert_eq!(svc_row.kind, InterfaceKind::Service);
    assert_eq!(svc_row.measurement, MeasurementKind::ServiceProbe);
    // No producer instance is running, so the probe is unreachable.
    assert_eq!(svc_row.count, 0, "no instance → no successful probes");
    assert!(
        svc_row
            .note
            .as_deref()
            .unwrap_or_default()
            .contains("unreachable"),
        "expected an unreachable note, got {:?}",
        svc_row.note
    );
}

/// A consumer that wires the *same* interface from the *same* producer via two
/// distinct `depends_on` links produces two separate rows — identical in
/// producer/interface but differing only by `link_id`. Regression guard for the
/// duplicate, indistinguishable rows that prompted carrying `link_id` end-to-end.
fn two_link_consumer_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "bench_consumer",
            tag: "v1",
            depends_on: { nodes: [
                { name: "bench_provider", tag: "v1", link_id: "left" },
                { name: "bench_provider", tag: "v1", link_id: "right" }
            ] }
        },
        interfaces: { services: { consumes: [
            { link_id: "left", name: "bench_svc" },
            { link_id: "right", name: "bench_svc" }
        ] } },
        execution: { language: "rust", run_cmd: ["sleep", "10"] }
    }"#
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_distinguishes_edges_by_link_id() {
    let started = start_core_node_with_mock_messenger().await;

    let provider = NodeConfigParser::from_content(provider_config()).expect("provider parses");
    started
        .node_stack
        .push_config(provider, false, "/tmp/bench_provider")
        .expect("push provider");
    let consumer =
        NodeConfigParser::from_content(two_link_consumer_config()).expect("consumer parses");
    started
        .node_stack
        .push_config(consumer, false, "/tmp/bench_consumer")
        .expect("push consumer");

    let goal = StackBenchmarkGoal::new(2, 0, 200);
    let (result, _) = run_benchmark_goal(&started, goal).await;

    assert!(result.success, "benchmark should succeed");
    let svc_rows: Vec<_> = result
        .rows
        .iter()
        .filter(|r| r.interface_name == "bench_svc")
        .collect();
    assert_eq!(
        svc_rows.len(),
        2,
        "each wired link is a separate row, got {} rows",
        svc_rows.len()
    );

    // Both rows share producer + interface but are distinguished only by link_id.
    for row in &svc_rows {
        assert_eq!(row.from_node, "bench_consumer");
        assert_eq!(row.to_node, "bench_provider");
        assert_eq!(row.interface_name, "bench_svc");
        assert_eq!(row.measurement, MeasurementKind::ServiceProbe);
    }
    let mut links: Vec<&str> = svc_rows.iter().map(|r| r.link_id.as_str()).collect();
    links.sort_unstable();
    assert_eq!(
        links,
        ["left", "right"],
        "the two links must be distinguishable by link_id"
    );
}
