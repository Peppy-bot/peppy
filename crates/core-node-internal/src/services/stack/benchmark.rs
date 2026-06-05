//! `stack_benchmark` action: measures the per-interface messaging latency that
//! wires each node to its direct dependencies, against the already-running
//! stack. Runs inside the core daemon (which owns the node graph in-process) and
//! acts as a messaging *client* to probe producers.
//!
//! ## No-trigger guarantee (by construction)
//! - Services / actions are measured only with `Probe`-kind queries
//!   ([`ServiceMessenger::probe_latency`] / [`ActionMessenger::probe_latency`]),
//!   which the framework auto-answers; no user handler runs and no goal is
//!   created.
//! - Real topic latency is *observe-only*: we subscribe but never publish onto a
//!   real topic key.
//! - The synthetic baseline publishes only to a reserved key
//!   ([`SYNTHETIC_BENCHMARK_TOPIC`]) that is verified not to collide with any
//!   real topic, so only the benchmark's own subscriber receives it.

use crate::Result;
use crate::names;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::gate::{Admission, ConcurrencyGate};
use config::node::{DependsOn, NodeConfig, QoSProfile};
use core_node_api::encoding::{
    BenchmarkFeedbackStep, ClockConfidence, ClockOffsetRequest, ClockOffsetResponse, InterfaceKind,
    InterfaceLatency, MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal,
    StackBenchmarkGoalResponse, StackBenchmarkResult, wall_now_ns,
};
use latency_report::stats::summarize;
use node_stack::NodeStack;
use peppylib::messaging::{
    CLOCK_OFFSET_SERVICE, ConcurrentAction, ConsumerFilter, PendingGoal, SenderTarget,
};
use peppylib::types::Payload;
use peppylib::{
    ActionMessenger, MessengerHandle, PeppyError, PeppyResult, ServiceMessenger, TopicMessenger,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::debug;

/// Gate budget: rejects a concurrent benchmark goal with a remaining-time hint.
const BENCHMARK_GATE_TIMEOUT_SECS: u64 = 1800;
/// A measured producer offset at or below this magnitude is treated as same-host
/// (the producer and the core node share a clock); the one-way number is exact.
const SAME_HOST_OFFSET_NS: u64 = 100_000;
/// A corrected one-way delta larger than this (or negative) is implausible and
/// is suppressed — it means the clocks are not adequately synchronized.
const IMPLAUSIBLE_DELIVERY_NS: i128 = 5_000_000_000;
/// Reserved topic name for the synthetic transport baseline. Double-underscore
/// sentinel; verified at runtime not to collide with any real topic.
const SYNTHETIC_BENCHMARK_TOPIC: &str = "__peppy_benchmark_synthetic__";
/// Fixed payload size for the synthetic baseline. Real payloads are opaque
/// (no runtime schema), so a representative fixed size is used; the row measures
/// the transport path for the topic's QoS, not its exact byte cost.
const SYNTHETIC_PAYLOAD_BYTES: usize = 256;
/// How long to wait for the synthetic subscription to establish before the first
/// publish (Zenoh route propagation).
const SUBSCRIBE_SETTLE: Duration = Duration::from_millis(100);

pub async fn listen_for_stack_benchmark(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::STACK_BENCHMARK_ACTION,
        true,
    )
    .await?;

    let handler = StackBenchmarkGoalHandler {
        context: BenchmarkActionContext {
            node_stack,
            messenger: messenger.clone(),
            bound_core_node: core_node_name.to_string(),
            core_instance_id: instance_id.to_string(),
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });
    Ok(handle)
}

#[derive(Clone)]
struct StackBenchmarkGoalHandler {
    context: BenchmarkActionContext,
    gate: ConcurrencyGate,
}

#[derive(Clone)]
struct BenchmarkActionContext {
    node_stack: Arc<NodeStack>,
    messenger: MessengerHandle,
    bound_core_node: String,
    core_instance_id: String,
}

fn encode_accepted() -> PeppyResult<Payload> {
    StackBenchmarkGoalResponse::accepted()
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: "stack_benchmark".to_string(),
            reason: e.to_string(),
        })
}

