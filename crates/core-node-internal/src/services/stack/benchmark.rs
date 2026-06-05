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
use crate::services::node::resolve_interface_doc;
use config::consts::PeppyDirs;
use config::node::{DependsOn, NodeConfig, QoSProfile, node_conforms_to};
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
    peppy_dirs: PeppyDirs,
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
            peppy_dirs,
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
    /// Interface cache root, used to resolve a conformed topic's QoS from its
    /// interface contract (a conformed producer has no native `emits` to read).
    peppy_dirs: PeppyDirs,
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

/// A dependency edge to measure: a consumer's wired interface to a specific
/// producer.
struct Edge {
    from_node: String,
    from_tag: String,
    to_node: String,
    to_tag: String,
    interface: String,
    /// The consumer's `depends_on` link this interface was wired through. Two
    /// edges can share producer + interface but differ only by this link.
    link_id: String,
    /// `Some((iface_name, iface_tag))` when this edge is resolved through
    /// interface conformance — the producer emits/serves the artifact under the
    /// interface-keyed wire path, so measurement must target the interface, not
    /// the node. `None` for a direct `depends_on.nodes` edge.
    origin: Option<(String, String)>,
    kind: InterfaceKind,
    /// Producer-declared QoS for topic edges (used by delivery + synthetic).
    qos: QoSProfile,
}

impl Edge {
    /// The wire target to probe/subscribe this edge through. Conformed artifacts
    /// ride the interface-keyed path (`SenderTarget::interface`); native ones the
    /// node-keyed path (`SenderTarget::node`). Using the wrong one silently never
    /// matches on the wire.
    fn target(&self) -> std::result::Result<SenderTarget, peppylib::messaging::SenderTargetError> {
        match &self.origin {
            Some((name, tag)) => SenderTarget::interface(name, tag),
            None => SenderTarget::node(&self.to_node, &self.to_tag),
        }
    }

    /// `Some("name:tag")` of the interface this edge routes through, for the row.
    fn via_interface(&self) -> Option<String> {
        self.origin
            .as_ref()
            .map(|(name, tag)| format!("{name}:{tag}"))
    }
}

fn emit_feedback(
    tx: &UnboundedSender<StackBenchmarkFeedback>,
    step: BenchmarkFeedbackStep,
    line: impl Into<String>,
) {
    let _ = tx.send(StackBenchmarkFeedback::stdout(line, step));
}

/// A producer a consumed `link_id` resolves to, with the wire origin.
struct ResolvedProducer {
    name: String,
    tag: String,
    /// `Some((iface_name, iface_tag))` for an interface-conformance edge; `None`
    /// for a direct node dependency.
    origin: Option<(String, String)>,
}

