mod common;

use std::ops::ControlFlow;
use std::time::Duration;

use common::{
    FeedbackDrainOutcome, drain_feedback, fetch_end, goal_answer, send_daemon_goal,
    start_core_node_with_mock_messenger,
};
use config::node::NodeConfigParser;
use core_node_api::encoding::{InterfaceKind, MeasurementKind};
use core_node_api::encoding::{
    StackBenchmarkFeedback, StackBenchmarkGoal, StackBenchmarkGoalResponse, StackBenchmarkResult,
};

/// Drives the `stack_benchmark` action end-to-end against the in-process daemon
/// and returns the decoded result of the completed goal plus the count of
/// feedback messages received. Panics when the goal does not complete.
async fn run_benchmark_goal(
    started: &common::StartedCoreNode,
    goal: StackBenchmarkGoal,
) -> (StackBenchmarkResult, usize) {
    let mut action_handle = send_daemon_goal(
        &started.caller_handle,
        &started.core_node_name,
        &goal,
        Duration::from_secs(5),
    )
    .await
    .expect("send stack_benchmark goal");

    goal_answer(
        &action_handle,
        StackBenchmarkGoalResponse::decode,
        "goal response",
    )
    .expect("decode goal response")
    .expect("the daemon accepts the benchmark goal");

    // Drain feedback to end-of-stream with a generous overall deadline.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    let mut feedback_count = 0usize;
    let drained = drain_feedback(
        &mut action_handle,
        |payload| StackBenchmarkFeedback::decode(payload).expect("decode feedback"),
        |_feedback| {
            feedback_count += 1;
            ControlFlow::Continue(())
        },
        deadline,
    )
    .await;
    if matches!(drained, FeedbackDrainOutcome::TimedOut) {
        panic!("timed out draining benchmark feedback");
    }

    let result = fetch_end(
        &started.caller_handle,
        &action_handle,
        Duration::from_secs(10),
        StackBenchmarkResult::decode,
        "benchmark result",
    )
    .await
    .unwrap_or_else(|err| panic!("{err}"))
    .completed();
    (result, feedback_count)
}

/// An empty stack (only the core node, which consumes nothing) completes with
/// an empty result, and does not hang.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_empty_stack_completes() {
    let started = start_core_node_with_mock_messenger().await;

    let goal = StackBenchmarkGoal::new(2, 0, 200);
    let (result, feedback_count) = run_benchmark_goal(&started, goal).await;

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

/// Two benchmark goals sent one after the other both complete: the
/// single-goal gate is released when a run completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_runs_repeatedly() {
    let started = start_core_node_with_mock_messenger().await;

    for _ in 0..2 {
        let goal = StackBenchmarkGoal::new(1, 0, 200);
        let (result, _) = run_benchmark_goal(&started, goal).await;
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
/// samples and an `unreachable` note, but the edge is enumerated and probed
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

/// A consumer that wires `bench_svc` of `bench_provider` through two links.
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

/// A consumer that wires the *same* interface from the *same* producer via two
/// distinct `depends_on` links produces two separate rows, identical in
/// producer/interface but differing only by `link_id`, which the benchmark
/// carries end to end so the two rows can be told apart.
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

fn action_and_topic_provider_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: { name: "bench_provider", tag: "v1" },
        interfaces: {
            topics: { emits: [{ name: "bench_topic" }] },
            actions: { exposes: [{ name: "bench_act" }] }
        },
        execution: { language: "rust", run_cmd: ["sleep", "10"] }
    }"#
}

fn action_and_topic_consumer_config() -> &'static str {
    r#"{
        peppy_schema: "node/v1",
        manifest: {
            name: "bench_consumer",
            tag: "v1",
            depends_on: { nodes: [{ name: "bench_provider", tag: "v1", link_id: "prov" }] }
        },
        interfaces: {
            topics: { consumes: [{ link_id: "prov", name: "bench_topic" }] },
            actions: { consumes: [{ link_id: "prov", name: "bench_act" }] }
        },
        execution: { language: "rust", run_cmd: ["sleep", "10"] }
    }"#
}

/// Each edge kind is measured the way its kind calls for, in edge order: a
/// topic edge yields a probe of its producer node followed by the delivery
/// measurement, and an action edge yields one action probe. No producer
/// instance runs, so every row is empty, but the measurement each row names
/// does not depend on a reply.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stack_benchmark_measures_each_edge_kind_its_own_way() {
    let started = start_core_node_with_mock_messenger().await;

    let provider = NodeConfigParser::from_content(action_and_topic_provider_config())
        .expect("provider parses");
    started
        .node_stack
        .push_config(provider, false, "/tmp/bench_provider")
        .expect("push provider");
    let consumer = NodeConfigParser::from_content(action_and_topic_consumer_config())
        .expect("consumer parses");
    started
        .node_stack
        .push_config(consumer, false, "/tmp/bench_consumer")
        .expect("push consumer");

    let goal = StackBenchmarkGoal::new(2, 0, 200);
    let (result, _) = run_benchmark_goal(&started, goal).await;

    let measured: Vec<(&str, InterfaceKind, MeasurementKind)> = result
        .rows
        .iter()
        .map(|row| (row.interface_name.as_str(), row.kind, row.measurement))
        .collect();
    assert_eq!(
        measured,
        [
            (
                "bench_topic",
                InterfaceKind::Topic,
                MeasurementKind::NodeProbe
            ),
            (
                "bench_topic",
                InterfaceKind::Topic,
                MeasurementKind::TopicDelivery
            ),
            (
                "bench_act",
                InterfaceKind::Action,
                MeasurementKind::ActionProbe
            ),
        ]
    );
}