fn encode_rejected(reason: impl Into<String>) -> PeppyResult<Payload> {
    StackBenchmarkGoalResponse::rejected(reason)
        .encode()
        .map_err(|e| PeppyError::InvalidServiceRequest {
            identifier: "stack_benchmark".to_string(),
            reason: e.to_string(),
        })
}

impl GoalHandler for StackBenchmarkGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        let goal = match StackBenchmarkGoal::decode(pending.request_bytes()) {
            Ok(goal) => goal,
            Err(e) => {
                reject_goal(
                    pending,
                    encode_rejected(format!("invalid goal payload: {e}")),
                )
                .await;
                return;
            }
        };

        let generation = match self.gate.try_admit(BENCHMARK_GATE_TIMEOUT_SECS, false) {
            // `stack_benchmark` never forces, so nothing is ever superseded.
            Admission::Admitted { generation, .. } => generation,
            Admission::AlreadyRunning { .. } => {
                reject_goal(
                    pending,
                    encode_rejected("a stack benchmark is already in progress"),
                )
                .await;
                return;
            }
        };

        let Some(goal_ctx) = accept_goal(pending, encode_accepted()).await else {
            self.gate.clear_running();
            return;
        };

        debug!("Received `stack_benchmark` goal");

        let feedback_publisher = goal_ctx
            .feedback_publisher()
            .expect("stack_benchmark declares a feedback topic");
        let gate_for_task = self.gate.clone();
        let context = self.context.clone();

        tokio::spawn(async move {
            let slot = gate_for_task.into_slot_guard(generation);

            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<StackBenchmarkFeedback>();
            let drain = tokio::spawn(async move {
                while let Some(feedback) = rx.recv().await {
                    if let Ok(payload) = feedback.encode() {
                        let _ = feedback_publisher.publish(payload).await;
                    }
                }
            });

            let result = run_benchmark(&context, goal, &tx).await;

            // Close the feedback channel and flush before completing so the
            // end-of-stream sentinel doesn't race ahead of the final lines.
            drop(tx);
            let _ = drain.await;

            if let Ok(payload) = result.encode() {
                slot.release_then_complete(&goal_ctx, payload).await;
            }
        });
    }
}

/// A direct-dependency edge to measure: a consumer's wired interface to a
/// specific producer.
struct Edge {
    from_node: String,
    from_tag: String,
    to_node: String,
    to_tag: String,
    interface: String,
    kind: InterfaceKind,
    /// Producer-declared QoS for topic edges (used by delivery + synthetic).
    qos: QoSProfile,
}

fn emit_feedback(
    tx: &UnboundedSender<StackBenchmarkFeedback>,
    step: BenchmarkFeedbackStep,
    line: impl Into<String>,
) {
    let _ = tx.send(StackBenchmarkFeedback::stdout(line, step));
}

/// Resolve a consumed interface's `link_id` to the producer `(name, tag)` via
/// the consumer's `depends_on.nodes`.
fn resolve_dep(depends_on: Option<&DependsOn>, link_id: &str) -> Option<(String, String)> {
    let depends_on = depends_on?;
    depends_on
        .nodes
        .iter()
        .find(|d| d.link_id == link_id)
        .map(|d| (d.name.as_str().to_string(), d.tag.clone()))
}

/// Producer-declared QoS for `topic_name`, defaulting to [`QoSProfile::Standard`].
fn producer_topic_qos(producer: Option<&NodeConfig>, topic_name: &str) -> QoSProfile {
    producer
        .and_then(|c| c.interfaces.topics.as_ref())
        .and_then(|t| t.emits.as_ref())
        .and_then(|emits| emits.iter().find(|e| e.name == topic_name))
        .map(|e| e.qos_profile.clone())
        .unwrap_or_default()
}