/// Resolve a consumed interface's `link_id` to its producer(s):
/// - a `depends_on.nodes` entry resolves to exactly that node (`origin = None`);
/// - a `depends_on.interfaces` entry resolves to **every** config in `configs`
///   that `conforms_to` the interface (`origin = Some`), since any of them can
///   satisfy the dependency.
fn resolve_link(
    depends_on: Option<&DependsOn>,
    link_id: &str,
    configs: &[NodeConfig],
) -> Vec<ResolvedProducer> {
    let Some(depends_on) = depends_on else {
        return Vec::new();
    };

    if let Some(d) = depends_on.nodes.iter().find(|d| d.link_id == link_id) {
        return vec![ResolvedProducer {
            name: d.name.as_str().to_string(),
            tag: d.tag.clone(),
            origin: None,
        }];
    }

    if let Some(dep) = depends_on.interfaces.iter().find(|d| d.link_id == link_id) {
        let iface_name = dep.name.as_str();
        let iface_tag = dep.tag.as_str();
        return configs
            .iter()
            .filter(|c| node_conforms_to(c, iface_name, iface_tag))
            .map(|c| ResolvedProducer {
                name: c.manifest.name.as_str().to_string(),
                tag: c.manifest.tag.clone(),
                origin: Some((iface_name.to_string(), iface_tag.to_string())),
            })
            .collect();
    }

    Vec::new()
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

/// Walk every node's consumed interfaces and resolve each to one edge per
/// producer. A direct node dep yields one edge; an interface dep yields one per
/// conforming producer. Topic QoS for interface-conformance edges is left at the
/// default here and resolved from the interface contract by
/// [`resolve_conformed_topic_qos`] (the conformed producer has no native `emits`
/// to read it from).
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

        let push_edges = |name: &str, link_id: &str, kind: InterfaceKind, edges: &mut Vec<Edge>| {
            for producer in resolve_link(depends_on, link_id, configs) {
                let qos = if kind == InterfaceKind::Topic && producer.origin.is_none() {
                    let p = by_key
                        .get(&(producer.name.as_str(), producer.tag.as_str()))
                        .copied();
                    producer_topic_qos(p, name)
                } else {
                    QoSProfile::default()
                };
                edges.push(Edge {
                    from_node: from_node.clone(),
                    from_tag: from_tag.clone(),
                    to_node: producer.name,
                    to_tag: producer.tag,
                    interface: name.to_string(),
                    link_id: link_id.to_string(),
                    origin: producer.origin,
                    kind,
                    qos,
                });
            }
        };

        if let Some(topics) = config.interfaces.topics.as_ref() {
            for c in topics.consumes.iter().flatten() {
                push_edges(&c.name, &c.link_id, InterfaceKind::Topic, &mut edges);
            }
        }
        if let Some(services) = config.interfaces.services.as_ref() {
            for c in services.consumes.iter().flatten() {
                push_edges(&c.name, &c.link_id, InterfaceKind::Service, &mut edges);
            }
        }
        if let Some(actions) = config.interfaces.actions.as_ref() {
            for c in actions.consumes.iter().flatten() {
                push_edges(&c.name, &c.link_id, InterfaceKind::Action, &mut edges);
            }
        }
    }
    edges
}

