//! `stack_benchmark` action: measures the per-interface messaging latency that
//! wires each node to its direct dependencies, against the already-running
//! stack. Runs inside the core daemon (which owns the node graph in-process) and
//! acts as a messaging *client* to probe producers.
//!
//! ## No-trigger guarantee (by construction)
//! - Services / actions are measured with `Probe`-kind queries
//!   ([`ServiceMessenger::probe_latency`] / [`ActionMessenger::probe_latency`])
//!   that carry a real-payload-sized body and ask the producer to reply with a
//!   real-sized response (sizes estimated from the message schema), so the
//!   round-trip reflects real serialization + transport. The framework still
//!   auto-answers them: no user handler runs and no goal is created.
//! - Topic edges get a synthetic *node-probe* row: a `Probe`-kind query to the
//!   producer node's always-on `node_health` framework service, with the reply
//!   sized from the topic's message schema. It rides the same probe auto-answer
//!   path, so no handler runs and the real topic key is never published.
//! - Real topic latency is *observe-only*: we subscribe to the producer's live
//!   traffic and never publish onto a real topic key.

use crate::Result;
use crate::names;
use crate::services::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use crate::services::node::gate::{Admission, ConcurrencyGate};
use crate::services::node::resolve_interface_doc;
use config::node::{
    DependsOn, MessageSizeEstimate, NodeConfig, QoSProfile, estimate_serialized_size,
    node_conforms_to,
};
use core_node_api::encoding::{
    BenchmarkFeedbackStep, ClockConfidence, ClockOffsetRequest, ClockOffsetResponse, InterfaceKind,
    InterfaceLatency, MeasurementKind, StackBenchmarkFeedback, StackBenchmarkGoal,
    StackBenchmarkGoalResponse, StackBenchmarkResult,
};
use daemon_config::consts::PeppyDirs;
use latency_report::stats::summarize;
use node_stack::NodeStack;
use peppylib::clock::wall_now_ns;
use peppylib::messaging::{
    CLOCK_OFFSET_SERVICE, ConcurrentAction, ConsumerFilter, NODE_HEALTH_SERVICE, PendingGoal,
    SenderTarget, ServiceTarget,
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
use tracing::debug;

/// Gate budget: rejects a concurrent benchmark goal with a remaining-time hint.
const BENCHMARK_GATE_TIMEOUT_SECS: u64 = 1800;
/// Absolute floor below which a measured producer offset is treated as same-host
/// regardless of the round trip: at this magnitude the clocks share a host and
/// the one-way number is exact. The dominant same-host test is RTT-relative (see
/// [`classify_clock`]); this floor only covers the degenerate case of a
/// near-instant round trip whose half is itself under the threshold.
const SAME_HOST_OFFSET_NS: u64 = 100_000;
/// Number of NTP exchanges to take when measuring a producer's clock offset. The
/// estimate is biased by half the round-trip *asymmetry*, so a single sample on a
/// busy producer (or loaded host) is noisy; keeping the sample with the smallest
/// round trip — the one least perturbed by scheduling/queue delay — is the
/// standard NTP defense. See [`poll_producer_offset`].
const OFFSET_SAMPLES: u32 = 5;
/// A corrected one-way delta larger than this (or negative) is implausible and
/// is suppressed — it means the clocks are not adequately synchronized.
const IMPLAUSIBLE_DELIVERY_NS: i128 = 5_000_000_000;

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
    /// Producer-declared QoS for topic edges; the delivery subscription matches
    /// it so it receives the producer's frames.
    qos: QoSProfile,
    /// Estimated serialized size (bytes) of the real messages, so probes carry
    /// real-sized payloads instead of an empty sentinel. For service/action
    /// edges: the request and response message sizes. For topic edges: the
    /// request side stays `0` (a topic has no request leg; the node-probe sends
    /// a bare probe) and the response side is the topic message size, so the
    /// producer's framework replies with a topic-sized body. `0` when the
    /// message schema is unavailable (falls back to an empty probe). The
    /// `*_variable` flags mark a schema with variable-length fields
    /// (string/bytes/unbounded array), so the size is a lower bound.
    probe_request_size: usize,
    probe_response_size: usize,
    request_variable: bool,
    response_variable: bool,
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
                    // Filled in by resolve_probe_sizes.
                    probe_request_size: 0,
                    probe_response_size: 0,
                    request_variable: false,
                    response_variable: false,
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
    let mut cache: HashMap<(String, String), Option<daemon_config::interface::PeppyInterface>> =
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

/// Estimate the real payload sizes for each edge from the producer's message
/// schema, so probes can carry real-sized payloads. Service/action edges
/// resolve their request/response `MessageFormat`s; topic edges resolve the
/// topic's `MessageFormat` as the response side (the node-probe asks the
/// producer to reply with a topic-sized body). Formats come from the producer's
/// node config (direct deps) or interface contract (conformance deps), then a
/// lower-bound serialized size is estimated. Leaves an edge at `0` (empty
/// probe) when the schema can't be resolved, and never aborts.
fn resolve_probe_sizes(edges: &mut [Edge], configs: &[NodeConfig], peppy_dirs: &PeppyDirs) {
    let by_key: HashMap<(&str, &str), &NodeConfig> = configs
        .iter()
        .map(|c| ((c.manifest.name.as_str(), c.manifest.tag.as_str()), c))
        .collect();
    let mut iface_cache: HashMap<
        (String, String),
        Option<daemon_config::interface::PeppyInterface>,
    > = HashMap::new();

    for edge in edges.iter_mut() {
        let (req, resp) = match &edge.origin {
            None => {
                let producer = by_key
                    .get(&(edge.to_node.as_str(), edge.to_tag.as_str()))
                    .copied();
                formats_from_node(producer, edge.kind, &edge.interface)
            }
            Some((name, tag)) => {
                let doc = iface_cache
                    .entry((name.clone(), tag.clone()))
                    .or_insert_with(|| {
                        resolve_interface_doc(peppy_dirs, name, tag, None, &|_| {}).ok()
                    });
                formats_from_interface(doc.as_ref(), edge.kind, &edge.interface)
            }
        };
        if let Some(r) = req {
            edge.probe_request_size = r.bytes;
            edge.request_variable = r.has_variable;
        }
        if let Some(r) = resp {
            edge.probe_response_size = r.bytes;
            edge.response_variable = r.has_variable;
        }
    }
}

/// Request/response size estimates for an interface exposed by a node config.
/// Topic edges have no request leg, so the topic message size lands on the
/// response side (the node-probe asks for a topic-sized reply).
fn formats_from_node(
    node: Option<&NodeConfig>,
    kind: InterfaceKind,
    name: &str,
) -> (Option<MessageSizeEstimate>, Option<MessageSizeEstimate>) {
    let Some(node) = node else {
        return (None, None);
    };
    match kind {
        InterfaceKind::Service => {
            let svc = node
                .interfaces
                .services
                .as_ref()
                .and_then(|s| s.exposes.as_ref())
                .and_then(|v| v.iter().find(|e| e.name == name));
            (
                svc.and_then(|s| s.request_message_format.as_ref())
                    .map(estimate_serialized_size),
                svc.and_then(|s| s.response_message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
        InterfaceKind::Action => {
            let goal = node
                .interfaces
                .actions
                .as_ref()
                .and_then(|a| a.exposes.as_ref())
                .and_then(|v| v.iter().find(|e| e.name == name))
                .and_then(|a| a.goal_service.as_ref());
            (
                goal.and_then(|g| g.request_message_format.as_ref())
                    .map(estimate_serialized_size),
                goal.and_then(|g| g.response_message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
        InterfaceKind::Topic => {
            let topic = node
                .interfaces
                .topics
                .as_ref()
                .and_then(|t| t.emits.as_ref())
                .and_then(|v| v.iter().find(|e| e.name == name));
            (
                None,
                topic
                    .and_then(|t| t.message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
    }
}

/// Request/response size estimates for an interface declared in an interface
/// contract. Same response-side convention for topics as [`formats_from_node`].
fn formats_from_interface(
    doc: Option<&daemon_config::interface::PeppyInterface>,
    kind: InterfaceKind,
    name: &str,
) -> (Option<MessageSizeEstimate>, Option<MessageSizeEstimate>) {
    let Some(doc) = doc else {
        return (None, None);
    };
    match kind {
        InterfaceKind::Service => {
            let svc = doc.interfaces.services.iter().find(|e| e.name == name);
            (
                svc.and_then(|s| s.request_message_format.as_ref())
                    .map(estimate_serialized_size),
                svc.and_then(|s| s.response_message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
        InterfaceKind::Action => {
            let goal = doc
                .interfaces
                .actions
                .iter()
                .find(|e| e.name == name)
                .and_then(|a| a.goal_service.as_ref());
            (
                goal.and_then(|g| g.request_message_format.as_ref())
                    .map(estimate_serialized_size),
                goal.and_then(|g| g.response_message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
        InterfaceKind::Topic => {
            let topic = doc.interfaces.topics.iter().find(|e| e.name == name);
            (
                None,
                topic
                    .and_then(|t| t.message_format.as_ref())
                    .map(estimate_serialized_size),
            )
        }
    }
}

/// Human-readable byte count (decimal units, matching the rest of the output).
fn human_bytes(n: usize) -> String {
    let f = n as f64;
    if n < 1_000 {
        format!("{n}B")
    } else if f < 1e6 {
        format!("{:.1}KB", f / 1e3)
    } else if f < 1e9 {
        format!("{:.1}MB", f / 1e6)
    } else {
        format!("{:.1}GB", f / 1e9)
    }
}

/// The probe row note describing the real request→response payload sizes it
/// measured. A `≥` prefix marks a schema-derived lower bound (the format has a
/// variable-length field). `honored_full` is whether the producer **ever**
/// replied with the full requested response size — honoring sized replies is a
/// binary property of the producer's framework version, so a single full reply
/// proves it. Only a producer that never honored (predates sized probes, always
/// replies empty) is flagged; this is deliberately robust to a transient short
/// reply, which is otherwise position-dependent and would flicker the note.
fn payload_note(edge: &Edge, honored_full: bool) -> String {
    let side = |bytes: usize, variable: bool| {
        let s = human_bytes(bytes);
        if variable { format!("≥{s}") } else { s }
    };
    let mut note = format!(
        "payload {} → {}",
        side(edge.probe_request_size, edge.request_variable),
        side(edge.probe_response_size, edge.response_variable),
    );
    if edge.probe_response_size > 0 && !honored_full {
        note.push_str(" (rebuild producer for sized replies)");
    }
    note
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
    resolve_probe_sizes(&mut edges, &configs, &ctx.peppy_dirs);

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
            InterfaceKind::Service => {
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::Probing,
                    format!("Probing {}", edge_label(edge)),
                );
                rows.push(
                    measure_probe(
                        ctx,
                        edge,
                        warmup,
                        samples,
                        timeout,
                        MeasurementKind::ServiceProbe,
                    )
                    .await,
                );
            }
            InterfaceKind::Action => {
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::Probing,
                    format!("Probing {}", edge_label(edge)),
                );
                rows.push(
                    measure_probe(
                        ctx,
                        edge,
                        warmup,
                        samples,
                        timeout,
                        MeasurementKind::ActionProbe,
                    )
                    .await,
                );
            }
            // A topic edge yields two rows: a synthetic node-probe (handler-free
            // round-trip with a topic-schema-sized reply) and the observe-only
            // delivery measurement on live traffic.
            InterfaceKind::Topic => {
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::Probing,
                    format!("Probing producer node of {}", edge_label(edge)),
                );
                rows.push(
                    measure_probe(
                        ctx,
                        edge,
                        warmup,
                        samples,
                        timeout,
                        MeasurementKind::NodeProbe,
                    )
                    .await,
                );
                emit_feedback(
                    tx,
                    BenchmarkFeedbackStep::TopicDelivery,
                    format!("Measuring delivery {}", edge_label(edge)),
                );
                rows.push(measure_topic_delivery(ctx, edge, warmup, samples, timeout).await);
            }
        }
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

/// Timed `Probe` round-trips, dispatched by `measurement`:
/// - [`MeasurementKind::ServiceProbe`] / [`MeasurementKind::ActionProbe`] target
///   the edge's own service (or the action's goal service);
/// - [`MeasurementKind::NodeProbe`] (topic edges) targets the producer node's
///   always-on `node_health` framework service, asking for a reply sized from
///   the topic's message schema; the real topic key is never published.
///
/// The user handler never runs; the measurement is clock-independent.
async fn measure_probe(
    ctx: &BenchmarkActionContext,
    edge: &Edge,
    warmup: u32,
    samples: u32,
    timeout: Duration,
    measurement: MeasurementKind,
) -> InterfaceLatency {
    // The node-probe rides the node-keyed wire path regardless of how the topic
    // itself is keyed: `node_health` is a per-node framework service, so an
    // interface-conformance edge still probes the producer node directly.
    let target = match measurement {
        MeasurementKind::NodeProbe => SenderTarget::node(&edge.to_node, &edge.to_tag),
        _ => edge.target(),
    };
    let target = match target {
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
    let service_name = match measurement {
        MeasurementKind::NodeProbe => NODE_HEALTH_SERVICE,
        _ => edge.interface.as_str(),
    };

    // Carry a real-payload-sized request and ask the producer to reply with the
    // real response size, so the round-trip reflects real serialization +
    // transport — still without running the handler.
    let request_size = edge.probe_request_size;
    let response_size = edge.probe_response_size.min(u32::MAX as usize) as u32;

    let total = warmup.saturating_add(samples);
    let mut out = Vec::new();
    let mut any_success = false;
    let mut consecutive_errors: u32 = 0;
    // Whether the producer EVER returned the full requested response size.
    // Honoring sized replies is a binary property of the producer's framework
    // version, so one full reply proves it — keep it robust to a transient short
    // reply (e.g. an empty first/discovery reply) rather than trusting any single
    // sample, which would make the note flicker depending on ordering.
    let mut honored_full = false;
    for i in 0..total {
        let result = match measurement {
            MeasurementKind::ActionProbe => {
                ActionMessenger::probe_latency(
                    &ctx.messenger,
                    &ctx.bound_core_node,
                    &ctx.core_instance_id,
                    target.clone(),
                    service_name,
                    None, // wildcard: the edge's producers all live on this daemon
                    timeout,
                    request_size,
                    response_size,
                )
                .await
            }
            _ => {
                ServiceMessenger::probe_latency(
                    &ctx.messenger,
                    &ctx.bound_core_node,
                    &ctx.core_instance_id,
                    target.clone(),
                    service_name,
                    ServiceTarget::Any, // the edge's producers all live on this daemon
                    timeout,
                    request_size,
                    response_size,
                )
                .await
            }
        };
        match result {
            Ok((d, resp_bytes)) => {
                any_success = true;
                consecutive_errors = 0;
                honored_full |= resp_bytes >= edge.probe_response_size;
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
    } else if edge.probe_request_size == 0 && edge.probe_response_size == 0 {
        // No schema resolved → this was an empty probe; say so rather than
        // claiming a 0-byte payload measurement.
        Some("payload size unknown (no message schema)".to_string())
    } else {
        Some(payload_note(edge, honored_full))
    };
    row_from_samples(edge, measurement, ClockConfidence::NotApplicable, out, note)
}

/// Poll a producer's `clock_offset` service to get its measured offset to the
/// core node, used to normalize cross-host topic timestamps.
///
/// A single NTP exchange's offset is biased by half the round-trip *asymmetry*,
/// which on a busy producer is hundreds of µs. We take [`OFFSET_SAMPLES`]
/// exchanges and keep the one with the smallest round trip: the least-delayed
/// sample is the least perturbed by scheduling/queue asymmetry, so its offset is
/// the closest to the true clock difference. Returns the best `(offset_ns,
/// round_trip_delay_ns)`, or `None` if every exchange failed.
async fn poll_producer_offset(
    ctx: &BenchmarkActionContext,
    to_node: &str,
    to_tag: &str,
    timeout: Duration,
) -> Option<(i64, u64)> {
    let target = SenderTarget::node(to_node, to_tag).ok()?;
    let mut best: Option<(i64, u64)> = None;
    for _ in 0..OFFSET_SAMPLES {
        let Ok(request) = ClockOffsetRequest::new().encode() else {
            continue;
        };
        let reply = ServiceMessenger::poll(
            &ctx.messenger,
            &ctx.bound_core_node,
            &ctx.core_instance_id,
            target.clone(),
            CLOCK_OFFSET_SERVICE,
            ServiceTarget::Any, // the node's clock_offset endpoint lives on this daemon
            request,
            timeout,
        )
        .await;
        let Ok(reply) = reply else { continue };
        let Ok(decoded) = ClockOffsetResponse::decode(reply.payload().as_ref()) else {
            continue;
        };
        let sample = (decoded.offset_ns, decoded.round_trip_delay_ns);
        if best.is_none_or(|(_, best_rtt)| sample.1 < best_rtt) {
            best = Some(sample);
        }
    }
    best
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
                 deploy PTP or NTP and rely on the probe numbers — see the guide"
                    .to_string(),
            ),
        );
    }
    match offset {
        None => (
            ClockConfidence::SameHost,
            Some("producer clock offset unavailable; treated as same-host".to_string()),
        ),
        Some((o, rtt)) => {
            // The offset estimate is only accurate to ±(asymmetry)/2, and the
            // asymmetry is bounded by the round trip — so an offset within half
            // the RTT is indistinguishable from zero and means same-host. The
            // absolute floor covers a near-instant round trip whose half is
            // itself below the noise we expect from co-located clocks. Without
            // this, a busy producer's hundreds-of-µs scheduling asymmetry
            // misreads a same-host edge as cross-host `corrected`.
            let same_host_bound = (rtt / 2).max(SAME_HOST_OFFSET_NS);
            if o.unsigned_abs() <= same_host_bound {
                (ClockConfidence::SameHost, None)
            } else {
                (ClockConfidence::CrossHostCorrected, None)
            }
        }
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

    loop {
        if measured >= samples {
            break;
        }
        match tokio::time::timeout(per_sample_timeout, subscription.on_next_message()).await {
            Ok(Some(msg)) => {
                // Every session enables timestamping for its own role (see
                // `pmi::zenoh_config`), so a delivered sample is always stamped.
                // A missing timestamp means a session disabled it out-of-band
                // (e.g. a custom `ZENOH_SESSION_CONFIG`) — an invariant break,
                // surfaced loudly rather than silently dropped.
                let src = msg.source_timestamp_nanos().expect(
                    "delivery sample missing its producer timestamp; sessions enable timestamping",
                );
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
        note = Some("no live traffic observed within the timeout".to_string());
    }
    row_from_samples(edge, MeasurementKind::TopicDelivery, confidence, out, note)
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
                peppy_schema: "node/v1",
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
                peppy_schema: "node/v1",
                manifest: { name: "uvc_camera_python_mock", tag: "v1" },
                execution: { language: "rust", run_cmd: ["camera"] },
                interfaces: { conforms_to: [ { name: "uvc_camera", tag: "v1" } ] }
            }"#,
        )
    }

    fn arm() -> NodeConfig {
        parse(
            r#"{
                peppy_schema: "node/v1",
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

    #[test]
    fn classify_clock_treats_offset_within_half_rtt_as_same_host() {
        // The regression: a busy same-host producer's single-sample offset
        // (200µs) sits well inside half the round trip (1ms RTT → 500µs bound),
        // so it must read same-host, not cross-host `corrected`.
        let (confidence, note) = classify_clock(Some((200_000, 1_000_000)), false);
        assert_eq!(confidence, ClockConfidence::SameHost);
        assert!(note.is_none());
    }

    #[test]
    fn classify_clock_flags_offset_beyond_half_rtt_as_cross_host() {
        // A 2ms offset on a 1ms round trip cannot come from asymmetry alone —
        // it's a genuine clock difference, so correct it.
        let (confidence, _) = classify_clock(Some((2_000_000, 1_000_000)), false);
        assert_eq!(confidence, ClockConfidence::CrossHostCorrected);
    }

    #[test]
    fn classify_clock_absolute_floor_covers_near_instant_round_trip() {
        // Tiny RTT (20µs → 10µs half) but a 50µs offset: the absolute floor
        // keeps it same-host rather than over-reacting to sub-100µs noise.
        let (confidence, _) = classify_clock(Some((50_000, 20_000)), false);
        assert_eq!(confidence, ClockConfidence::SameHost);
    }

    #[test]
    fn classify_clock_implausible_is_flagged_and_unavailable_is_same_host() {
        assert_eq!(
            classify_clock(Some((123, 456)), true).0,
            ClockConfidence::CrossHostFlagged
        );
        assert_eq!(classify_clock(None, false).0, ClockConfidence::SameHost);
    }

    #[test]
    fn human_bytes_uses_decimal_units() {
        assert_eq!(human_bytes(512), "512B");
        assert_eq!(human_bytes(1500), "1.5KB");
        assert_eq!(human_bytes(6_220_800), "6.2MB");
    }

    #[test]
    fn formats_from_node_sizes_topic_message_on_response_side() {
        let provider = parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "camera", tag: "v1" },
                execution: { language: "rust", run_cmd: ["camera"] },
                interfaces: { topics: { emits: [ {
                    name: "frames",
                    message_format: {
                        width: "u32",
                        height: "u32",
                        frame: { $type: "array", $items: "u8" }
                    }
                } ] } }
            }"#,
        );
        let (req, resp) = formats_from_node(Some(&provider), InterfaceKind::Topic, "frames");
        assert!(req.is_none(), "a topic has no request leg");
        let resp = resp.expect("topic message sized");
        assert!(resp.bytes > 0);
        assert!(resp.has_variable, "unbounded frame array is a lower bound");
        // An unknown topic name resolves to no size (→ empty probe).
        assert_eq!(
            formats_from_node(Some(&provider), InterfaceKind::Topic, "nope").1,
            None
        );
    }

    #[test]
    fn formats_from_interface_sizes_topic_message_on_response_side() {
        let doc = daemon_config::interface::PeppyInterfaceParser::from_content(
            r#"{
                peppy_schema: "interface/v1",
                manifest: { name: "uvc_camera", tag: "v1" },
                interfaces: { topics: [ {
                    name: "video_stream",
                    message_format: {
                        encoding: "string",
                        width: "u32",
                        height: "u32",
                        frame: { $type: "array", $items: "u8" }
                    }
                } ] }
            }"#,
        )
        .expect("parse interface");
        let (req, resp) = formats_from_interface(Some(&doc), InterfaceKind::Topic, "video_stream");
        assert!(req.is_none(), "a topic has no request leg");
        let resp = resp.expect("topic message sized");
        assert!(resp.bytes > 0);
        assert!(resp.has_variable);
    }

    #[test]
    fn formats_from_node_resolves_service_request_response_sizes() {
        let provider = parse(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "arm", tag: "v1" },
                execution: { language: "rust", run_cmd: ["arm"] },
                interfaces: { services: { exposes: [ {
                    name: "move_arm",
                    request_message_format: { x: "f64", y: "f64", z: "f64" },
                    response_message_format: { ok: "bool" }
                } ] } }
            }"#,
        );
        let (req, resp) = formats_from_node(Some(&provider), InterfaceKind::Service, "move_arm");
        let req = req.expect("request sized");
        let resp = resp.expect("response sized");
        assert!(req.bytes > resp.bytes, "3×f64 request > a bool response");
        assert!(!req.has_variable && !resp.has_variable);
        // An unknown service name resolves to no sizes (→ empty probe).
        assert_eq!(
            formats_from_node(Some(&provider), InterfaceKind::Service, "nope").0,
            None
        );
    }

    fn probe_edge(req: usize, resp: usize, req_var: bool, resp_var: bool) -> Edge {
        Edge {
            from_node: "c".into(),
            from_tag: "v1".into(),
            to_node: "p".into(),
            to_tag: "v1".into(),
            interface: "svc".into(),
            link_id: "l".into(),
            origin: None,
            kind: InterfaceKind::Service,
            qos: QoSProfile::default(),
            probe_request_size: req,
            probe_response_size: resp,
            request_variable: req_var,
            response_variable: resp_var,
        }
    }

    #[test]
    fn payload_note_marks_variable_and_degraded() {
        let edge = probe_edge(64, 4000, false, true);
        // The response side is a schema lower bound (`≥`); the producer honored
        // sized replies (returned the full size at least once), so no marker.
        assert_eq!(payload_note(&edge, true), "payload 64B → ≥4.0KB");
        // A producer that never honored the requested size predates sized
        // probes — flag it.
        assert!(payload_note(&edge, false).contains("rebuild producer"));
    }

    #[test]
    fn payload_note_no_marker_when_no_response_expected() {
        // An empty response schema (size 0) can't be "unhonored" — never flag it,
        // even if the producer replied empty.
        let edge = probe_edge(64, 0, false, false);
        assert!(!payload_note(&edge, false).contains("rebuild producer"));
    }
}