/// Walk every node's consumed interfaces and resolve each to a producer edge.
fn enumerate_edges(configs: &[NodeConfig]) -> Vec<Edge> {
    let by_key: HashMap<(&str, &str), &NodeConfig> = configs
        .iter()
        .map(|c| ((c.manifest.name.as_str(), c.manifest.tag.as_str()), c))
        .collect();

    let mut edges = Vec::new();
    for config in configs {
        let from_node = config.manifest.name.as_str().to_string();
        let from_tag = config.manifest.tag.clone();
        let depends_on = config.manifest.depends_on.as_ref();

        if let Some(topics) = config.interfaces.topics.as_ref() {
            for c in topics.consumes.iter().flatten() {
                if let Some((to_node, to_tag)) = resolve_dep(depends_on, &c.link_id) {
                    let producer = by_key.get(&(to_node.as_str(), to_tag.as_str())).copied();
                    let qos = producer_topic_qos(producer, &c.name);
                    edges.push(Edge {
                        from_node: from_node.clone(),
                        from_tag: from_tag.clone(),
                        to_node,
                        to_tag,
                        interface: c.name.clone(),
                        kind: InterfaceKind::Topic,
                        qos,
                    });
                }
            }
        }
        if let Some(services) = config.interfaces.services.as_ref() {
            for c in services.consumes.iter().flatten() {
                if let Some((to_node, to_tag)) = resolve_dep(depends_on, &c.link_id) {
                    edges.push(Edge {
                        from_node: from_node.clone(),
                        from_tag: from_tag.clone(),
                        to_node,
                        to_tag,
                        interface: c.name.clone(),
                        kind: InterfaceKind::Service,
                        qos: QoSProfile::default(),
                    });
                }
            }
        }
        if let Some(actions) = config.interfaces.actions.as_ref() {
            for c in actions.consumes.iter().flatten() {
                if let Some((to_node, to_tag)) = resolve_dep(depends_on, &c.link_id) {
                    edges.push(Edge {
                        from_node: from_node.clone(),
                        from_tag: from_tag.clone(),
                        to_node,
                        to_tag,
                        interface: c.name.clone(),
                        kind: InterfaceKind::Action,
                        qos: QoSProfile::default(),
                    });
                }
            }
        }
    }
    edges
}

fn qos_label(qos: &QoSProfile) -> &'static str {
    match qos {
        QoSProfile::SensorData => "sensor_data",
        QoSProfile::Standard => "standard",
        QoSProfile::Reliable => "reliable",
        QoSProfile::Critical => "critical",
    }
}

/// The benchmark executor. Returns a result even on partial failure; per-edge
/// problems are encoded as notes on the rows rather than aborting the run.
async fn run_benchmark(
    ctx: &BenchmarkActionContext,
    goal: StackBenchmarkGoal,
    tx: &UnboundedSender<StackBenchmarkFeedback>,
) -> StackBenchmarkResult {
    let timeout = Duration::from_millis(goal.per_sample_timeout_ms);
    let warmup = goal.warmup;
    let samples = goal.samples;

    let configs: Vec<NodeConfig> = ctx
        .node_stack
        .snapshot()
        .iter()
        .map(|h| h.read().config().clone())
        .collect();
    let edges = enumerate_edges(&configs);

    let topics = edges
        .iter()
        .filter(|e| e.kind == InterfaceKind::Topic)
        .count();
    let services = edges
        .iter()
        .filter(|e| e.kind == InterfaceKind::Service)
        .count();
    let actions = edges
        .iter()
        .filter(|e| e.kind == InterfaceKind::Action)
        .count();
    emit_feedback(
        tx,
        BenchmarkFeedbackStep::Enumerating,
        format!(
            "Found {} interface edge(s): {topics} topic(s), {services} service(s), {actions} action(s)",
            edges.len()
        ),
    );

    let mut rows: Vec<InterfaceLatency> = Vec::new();

    for edge in &edges {
        match edge.kind {
            InterfaceKind::Service | InterfaceKind::Action => {
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::Probing,
                    format!("Probing {}", edge_label(edge)),
                );
                rows.push(measure_probe(ctx, edge, warmup, samples, timeout).await);
            }
            InterfaceKind::Topic => {
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::TopicDelivery,
                    format!("Measuring delivery {}", edge_label(edge)),
                );
                rows.push(measure_topic_delivery(ctx, edge, warmup, samples, timeout).await);
            }
        }
    }

    if goal.include_synthetic_baseline
        && let Some(synthetic) =
            synthetic_rows(ctx, &configs, &edges, warmup, samples, timeout, tx).await
    {
        rows.extend(synthetic);
    }

    emit_feedback(
        tx,
        BenchmarkFeedbackStep::Aggregating,
        format!("Aggregated {} row(s)", rows.len()),
    );
    StackBenchmarkResult::success(rows)
}