/// Fill in topic QoS for interface-conformance edges from the interface
/// contract. The conformed producer declares no native `emits`, so the QoS lives
/// in the `(iface_name, iface_tag)` contract. On any cache miss / parse failure
/// the edge keeps the default QoS and is left to measure on a best-effort basis
/// rather than aborting the benchmark.
fn resolve_conformed_topic_qos(
    edges: &mut [Edge],
    peppy_dirs: &PeppyDirs,
    tx: &UnboundedSender<StackBenchmarkFeedback>,
) {
    let mut cache: HashMap<(String, String), Option<config::interface::PeppyInterface>> =
        HashMap::new();
    for edge in edges.iter_mut() {
        if edge.kind != InterfaceKind::Topic {
            continue;
        }
        let Some((iface_name, iface_tag)) = edge.origin.clone() else {
            continue;
        };
        let doc = cache
            .entry((iface_name.clone(), iface_tag.clone()))
            .or_insert_with(|| {
                match resolve_interface_doc(peppy_dirs, &iface_name, &iface_tag, None, &|_| {}) {
                    Ok(doc) => Some(doc),
                    Err(e) => {
                        emit_feedback(
                            tx,
                            BenchmarkFeedbackStep::Enumerating,
                            format!(
                                "Could not resolve QoS for `{iface_name}:{iface_tag}` \
                                 (defaulting): {e}"
                            ),
                        );
                        None
                    }
                }
            });
        if let Some(doc) = doc
            && let Some(topic) = doc
                .interfaces
                .topics
                .iter()
                .find(|t| t.name == edge.interface)
        {
            edge.qos = topic.qos_profile.clone();
        }
    }
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
    let mut edges = enumerate_edges(&configs);
    resolve_conformed_topic_qos(&mut edges, &ctx.peppy_dirs, tx);

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
    let arrow = if edge.origin.is_some() { "➔" } else { "→" };
    format!(
        "{}:{} {arrow} {}:{}/{} (binding: {})",
        edge.from_node, edge.from_tag, edge.to_node, edge.to_tag, edge.interface, edge.link_id
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
        link_id: edge.link_id.clone(),
        via_interface: edge.via_interface(),
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
    let target = match edge.target() {
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

    let target = match edge.target() {
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

    let payload = Payload::from(vec![0u8; SYNTHETIC_PAYLOAD_BYTES]);
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
            payload.clone(),
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

#[cfg(test)]
mod tests {
    use super::*;
    use config::node::NodeConfigParser;

    fn parse(content: &str) -> NodeConfig {
        NodeConfigParser::from_content(content).expect("parse node config")
    }

    /// Consumer depending on the `uvc_camera:v1` interface (topic + service) and
    /// on a concrete `arm:v1` node (action).
    fn consumer() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node_v1",
                manifest: {
                    name: "brain", tag: "v1",
                    depends_on: {
                        interfaces: [ { name: "uvc_camera", tag: "v1", link_id: "camera" } ],
                        nodes: [ { name: "arm", tag: "v1", link_id: "robot_controller" } ]
                    }
                },
                execution: { language: "rust", run_cmd: ["brain"] },
                interfaces: {
                    topics: { consumes: [ { link_id: "camera", name: "video_stream" } ] },
                    services: { consumes: [ { link_id: "camera", name: "video_stream_info" } ] },
                    actions: { consumes: [ { link_id: "robot_controller", name: "move_arm" } ] }
                }
            }"#,
        )
    }

    fn camera_mock() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node_v1",
                manifest: { name: "uvc_camera_python_mock", tag: "v1" },
                execution: { language: "rust", run_cmd: ["camera"] },
                interfaces: { conforms_to: [ { name: "uvc_camera", tag: "v1" } ] }
            }"#,
        )
    }

    fn arm() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node_v1",
                manifest: { name: "arm", tag: "v1" },
                execution: { language: "rust", run_cmd: ["arm"] },
                interfaces: { actions: { exposes: [ { name: "move_arm" } ] } }
            }"#,
        )
    }

    fn find<'a>(edges: &'a [Edge], iface: &str) -> &'a Edge {
        edges
            .iter()
            .find(|e| e.interface == iface)
            .unwrap_or_else(|| panic!("no edge for `{iface}`"))
    }

    #[test]
    fn enumerate_resolves_interface_deps_to_conforming_producer() {
        let configs = vec![consumer(), camera_mock(), arm()];
        let edges = enumerate_edges(&configs);

        // Two interface-conformance edges (topic + service) + one direct action.
        assert_eq!(edges.len(), 3, "edges: {}", edges.len());

        let video = find(&edges, "video_stream");
        assert_eq!(video.kind, InterfaceKind::Topic);
        assert_eq!(video.to_node, "uvc_camera_python_mock");
        assert_eq!(
            video.origin,
            Some(("uvc_camera".to_string(), "v1".to_string()))
        );
        assert_eq!(video.via_interface(), Some("uvc_camera:v1".to_string()));

        let info = find(&edges, "video_stream_info");
        assert_eq!(info.kind, InterfaceKind::Service);
        assert_eq!(
            info.origin,
            Some(("uvc_camera".to_string(), "v1".to_string()))
        );

        let move_arm = find(&edges, "move_arm");
        assert_eq!(move_arm.kind, InterfaceKind::Action);
        assert_eq!(move_arm.to_node, "arm");
        assert_eq!(move_arm.origin, None);
        assert_eq!(move_arm.via_interface(), None);
    }

    #[test]
    fn edge_target_picks_interface_vs_node() {
        let configs = vec![consumer(), camera_mock(), arm()];
        let edges = enumerate_edges(&configs);

        // Conformed artifacts must target the interface-keyed wire path.
        let video = find(&edges, "video_stream");
        let target = video.target().expect("interface target");
        assert!(target.is_interface());
        assert_eq!(target.name(), "uvc_camera");
        assert_eq!(target.tag(), "v1");

        // Direct node deps must target the node-keyed wire path.
        let move_arm = find(&edges, "move_arm");
        let target = move_arm.target().expect("node target");
        assert!(target.is_node());
        assert_eq!(target.name(), "arm");
    }

    #[test]
    fn enumerate_skips_interface_dep_without_provider() {
        // No conforming provider in the set → the interface edges drop out, but
        // the direct action edge survives.
        let configs = vec![consumer(), arm()];
        let edges = enumerate_edges(&configs);
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].interface, "move_arm");
    }
}