fn edge_label(edge: &Edge) -> String {
    format!(
        "{}:{} → {}:{}/{}",
        edge.from_node, edge.from_tag, edge.to_node, edge.to_tag, edge.interface
    )
}

fn row_from_samples(
    edge: &Edge,
    measurement: MeasurementKind,
    clock_confidence: ClockConfidence,
    samples_ns: Vec<u64>,
    note: Option<String>,
) -> InterfaceLatency {
    let summary = summarize(&samples_ns);
    InterfaceLatency {
        from_node: edge.from_node.clone(),
        from_tag: edge.from_tag.clone(),
        to_node: edge.to_node.clone(),
        to_tag: edge.to_tag.clone(),
        interface_name: edge.interface.clone(),
        kind: edge.kind,
        measurement,
        clock_confidence,
        p50_ns: summary.p50_ns,
        p90_ns: summary.p90_ns,
        mean_ns: summary.mean_ns,
        count: summary.count,
        samples_ns,
        note,
    }
}

/// Timed `Probe` round-trips to a service or an action's goal service. The user
/// handler never runs; the measurement is clock-independent.
async fn measure_probe(
    ctx: &BenchmarkActionContext,
    edge: &Edge,
    warmup: u32,
    samples: u32,
    timeout: Duration,
) -> InterfaceLatency {
    let measurement = match edge.kind {
        InterfaceKind::Action => MeasurementKind::ActionProbe,
        _ => MeasurementKind::ServiceProbe,
    };
    let target = match SenderTarget::node(&edge.to_node, &edge.to_tag) {
        Ok(t) => t,
        Err(e) => {
            return row_from_samples(
                edge,
                measurement,
                ClockConfidence::NotApplicable,
                Vec::new(),
                Some(format!("invalid target: {e}")),
            );
        }
    };

    let total = warmup.saturating_add(samples);
    let mut out = Vec::new();
    let mut any_success = false;
    let mut consecutive_errors: u32 = 0;
    for i in 0..total {
        let result = match edge.kind {
            InterfaceKind::Action => {
                ActionMessenger::probe_latency(
                    &ctx.messenger,
                    &ctx.bound_core_node,
                    &ctx.core_instance_id,
                    target.clone(),
                    &edge.interface,
                    Some(&ctx.bound_core_node),
                    None,
                    timeout,
                )
                .await
            }
            _ => {
                ServiceMessenger::probe_latency(
                    &ctx.messenger,
                    &ctx.bound_core_node,
                    &ctx.core_instance_id,
                    target.clone(),
                    &edge.interface,
                    Some(&ctx.bound_core_node),
                    None,
                    timeout,
                )
                .await
            }
        };
        match result {
            Ok(d) => {
                any_success = true;
                consecutive_errors = 0;
                if i >= warmup {
                    out.push(d.as_nanos() as u64);
                }
            }
            Err(_) => {
                consecutive_errors += 1;
                // Bail on a dead edge instead of spending the whole sample budget
                // waiting out the per-sample timeout on every probe.
                if !any_success && consecutive_errors >= 3 {
                    break;
                }
            }
        }
    }

    let note = if out.is_empty() {
        Some("unreachable (no producer instance responded)".to_string())
    } else {
        None
    };
    row_from_samples(edge, measurement, ClockConfidence::NotApplicable, out, note)
}

/// Poll a producer's `clock_offset` service to get its measured offset to the
/// core node, used to normalize cross-host topic timestamps.
async fn poll_producer_offset(
    ctx: &BenchmarkActionContext,
    to_node: &str,
    to_tag: &str,
    timeout: Duration,
) -> Option<(i64, u64)> {
    let request = ClockOffsetRequest::new().encode().ok()?;
    let target = SenderTarget::node(to_node, to_tag).ok()?;
    let reply = ServiceMessenger::poll(
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        target,
        CLOCK_OFFSET_SERVICE,
        Some(&ctx.bound_core_node),
        None,
        request,
        timeout,
    )
    .await
    .ok()?;
    let decoded = ClockOffsetResponse::decode(reply.payload().as_ref()).ok()?;
    Some((decoded.offset_ns, decoded.round_trip_delay_ns))
}

fn classify_clock(
    offset: Option<(i64, u64)>,
    had_implausible: bool,
) -> (ClockConfidence, Option<String>) {
    if had_implausible {
        return (
            ClockConfidence::CrossHostFlagged,
            Some(
                "some deltas were negative or implausibly large (cross-host clock skew); \
                 deploy PTP or NTP and rely on the probe/synthetic numbers — see the guide"
                    .to_string(),
            ),
        );
    }
    match offset {
        None => (
            ClockConfidence::SameHost,
            Some("producer clock offset unavailable; treated as same-host".to_string()),
        ),
        Some((o, _)) if o.unsigned_abs() <= SAME_HOST_OFFSET_NS => {
            (ClockConfidence::SameHost, None)
        }
        Some(_) => (ClockConfidence::CrossHostCorrected, None),
    }
}

/// Observe-only real delivery latency: subscribe to the producer's topic and
/// compute `receive − source − producer_offset`. Never publishes.
async fn measure_topic_delivery(
    ctx: &BenchmarkActionContext,
    edge: &Edge,
    warmup: u32,
    samples: u32,
    per_sample_timeout: Duration,
) -> InterfaceLatency {
    let offset = poll_producer_offset(ctx, &edge.to_node, &edge.to_tag, per_sample_timeout).await;

    let target = match SenderTarget::node(&edge.to_node, &edge.to_tag) {
        Ok(t) => t,
        Err(e) => {
            return row_from_samples(
                edge,
                MeasurementKind::TopicDelivery,
                ClockConfidence::NotApplicable,
                Vec::new(),
                Some(format!("invalid target: {e}")),
            );
        }
    };

    let mut subscription = match TopicMessenger::subscribe(
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        Some(target),
        false,
        &edge.interface,
        Some(&ctx.bound_core_node),
        &ConsumerFilter::Any,
        edge.qos.clone(),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => {
            return row_from_samples(
                edge,
                MeasurementKind::TopicDelivery,
                ClockConfidence::NotApplicable,
                Vec::new(),
                Some(format!("subscribe failed: {e}")),
            );
        }
    };

    let off = offset.map(|(o, _)| o).unwrap_or(0) as i128;
    let mut seen: u32 = 0;
    let mut measured: u32 = 0;
    let mut out = Vec::new();
    let mut had_implausible = false;
    let mut had_missing_ts = false;

    loop {
        if measured >= samples {
            break;
        }
        match tokio::time::timeout(per_sample_timeout, subscription.on_next_message()).await {
            Ok(Some(msg)) => {
                let Some(src) = msg.source_timestamp_nanos() else {
                    had_missing_ts = true;
                    continue;
                };
                seen += 1;
                if seen <= warmup {
                    continue;
                }
                let recv = wall_now_ns().unwrap_or(0);
                let corrected = recv as i128 - src as i128 - off;
                measured += 1;
                if (0..=IMPLAUSIBLE_DELIVERY_NS).contains(&corrected) {
                    out.push(corrected as u64);
                } else {
                    had_implausible = true;
                }
            }
            // Channel closed or no traffic within the window — stop observing.
            Ok(None) | Err(_) => break,
        }
    }

    let (confidence, mut note) = classify_clock(offset, had_implausible);
    if out.is_empty() && !had_implausible {
        note = Some(if had_missing_ts {
            "no source timestamps on samples (timestamping disabled?)".to_string()
        } else {
            "no live traffic observed within the timeout".to_string()
        });
    }
    row_from_samples(edge, MeasurementKind::TopicDelivery, confidence, out, note)
}

/// Synthetic transport baseline: for each distinct QoS in use, time
/// publish→own-receive on the reserved key, then attach a synthetic row to each
/// topic edge of that QoS. Clock-independent (single-clock round-trip).
async fn synthetic_rows(
    ctx: &BenchmarkActionContext,
    configs: &[NodeConfig],
    edges: &[Edge],
    warmup: u32,
    samples: u32,
    per_sample_timeout: Duration,
    tx: &UnboundedSender<StackBenchmarkFeedback>,
) -> Option<Vec<InterfaceLatency>> {
    // Safety: the reserved key must not collide with any real topic a node emits
    // or consumes.
    if topic_name_in_use(configs, SYNTHETIC_BENCHMARK_TOPIC) {
        emit_feedback(
            tx,
            BenchmarkFeedbackStep::Synthetic,
            format!(
                "Skipping synthetic baseline: reserved topic `{SYNTHETIC_BENCHMARK_TOPIC}` is in use"
            ),
        );
        return None;
    }

    // Measure once per distinct QoS profile among topic edges.
    let mut by_qos: HashMap<&'static str, Vec<u64>> = HashMap::new();
    for edge in edges.iter().filter(|e| e.kind == InterfaceKind::Topic) {
        let label = qos_label(&edge.qos);
        if by_qos.contains_key(label) {
            continue;
        }
        emit_feedback(
            tx,
            BenchmarkFeedbackStep::Synthetic,
            format!("Synthetic transport baseline ({label})"),
        );
        let samples_ns =
            measure_synthetic(ctx, &edge.qos, warmup, samples, per_sample_timeout).await;
        by_qos.insert(label, samples_ns);
    }

    let mut rows = Vec::new();
    for edge in edges.iter().filter(|e| e.kind == InterfaceKind::Topic) {
        let label = qos_label(&edge.qos);
        let samples_ns = by_qos.get(label).cloned().unwrap_or_default();
        let note = if samples_ns.is_empty() {
            Some("synthetic baseline produced no samples".to_string())
        } else {
            Some(format!(
                "{SYNTHETIC_PAYLOAD_BYTES}B fixed payload, {label} QoS"
            ))
        };
        rows.push(row_from_samples(
            edge,
            MeasurementKind::TopicSynthetic,
            ClockConfidence::NotApplicable,
            samples_ns,
            note,
        ));
    }
    Some(rows)
}

fn topic_name_in_use(configs: &[NodeConfig], name: &str) -> bool {
    configs.iter().any(|c| {
        c.interfaces.topics.as_ref().is_some_and(|t| {
            t.emits.iter().flatten().any(|e| e.name == name)
                || t.consumes.iter().flatten().any(|c| c.name == name)
        })
    })
}

async fn measure_synthetic(
    ctx: &BenchmarkActionContext,
    qos: &QoSProfile,
    warmup: u32,
    samples: u32,
    per_sample_timeout: Duration,
) -> Vec<u64> {
    let publisher_target = match SenderTarget::node(&ctx.bound_core_node, names::CORE_NODE_TAG) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };
    let subscribe_target = match SenderTarget::node(&ctx.bound_core_node, names::CORE_NODE_TAG) {
        Ok(t) => t,
        Err(_) => return Vec::new(),
    };

    let mut subscription = match TopicMessenger::subscribe(
        &ctx.messenger,
        &ctx.bound_core_node,
        &ctx.core_instance_id,
        Some(subscribe_target),
        false,
        SYNTHETIC_BENCHMARK_TOPIC,
        Some(&ctx.bound_core_node),
        &ConsumerFilter::Any,
        qos.clone(),
    )
    .await
    {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };

    // Let the subscription route establish before the first publish.
    tokio::time::sleep(SUBSCRIBE_SETTLE).await;

    let payload_bytes = vec![0u8; SYNTHETIC_PAYLOAD_BYTES];
    let total = warmup.saturating_add(samples);
    let mut out = Vec::new();
    let mut received_any = false;
    for i in 0..total {
        let start = Instant::now();
        if TopicMessenger::emit(
            &ctx.messenger,
            &ctx.bound_core_node,
            &ctx.core_instance_id,
            publisher_target.clone(),
            SYNTHETIC_BENCHMARK_TOPIC,
            qos.clone(),
            Payload::from(payload_bytes.clone()),
        )
        .await
        .is_err()
        {
            continue;
        }
        match tokio::time::timeout(per_sample_timeout, subscription.on_next_message()).await {
            Ok(Some(_)) => {
                received_any = true;
                if i >= warmup {
                    out.push(start.elapsed().as_nanos() as u64);
                }
            }
            // Dropped (e.g. SensorData) or timed out — skip this sample, but bail
            // early if the self-subscription never receives anything.
            Ok(None) | Err(_) => {
                if !received_any && i >= 5 {
                    break;
                }
            }
        }
    }
    out
}
