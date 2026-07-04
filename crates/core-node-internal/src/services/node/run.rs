use super::super::action_loop::{GoalHandler, accept_goal, reject_goal, run_action_loop};
use super::gate::ConcurrencyGate;
use super::pairing::{PairingCoordinator, plan_requested_pairs};
use super::{FeedbackLine, FeedbackStream, create_action_log_file, write_error_to_log};
use crate::Result;
use crate::names;
use config::peppy_config::PeerConfig;
use config::runtime::Name;
use config::runtime::RuntimeConfig;
use config::{AnyType, apply_parameter_defaults, resolve_argument_path};
use core_node_api::InstanceState;
use core_node_api::encoding::{NodeRunFeedback, NodeRunGoal, NodeRunGoalResponse, NodeRunResult};
use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::{Mode, PeppyConfig};
use futures::FutureExt;
use node_stack::{self, EntityHandle, NodeEntity, NodeStack};
use parking_lot::Mutex as StdMutex;
use peppylib::encoding::health::NodeHealthRequest;
use peppylib::encoding::ready::NodeReadyRequest;
use peppylib::messaging::SenderTarget;
use peppylib::messaging::{
    ActionFeedbackPublisher, ConcurrentAction, NODE_HEALTH_SERVICE, NODE_READY_SERVICE,
    PendingGoal, ServiceTarget,
};
use peppylib::types::Payload;
use peppylib::{MessengerHandle, PeppyError, PeppyResult, ServiceMessenger};
use std::collections::BTreeMap;
use std::fs::File;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};
use tokio::process::Child;
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::debug;

/// Quiet window the drain waits after the readers report they are caught up,
/// to absorb a brief second burst of startup output before closing the feedback
/// stream. A process emits its startup output as one burst, so a short window
/// suffices; a container prints intermittently during image conversion and
/// needs a longer one.
const PROCESS_DRAIN_QUIET_WINDOW: Duration = Duration::from_millis(10);
const CONTAINER_DRAIN_QUIET_WINDOW: Duration = Duration::from_millis(100);
/// Liveness backstop for the drain. It is not reached on the normal path: a
/// process drains as soon as its readers go idle, and a container that produces
/// stdout drains shortly after. It only bounds a wedged reader or a container
/// that never produces the stdout the drain was told to wait for.
const DRAIN_MAX_WAIT: Duration = Duration::from_secs(2);

/// Quiet window to use for the given node kind. See the constants above.
fn drain_quiet_window(is_container: bool) -> Duration {
    if is_container {
        CONTAINER_DRAIN_QUIET_WINDOW
    } else {
        PROCESS_DRAIN_QUIET_WINDOW
    }
}

/// Defaults the peppy daemon resolves from its `peppy_config` and ships to
/// every spawned node's launch config: the messaging topology (mode + peer
/// buffer sizes) and the daemon-liveness grace period for the node's watchdog.
/// Threaded as one unit (rather than parallel scalars) from the service
/// constructors through the run/launch context chains down to
/// [`apply_daemon_defaults`], so the next daemon-global knob touches this
/// struct and that function, not every context in between.
#[derive(Clone)]
pub struct DaemonDefaults {
    /// Daemon-global messaging mode, injected into every spawned node.
    pub messaging_mode: Mode,
    /// Daemon-global peer buffer sizes, injected into every spawned node.
    pub peer_buffer: PeerConfig,
    /// Daemon-liveness grace period (seconds), injected into every spawned node
    /// so its watchdog knows how long to tolerate a silent daemon.
    pub daemon_grace_secs: u64,
    /// Cooperative-shutdown grace period (seconds), injected into every spawned
    /// node so its runtime bounds registered shutdown hooks by the same window
    /// the daemon waits before force-killing a stopping node.
    pub shutdown_grace_secs: u64,
    /// The daemon's organization namespace (`"local"` when logged out, else the
    /// org id), stamped onto every spawned node's `discovery.organization_id` so
    /// the node opens its session under exactly the daemon's namespace and stays
    /// routing-isolated across the federation. Resolved per daemon generation
    /// from the cached credentials, not from `peppy_config`, so it is threaded in
    /// rather than derived in `from_peppy_config`.
    pub organization_namespace: String,
}

impl DaemonDefaults {
    /// Resolves the per-node defaults from the daemon's loaded `peppy_config`
    /// (the single place that knows which of its fields are shipped to
    /// spawned nodes) plus the daemon's resolved `organization_namespace`
    /// (which comes from the credentials, not `peppy_config`).
    pub fn from_peppy_config(config: &PeppyConfig, organization_namespace: String) -> Self {
        Self {
            messaging_mode: config.mode,
            peer_buffer: config.peer,
            daemon_grace_secs: config.lifecycle.daemon_grace_secs,
            shutdown_grace_secs: config.lifecycle.shutdown_grace_secs,
            organization_namespace,
        }
    }
}

#[derive(Clone)]
pub struct NodeRunServiceConfig {
    pub node_startup_timeout: Duration,
    pub node_start_health_timeout: Duration,
    pub peppy_dirs: PeppyDirs,
    pub health_monitor_interval: Duration,
    pub health_monitor_timeout: Duration,
    pub daemon_defaults: DaemonDefaults,
    /// Daemon-shutdown signal. Cancelled at the start of a clean shutdown so the
    /// per-node health monitors stop probing before the stack is torn down,
    /// rather than flagging intentionally-stopping nodes as unhealthy.
    pub shutdown_token: CancellationToken,
    /// The daemon's single pairing authority: reserve/deliver/dissolve for
    /// the goal's `requested_pairs`/`deferred_pairs`.
    pub pairing: Arc<PairingCoordinator>,
}

#[derive(Clone)]
pub(crate) struct NodeRunActionContext {
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) messenger: MessengerHandle,
    pub(crate) core_node_name: String,
    pub(crate) caller_instance_id: String,
    pub(crate) node_startup_timeout: Duration,
    pub(crate) node_start_health_timeout: Duration,
    pub(crate) peppy_dirs: PeppyDirs,
    pub(crate) health_monitor_interval: Duration,
    pub(crate) health_monitor_timeout: Duration,
    pub(crate) daemon_defaults: DaemonDefaults,
    pub(crate) shutdown_token: CancellationToken,
    pub(crate) pairing: Arc<PairingCoordinator>,
}

/// Applies the [`DaemonDefaults`] to a node's session config before it is
/// launched: the messaging mode + peer buffer sizes, and the daemon-resolved
/// liveness grace period (so the spawned node's watchdog self-terminates if
/// the daemon dies and stays gone). `container_separate_ns` forces the node
/// onto the router-relay (client) path even in peer mode, because a container
/// in a separate network namespace cannot form direct loopback peer links. So
/// the effective gossip is "peer mode AND not separate-namespace".
fn apply_daemon_defaults(
    cfg: &mut RuntimeConfig,
    defaults: DaemonDefaults,
    container_separate_ns: bool,
) {
    cfg.discovery.gossip = defaults.messaging_mode.is_peer() && !container_separate_ns;
    cfg.discovery.standard_buffer_size = defaults.peer_buffer.standard_buffer_size;
    cfg.discovery.high_throughput_buffer_size = defaults.peer_buffer.high_throughput_buffer_size;
    cfg.lifecycle.daemon_grace_secs = defaults.daemon_grace_secs;
    cfg.lifecycle.shutdown_grace_secs = defaults.shutdown_grace_secs;
    // Stamp the daemon's organization namespace so the spawned node opens its
    // session under it (`peppylib` resolves `discovery.organization_id` through
    // `resolve_session_namespace`). Always set: `"local"` when logged out.
    cfg.discovery.organization_id = Some(defaults.organization_namespace.clone());
}

struct ProcessNodeRunContext {
    action: NodeRunActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    sender_instance_id: String,
}

pub async fn listen_for_node_run(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    config: NodeRunServiceConfig,
) -> Result<JoinHandle<Result<()>>> {
    let action = ConcurrentAction::expose(
        messenger,
        core_node_name,
        instance_id,
        SenderTarget::node(node_name, names::CORE_NODE_TAG)?,
        names::NODE_RUN_ACTION,
        true,
    )
    .await?;

    let handler = NodeRunGoalHandler {
        context: NodeRunActionContext {
            node_stack,
            messenger: messenger.clone(),
            core_node_name: core_node_name.to_string(),
            caller_instance_id: instance_id.to_string(),
            node_startup_timeout: config.node_startup_timeout,
            node_start_health_timeout: config.node_start_health_timeout,
            peppy_dirs: config.peppy_dirs,
            health_monitor_interval: config.health_monitor_interval,
            health_monitor_timeout: config.health_monitor_timeout,
            daemon_defaults: config.daemon_defaults,
            shutdown_token: config.shutdown_token,
            pairing: config.pairing,
        },
        gate: ConcurrencyGate::new(),
    };

    let handle = tokio::spawn(async move { run_action_loop(action, handler).await });

    Ok(handle)
}

#[derive(Clone)]
struct NodeRunGoalHandler {
    context: NodeRunActionContext,
    gate: ConcurrencyGate,
}

impl GoalHandler for NodeRunGoalHandler {
    async fn handle_goal(&self, pending: PendingGoal) {
        handle_goal_request(pending, self.context.clone(), self.gate.clone()).await
    }
}

#[derive(Clone)]
struct FeedbackSync {
    /// Lines read from the child and forwarded onto the internal channel.
    read_count: Arc<AtomicU64>,
    /// Lines copied from the internal channel onto the external feedback topic.
    published_count: Arc<AtomicU64>,
    /// Output readers that have registered and not yet exited.
    readers_live: Arc<AtomicUsize>,
    /// Registered readers currently blocked waiting for more output, i.e. they
    /// have drained every complete line in their pipe.
    readers_idle: Arc<AtomicUsize>,
    /// Set once the first stdout line of the run is seen.
    stdout_seen: Arc<AtomicBool>,
    /// Woken on every state change that can affect drain progress.
    changed: Arc<Notify>,
}

impl FeedbackSync {
    fn new() -> Self {
        Self {
            read_count: Arc::new(AtomicU64::new(0)),
            published_count: Arc::new(AtomicU64::new(0)),
            readers_live: Arc::new(AtomicUsize::new(0)),
            readers_idle: Arc::new(AtomicUsize::new(0)),
            stdout_seen: Arc::new(AtomicBool::new(false)),
            changed: Arc::new(Notify::new()),
        }
    }

    fn signal_stdout(&self) {
        if !self.stdout_seen.swap(true, Ordering::Relaxed) {
            self.changed.notify_waiters();
        }
    }

    fn increment_read(&self) {
        self.read_count.fetch_add(1, Ordering::Relaxed);
    }

    fn increment_published(&self) {
        self.published_count.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    fn register_reader(&self) {
        self.readers_live.fetch_add(1, Ordering::Relaxed);
    }

    fn reader_idle(&self) {
        self.readers_idle.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    fn reader_active(&self) {
        self.readers_idle.fetch_sub(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    fn reader_exit(&self, was_idle: bool) {
        if was_idle {
            self.readers_idle.fetch_sub(1, Ordering::Relaxed);
        }
        self.readers_live.fetch_sub(1, Ordering::Relaxed);
        self.changed.notify_waiters();
    }

    /// True when every live reader is blocked waiting for more output (so all
    /// buffered lines have been read), every read line has been published onto
    /// the external feedback topic, and stdout has been seen if required.
    fn is_drained(&self, require_stdout: bool) -> bool {
        let live = self.readers_live.load(Ordering::Relaxed);
        let idle = self.readers_idle.load(Ordering::Relaxed);
        let read = self.read_count.load(Ordering::Relaxed);
        let published = self.published_count.load(Ordering::Relaxed);
        idle >= live
            && published >= read
            && (!require_stdout || self.stdout_seen.load(Ordering::Relaxed))
    }

    /// Waits until the readers have drained the child's pipes and every read
    /// line has reached the external feedback topic, then returns so the caller
    /// can close the stream.
    ///
    /// Unlike a fixed-time wait, this keys off a positive "reader is caught up"
    /// signal, so a reader that is slow to be scheduled under load delays the
    /// close instead of being mistaken for "no more output". Once drained, the
    /// state is confirmed stable for `quiet_window` so a brief second burst is
    /// not cut off. `max_wait` is a liveness backstop only.
    ///
    /// Returns `true` if the stream drained, `false` if `max_wait` elapsed.
    async fn wait_for_drain(
        &self,
        quiet_window: Duration,
        require_stdout: bool,
        max_wait: Duration,
    ) -> bool {
        let deadline = tokio::time::Instant::now() + max_wait;
        loop {
            // Register for the next state change before re-checking, so a change
            // between the check and the await is not lost.
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            if self.is_drained(require_stdout) {
                // No live reader can produce more output: nothing to settle.
                if self.readers_live.load(Ordering::Relaxed) == 0 {
                    return true;
                }
                // Live readers remain (the node is running): confirm the drained
                // state holds for the quiet window before closing.
                match tokio::time::timeout(quiet_window, notified).await {
                    Err(_) => return true,
                    Ok(_) => continue,
                }
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            let _ = tokio::time::timeout(deadline - now, notified).await;
        }
    }

    /// Drain the feedback stream, logging a debug warning if the backstop fires.
    async fn drain_or_warn(&self, instance_id: &str, quiet_window: Duration, require_stdout: bool) {
        if !self
            .wait_for_drain(quiet_window, require_stdout, DRAIN_MAX_WAIT)
            .await
        {
            debug!(
                "feedback drain timed out for node instance '{}'",
                instance_id
            );
        }
    }
}

impl node_stack::OutputReaderHooks for FeedbackSync {
    fn on_first_stdout_line(&self) {
        self.signal_stdout();
    }

    fn on_line_read(&self) {
        self.increment_read();
    }

    fn on_reader_registered(&self) {
        self.register_reader();
    }

    fn on_reader_idle(&self) {
        self.reader_idle();
    }

    fn on_reader_active(&self) {
        self.reader_active();
    }

    fn on_reader_exit(&self, was_idle: bool) {
        self.reader_exit(was_idle);
    }
}

/// Runs the node run pipeline: calls [`process_node_run`] and catches panics.
///
/// The caller is responsible for creating the log file and feedback channel.
///
/// This is the shared implementation used by both the action-server path
/// ([`handle_goal_request`]) and the direct-call path from `stack_launch`.
///
/// `cancel_token` lets an outer orchestrator (currently `stack_launch`'s
/// idle/max-timeout watchdog) request an in-flight abort. When cancelled after
/// the child process has been spawned but before the `Starting → Started`
/// commit, the pipeline explicitly calls `abort_started` to SIGKILL the child
/// and tear down its `Starting` entry; if cancelled before the spawn, it
/// returns without starting anything. Callers that don't need cancellation
/// pass `CancellationToken::new()` and never trigger it.
pub(crate) async fn run_node_run(
    goal: NodeRunGoal,
    runtime_config: RuntimeConfig,
    action_context: NodeRunActionContext,
    feedback_tx: mpsc::UnboundedSender<FeedbackLine>,
    log_file: Arc<StdMutex<File>>,
    sender_instance_id: String,
    cancel_token: CancellationToken,
) -> NodeRunResult {
    let log_file_for_panic = log_file.clone();

    let process_context = ProcessNodeRunContext {
        action: action_context,
        feedback_tx,
        log_file,
        sender_instance_id,
    };
    match AssertUnwindSafe(process_node_run(
        goal,
        runtime_config,
        process_context,
        cancel_token,
    ))
    .catch_unwind()
    .await
    {
        Ok(result) => result,
        Err(panic_payload) => {
            let msg = format!(
                "node_run task panicked: {}",
                super::panic_message(&*panic_payload)
            );
            tracing::error!("{}", msg);
            write_error_to_log(&log_file_for_panic, &msg);
            NodeRunResult::failure(msg)
        }
    }
}

async fn handle_goal_request(
    pending: PendingGoal,
    action_context: NodeRunActionContext,
    gate: ConcurrencyGate,
) {
    let sender_instance_id = pending.instance_id().to_string();

    let goal = match NodeRunGoal::decode(pending.request_bytes()) {
        Ok(goal) => goal,
        Err(e) => {
            reject_goal(
                pending,
                encode_rejected_start_goal(format!("invalid payload: {e}")),
            )
            .await;
            return;
        }
    };

    let generation = match gate.try_admit(goal.timeout_secs, false) {
        // `node_run` never forces, so nothing is ever superseded here.
        super::gate::Admission::Admitted { generation, .. } => generation,
        super::gate::Admission::AlreadyRunning { remaining_secs } => {
            reject_goal(
                pending,
                encode_rejected_start_goal(format!(
                    "action already in progress (times out in {remaining_secs}s)"
                )),
            )
            .await;
            return;
        }
    };

    // Parse runtime config to get instance_id for log file naming
    let runtime_config: RuntimeConfig = match serde_json5::from_str(&goal.runtime_config_json5) {
        Ok(config) => config,
        Err(e) => {
            let error_msg = format!("Failed to parse PEPPY_RUNTIME_CONFIG: {e}");
            gate.clear_running();
            reject_goal(pending, encode_rejected_start_goal(error_msg)).await;
            return;
        }
    };

    // Trust boundary: the runtime config travels in-band on the goal and is
    // re-exported as `PEPPY_RUNTIME_CONFIG` into the child, so a mismatch
    // would silently spawn a process under the wrong identity or bound to the
    // wrong daemon. Reject before allocating a log file or accepting the goal.
    if runtime_config.node_name.as_str() != goal.node_name
        || runtime_config.node_tag.as_str() != goal.tag
        || runtime_config.bound_core_node.as_str() != action_context.core_node_name
    {
        let error_msg = format!(
            "runtime_config identity mismatch: goal requested `{}:{}` on core node `{}`, \
             but runtime_config is `{}:{}` bound to `{}`",
            goal.node_name,
            goal.tag,
            action_context.core_node_name,
            runtime_config.node_name.as_str(),
            runtime_config.node_tag.as_str(),
            runtime_config.bound_core_node.as_str(),
        );
        gate.clear_running();
        reject_goal(pending, encode_rejected_start_goal(error_msg)).await;
        return;
    }

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();

    debug!(
        "Received `node_run` goal from {sender_instance_id}, node={}:{}, instance_id={}, runtime_config_len={}",
        goal.node_name,
        goal.tag,
        instance_id_str,
        goal.runtime_config_json5.len()
    );

    // Create log file for stdout/stderr
    let log_dir = action_context.peppy_dirs.logs_dir_run();
    let log_filename = format!("{}.log", instance_id_str);
    let (log_file, log_path) = match create_action_log_file(&log_dir, &log_filename) {
        Ok(result) => result,
        Err(error_msg) => {
            debug!("{}", error_msg);
            gate.clear_running();
            reject_goal(pending, encode_rejected_start_goal(error_msg)).await;
            return;
        }
    };

    debug!("Created log file for node run: {}", log_path.display());

    // `accept` registers the per-goal context before replying accepted.
    let Some(goal_ctx) = accept_goal(
        pending,
        super::encode_response_or_err(
            "node_run_goal",
            NodeRunGoalResponse::accepted(&log_path).encode(),
        ),
    )
    .await
    else {
        gate.clear_running();
        return;
    };

    let feedback_publisher = goal_ctx
        .feedback_publisher()
        .expect("node_run declares a feedback topic");
    let gate_for_task = gate.clone();
    tokio::spawn(async move {
        // Frees the gate slot on every exit: explicitly before completion on the
        // normal path (via `release_then_complete` below), or on unwind for a
        // panic. A no-op if a later goal already took over.
        let slot = gate_for_task.into_slot_guard(generation);
        let (feedback_tx, mut feedback_rx) = mpsc::unbounded_channel::<FeedbackLine>();

        // The node process outlives the action: once `node_run` reports the
        // instance healthy and committed, the node keeps running and its
        // output readers hold the feedback channel open. Awaiting a forwarder
        // task to learn when feedback ends would therefore hang forever.
        // Instead, drive the work future and forward feedback in the same
        // task, and stop forwarding once the work returns.
        let work = run_node_run(
            goal,
            runtime_config,
            action_context,
            feedback_tx,
            log_file,
            sender_instance_id,
            // Action-server path has no outer cancellation source; the internal
            // per-step timeouts inside `run_node_run` remain the only way out.
            CancellationToken::new(),
        );
        tokio::pin!(work);

        let mut feedback_open = true;
        let result = loop {
            tokio::select! {
                biased;
                outcome = &mut work => break outcome,
                maybe_line = feedback_rx.recv(), if feedback_open => match maybe_line {
                    Some(line) => publish_node_run_feedback(&feedback_publisher, line).await,
                    None => feedback_open = false,
                },
            }
        };

        // `run_node_run` returns only after its internal feedback flush, so any
        // lines still buffered here are the final ones and no more will arrive.
        // Drain them before `complete` emits the end-of-stream sentinel so the
        // sentinel never races ahead of the last feedback line.
        while let Ok(line) = feedback_rx.try_recv() {
            publish_node_run_feedback(&feedback_publisher, line).await;
        }

        if let Ok(payload) = result.encode() {
            slot.release_then_complete(&goal_ctx, payload).await;
        }
    });
}

/// Publishes one node_run feedback line onto the goal's feedback stream.
///
/// Encoding failures are intentionally dropped: a single malformed line should
/// not abort the run, and the line is already captured verbatim in the
/// per-instance log file.
async fn publish_node_run_feedback(publisher: &ActionFeedbackPublisher, line: FeedbackLine) {
    if let Ok(payload) = NodeRunFeedback::from_stream(line.stream, &line.line).encode() {
        let _ = publisher.publish(payload).await;
    }
}

fn encode_rejected_start_goal(reason: impl Into<String>) -> PeppyResult<Payload> {
    super::encode_response_or_err(
        "node_run_goal",
        NodeRunGoalResponse::rejected(reason).encode(),
    )
}

async fn process_node_run(
    goal: NodeRunGoal,
    mut runtime_config: RuntimeConfig,
    ctx: ProcessNodeRunContext,
    cancel_token: CancellationToken,
) -> NodeRunResult {
    let sender_instance_id = ctx.sender_instance_id.as_str();
    let NodeRunGoal {
        node_name,
        tag,
        env_vars,
        requested_pairs,
        deferred_pairs,
        ..
    } = goal;
    let mut env_vars = match super::validate_goal_env_vars(&env_vars) {
        Ok(vars) => vars,
        Err(e) => {
            let msg = e.to_string();
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    let instance_id_str = runtime_config.node_instance.instance_id.as_str();
    let instance_id = match Name::new(instance_id_str) {
        Ok(name) => name,
        Err(e) => {
            let msg = format!("Invalid instance_id: {}", e);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    debug!(
        "Processing `node_run` from {sender_instance_id}, node={}:{}, instance_id={}",
        node_name, tag, instance_id_str
    );

    let entity_handle = match ctx.action.node_stack.find(&node_name, &tag) {
        Some(entity) => entity,
        None => {
            let msg = format!(
                "Node '{}:{}' not found in node stack. \
                 Run `peppy node list` to see currently-loaded nodes, or `peppy node add {}:{}` to add it \
                 (the daemon does not persist added nodes across restarts).",
                node_name, tag, node_name, tag
            );
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    // Stack-wide `instance_id` uniqueness (spec rule 7): reject if the
    // candidate id is already tracked by a *different* `(node_name,
    // node_tag)` anywhere in the stack. The validator catches this at
    // plan time (`peppy node run` / launcher); this is the daemon's
    // defensive backstop at the trust boundary. Same-entity collisions
    // are caught by `prepare_and_spawn` under its write lock.
    if let Some((existing_name, existing_tag)) = ctx
        .action
        .node_stack
        .find_entity_label_for_instance_id_any_state(&instance_id)
        && (existing_name != node_name || existing_tag != tag)
    {
        let msg = format!(
            "Instance ID `{}` is already tracked by `{}`:{}; instance_ids must be unique across the entire stack",
            instance_id.as_str(),
            existing_name,
            existing_tag,
        );
        write_error_to_log(&ctx.log_file, &msg);
        return NodeRunResult::failure(msg);
    }

    let node_config = {
        let guard = entity_handle.read();
        if guard.artifact_path().is_none() {
            let msg = format!(
                "Node '{}:{}' is added but not built, run `peppy node build {}:{}` first.",
                node_name, tag, node_name, tag
            );
            drop(guard);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
        guard.config().clone()
    };

    // Pairing pre-spawn check (the trust-boundary twin of the CLI preflight
    // and the launcher validator): coverage of every required slot, and
    // resolution of each requested target to one concrete peer slot. Loud
    // failure here costs nothing — no process has been spawned yet. The
    // actual registry reservation happens after `prepare_and_spawn`, once
    // this instance exists in the stack (in `Starting`).
    let pairing_deps = node_config
        .manifest
        .depends_on
        .as_ref()
        .map(|d| d.pairings.as_slice())
        .unwrap_or_default();
    // Snapshot + claims are read under two short read locks, and only when
    // this run involves pairing at all; the registry re-validates at
    // reserve time, so plan-phase staleness is safe.
    let planned_pairs =
        if pairing_deps.is_empty() && requested_pairs.is_empty() && deferred_pairs.is_empty() {
            Vec::new()
        } else {
            let snapshot = ctx.action.node_stack.pairing_node_snapshots();
            let live_pairs = ctx.action.node_stack.live_pairs();
            let request = super::pairing::PairingRequest {
                node_name: &node_name,
                node_tag: &tag,
                instance_id: instance_id_str,
                pairing_deps,
                requested: &requested_pairs,
                deferred: &deferred_pairs,
            };
            match plan_requested_pairs(&snapshot, &live_pairs, &request) {
                Ok(p) => p,
                Err(msg) => {
                    write_error_to_log(&ctx.log_file, &msg);
                    return NodeRunResult::failure(msg);
                }
            }
        };
    // Deferred slots ride the same goal field for a manual `--defer-pair`
    // and for the earlier endpoint of a launch-planned pair (which the later
    // endpoint pairs automatically), so the wording must fit both: state the
    // mechanism, never instruct a manual step.
    for link_id in &deferred_pairs {
        let _ = ctx.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: format!(
                "pairing slot `{link_id}` deferred: instance starts unpaired; the pair is \
                 established automatically when a peer instance starts with \
                 `{instance_id_str}/{link_id}` as its pair target"
            ),
        });
    }

    let sccache_injected =
        super::inject_rust_build_env(&mut env_vars, node_config.execution.language);
    if sccache_injected {
        let _ = ctx.feedback_tx.send(FeedbackLine {
            stream: FeedbackStream::Stdout,
            line: "Using sccache for Rust compilation".to_string(),
        });
    }
    super::inject_node_runtime_env(
        &mut env_vars,
        node_config.manifest.name.as_str(),
        node_config.manifest.tag.as_str(),
    );

    // Apply any `$default` fallbacks from the schema before spawning, so the
    // spawned node sees a complete arg set and mount path resolution below
    // can find every referenced value. Type checks and unknown-key checks
    // are deferred to peppylib's `Processor::new_daemon` inside the spawned
    // node, where errors surface alongside any other startup failures.
    let missing = apply_parameter_defaults(
        &mut runtime_config.node_instance.arguments,
        &node_config.execution.parameters,
    );
    if !missing.is_empty() {
        let msg = format!("Missing required parameters: {}", missing.join(", "));
        write_error_to_log(&ctx.log_file, &msg);
        return NodeRunResult::failure(msg);
    }

    let is_container = node_config.execution.container.is_some();

    let container_config = node_config.execution.container.as_ref();
    let raw_mount_paths = container_config
        .and_then(|c| c.mount_paths.as_deref())
        .unwrap_or_default();
    let resolved_mount_paths = match resolve_mount_path_parameters(
        raw_mount_paths,
        &runtime_config.node_instance.arguments,
    ) {
        Ok(paths) => paths,
        Err(msg) => {
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    // A container's transport depends on whether it shares the host network
    // namespace, which `host_gateway()` reports:
    //
    //  - Lima (macOS): `Some(gateway)`: the container runs in a VM, a separate
    //    namespace. It reaches the host router only through the Lima gateway,
    //    and a loopback peer locator advertised inside the guest is unreachable
    //    from the host (and vice versa), so it cannot form direct peer links.
    //    Route it through the router as a client (gossip forced off) and rewrite
    //    `messaging_host` to the gateway, regardless of the daemon's mode.
    //  - Native (Linux): `None`: Apptainer shares the host network namespace,
    //    so `127.0.0.1` already reaches the host router and the node follows the
    //    daemon's messaging mode exactly like a process node.
    let container_gateway = if is_container {
        let apptainer = match tokio::task::spawn_blocking(containers::Apptainer::new).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                let msg = format!("Failed to initialize Apptainer: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeRunResult::failure(msg);
            }
            Err(e) => {
                let msg = format!("Apptainer initialization task failed: {}", e);
                write_error_to_log(&ctx.log_file, &msg);
                return NodeRunResult::failure(msg);
            }
        };
        apptainer.host_gateway()
    } else {
        None
    };

    // Build the config the spawned process receives: a copy of `runtime_config`
    // with the daemon-global defaults applied (and, for a separate-namespace
    // container, the gateway host). Mutate a clone rather than `runtime_config`
    // because `instance_id_str` still borrows the latter, and the rest of this
    // function reads it. The container override wins: a separate-namespace
    // container always routes through the router as a client even in peer mode.
    let mut launch_config = runtime_config.clone();
    apply_daemon_defaults(
        &mut launch_config,
        ctx.action.daemon_defaults,
        container_gateway.is_some(),
    );
    if let Some(gateway) = &container_gateway {
        launch_config.messaging_host = gateway.to_string();
    }

    // Serialize once, after every mutation (synthesized defaults, mode + buffer
    // sizes, and the gateway rewrite), so the spawned process receives the
    // fully-resolved runtime config. The inbound `runtime_config_json5` from the
    // goal still reflects the pre-defaulting state.
    let runtime_config_json5 = match serde_json5::to_string(&launch_config) {
        Ok(json) => json,
        Err(e) => {
            let msg = format!("Failed to serialize runtime config: {}", e);
            write_error_to_log(&ctx.log_file, &msg);
            return NodeRunResult::failure(msg);
        }
    };

    // Set up the FeedbackSync + two-channel forwarder. The internal channel
    // feeds the entity's output readers; the forwarder copies lines onto the
    // daemon's external feedback topic and increments published_count so
    // FeedbackSync can detect quiescence.
    let publish_enabled = Arc::new(AtomicBool::new(true));
    let feedback_sync = FeedbackSync::new();

    let (internal_feedback_tx, mut internal_feedback_rx) =
        mpsc::unbounded_channel::<FeedbackLine>();
    let external_feedback_tx = ctx.feedback_tx.clone();
    let feedback_sync_publisher = feedback_sync.clone();
    tokio::spawn(async move {
        while let Some(line) = internal_feedback_rx.recv().await {
            let _ = external_feedback_tx.send(FeedbackLine {
                stream: line.stream,
                line: line.line,
            });
            feedback_sync_publisher.increment_published();
        }
    });

    let start_ctx = node_stack::StartContext {
        instance_id: &instance_id,
        runtime_config_json5: &runtime_config_json5,
        slot_bindings: runtime_config.node_instance.slot_bindings.clone(),
        env_vars: &env_vars,
        mount_paths_resolved: &resolved_mount_paths,
        peppy_dirs: &ctx.action.peppy_dirs,
        output_sinks: node_stack::OutputSinks {
            feedback_tx: internal_feedback_tx.clone(),
            log_file: Arc::clone(&ctx.log_file),
            publish_enabled: Arc::clone(&publish_enabled),
            hooks: Arc::new(feedback_sync.clone()),
        },
    };
    // Reject early if an outer orchestrator already cancelled us; avoids
    // spawning a child process we're only going to tear down on the next line.
    if cancel_token.is_cancelled() {
        let msg = "cancelled before node process spawn".to_string();
        write_error_to_log(&ctx.log_file, &msg);
        feedback_sync
            .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
            .await;
        publish_enabled.store(false, Ordering::Release);
        return NodeRunResult::failure(msg);
    }

    let (mut child, started_ctx) =
        match node_stack::NodeEntity::prepare_and_spawn(&entity_handle, start_ctx).await {
            Ok(t) => t,
            Err(e) => {
                let msg = e.to_string();
                write_error_to_log(&ctx.log_file, &msg);
                feedback_sync
                    .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
                    .await;
                publish_enabled.store(false, Ordering::Release);
                return NodeRunResult::failure(msg);
            }
        };

    // Reserve every planned pair now that this instance is registered (in
    // `Starting`). The registry re-validates under its own lock, so a peer
    // slot claimed by a concurrent `node_run` since the pre-spawn check
    // fails here — loudly — instead of double-pairing. Pins are NOT
    // delivered yet; that happens after the instance commits to Running.
    for pair in &planned_pairs {
        let Err(reserve_msg) = ctx.action.pairing.reserve(&pair.own, &pair.peer).await else {
            continue;
        };
        let reason = format!(
            "failed to reserve pair for slot `{}`: {reserve_msg}",
            pair.own.link_id
        );
        ctx.action
            .pairing
            .dissolve_for_instance(instance_id_str)
            .await;
        let msg = node_stack::NodeEntity::abort_started(
            &entity_handle,
            child,
            started_ctx,
            reason,
            &instance_id,
        )
        .await;
        write_error_to_log(&ctx.log_file, &msg);
        feedback_sync
            .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
            .await;
        publish_enabled.store(false, Ordering::Release);
        return NodeRunResult::failure(msg);
    }

    let signal_target = NodeSignalTarget {
        messenger: &ctx.action.messenger,
        core_node_name: &ctx.action.core_node_name,
        caller_instance_id: &ctx.action.caller_instance_id,
        to_node_name: runtime_config.node_name.as_str(),
        to_node_tag: runtime_config.node_tag.as_str(),
        target_core_node: runtime_config.bound_core_node.as_str(),
        target_instance_id: instance_id_str,
    };

    debug!(
        "Successfully spawned node instance '{}', waiting for ready signal for {}s...",
        instance_id_str,
        ctx.action.node_startup_timeout.as_secs()
    );

    // Race the ready-signal wait against external cancellation. If the outer
    // idle/max-timeout watchdog cancels while the node is quietly starting,
    // we must SIGKILL the child and unregister the `Starting` instance;
    // otherwise the OS process outlives the launch failure.
    let ready_outcome = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => StartupOutcome::Cancelled,
        res = wait_for_ready_signal(&signal_target, ctx.action.node_startup_timeout, &mut child) => {
            match res {
                Ok(()) => StartupOutcome::Ok,
                Err(e) => StartupOutcome::Failed(e),
            }
        }
    };

    if let Some(reason) = startup_abort_reason(&ready_outcome) {
        debug!(
            "Aborting node instance '{}' during ready wait: {}",
            instance_id_str, reason
        );
        ctx.action
            .pairing
            .dissolve_for_instance(instance_id_str)
            .await;
        let msg = node_stack::NodeEntity::abort_started(
            &entity_handle,
            child,
            started_ctx,
            reason.to_string(),
            &instance_id,
        )
        .await;
        feedback_sync
            .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
            .await;
        publish_enabled.store(false, Ordering::Release);
        return NodeRunResult::failure(msg);
    }

    debug!(
        "Node instance '{}' is ready, performing health check...",
        instance_id_str
    );

    let health_outcome = tokio::select! {
        biased;
        _ = cancel_token.cancelled() => StartupOutcome::Cancelled,
        res = perform_health_check(&signal_target, ctx.action.node_start_health_timeout, &mut child) => {
            match res {
                Ok(()) => StartupOutcome::Ok,
                Err(e) => StartupOutcome::Failed(e),
            }
        }
    };

    match health_outcome {
        StartupOutcome::Ok => {
            debug!(
                "Health check passed for node instance '{}'",
                instance_id_str
            );
            // Last chance to bail out cleanly: once `commit_started` succeeds
            // the child is owned by the stack and a late cancel would have to
            // go through the normal stop path instead of abort_started.
            if cancel_token.is_cancelled() {
                let reason = "cancelled before commit".to_string();
                debug!(
                    "Aborting node instance '{}' after health check: {}",
                    instance_id_str, reason
                );
                ctx.action
                    .pairing
                    .dissolve_for_instance(instance_id_str)
                    .await;
                let msg = node_stack::NodeEntity::abort_started(
                    &entity_handle,
                    child,
                    started_ctx,
                    reason,
                    &instance_id,
                )
                .await;
                feedback_sync
                    .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
                    .await;
                publish_enabled.store(false, Ordering::Release);
                return NodeRunResult::failure(msg);
            }
            let pid = child.id().unwrap_or(0);
            let commit_result = node_stack::NodeEntity::commit_started(
                &entity_handle,
                child,
                started_ctx,
                instance_id.clone(),
            )
            .await;
            match commit_result {
                Ok(committed_child) => {
                    // Cancelled by the exit watcher once the node's process
                    // exits on its own, so the health monitor stops probing a
                    // now-terminal instance instead of logging a spurious
                    // "became unhealthy" on the way out.
                    let instance_done = CancellationToken::new();

                    spawn_exit_watcher(ExitWatcherParams {
                        child: committed_child,
                        entity_handle: Arc::clone(&entity_handle),
                        to_node_name: runtime_config.node_name.as_str().to_owned(),
                        node_tag: tag.clone(),
                        target_instance_id: instance_id.clone(),
                        peppy_dirs: ctx.action.peppy_dirs.clone(),
                        pairing: Arc::clone(&ctx.action.pairing),
                        instance_done: instance_done.clone(),
                        shutdown_token: ctx.action.shutdown_token.clone(),
                    });

                    spawn_health_monitor(HealthMonitorParams {
                        messenger: ctx.action.messenger.clone(),
                        core_node_name: ctx.action.core_node_name.clone(),
                        caller_instance_id: ctx.action.caller_instance_id.clone(),
                        to_node_name: runtime_config.node_name.as_str().to_owned(),
                        target_core_node: runtime_config.bound_core_node.as_str().to_owned(),
                        target_instance_id: instance_id.clone(),
                        node_tag: tag.clone(),
                        node_stack: Arc::clone(&ctx.action.node_stack),
                        peppy_dirs: ctx.action.peppy_dirs.clone(),
                        interval: ctx.action.health_monitor_interval,
                        timeout: ctx.action.health_monitor_timeout,
                        shutdown_token: ctx.action.shutdown_token.clone(),
                        instance_done,
                    });

                    // The instance is Running: deliver every reserved pin
                    // live over `peer_update` (boot config is always
                    // all-Unpaired, so this is the only way slots get
                    // paired). The watchers above are already running, so
                    // the process is reaped even if the run fails here.
                    if !planned_pairs.is_empty() {
                        if let Err(reason) = ctx
                            .action
                            .pairing
                            .deliver_pairs_for_instance(instance_id_str, &planned_pairs)
                            .await
                        {
                            ctx.action
                                .pairing
                                .dissolve_for_instance(instance_id_str)
                                .await;
                            let msg = format!(
                                "node instance '{instance_id_str}' started but pairing \
                                 delivery failed: {reason}. The instance was left running \
                                 with its pairing slots unpaired; stop it with \
                                 `peppy node stop {instance_id_str}`"
                            );
                            write_error_to_log(&ctx.log_file, &msg);
                            feedback_sync
                                .drain_or_warn(
                                    instance_id_str,
                                    drain_quiet_window(is_container),
                                    false,
                                )
                                .await;
                            publish_enabled.store(false, Ordering::Release);
                            return NodeRunResult::failure(msg);
                        }
                        for pair in &planned_pairs {
                            let _ = ctx.feedback_tx.send(FeedbackLine {
                                stream: FeedbackStream::Stdout,
                                line: format!("paired: {} ⇌ {}", pair.own, pair.peer),
                            });
                        }
                    }

                    // Wait until the readers have drained the child's startup
                    // output onto the feedback stream before closing it. Keyed
                    // off a positive "reader caught up" signal, so heavy load
                    // delays the close instead of dropping output. Containers
                    // wait for their first stdout line; processes do not, so a
                    // silent process is not penalized.
                    feedback_sync
                        .drain_or_warn(
                            instance_id_str,
                            drain_quiet_window(is_container),
                            is_container,
                        )
                        .await;
                    let result = NodeRunResult::success(pid);
                    publish_enabled.store(false, Ordering::Release);
                    result
                }
                Err(e) => {
                    let msg = format!("Failed to register instance: {}", e);
                    ctx.action
                        .pairing
                        .dissolve_for_instance(instance_id_str)
                        .await;
                    write_error_to_log(&ctx.log_file, &msg);
                    feedback_sync
                        .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
                        .await;
                    publish_enabled.store(false, Ordering::Release);
                    NodeRunResult::failure(msg)
                }
            }
        }
        StartupOutcome::Cancelled | StartupOutcome::Failed(_) => {
            let reason = match health_outcome {
                StartupOutcome::Cancelled => "cancelled during health check".to_string(),
                StartupOutcome::Failed(e) => e,
                StartupOutcome::Ok => unreachable!(),
            };
            debug!(
                "Aborting node instance '{}' during health check: {}",
                instance_id_str, reason
            );
            ctx.action
                .pairing
                .dissolve_for_instance(instance_id_str)
                .await;
            let msg = node_stack::NodeEntity::abort_started(
                &entity_handle,
                child,
                started_ctx,
                reason,
                &instance_id,
            )
            .await;
            feedback_sync
                .drain_or_warn(instance_id_str, drain_quiet_window(is_container), false)
                .await;
            publish_enabled.store(false, Ordering::Release);
            NodeRunResult::failure(msg)
        }
    }
}

/// Outcome of a startup step (ready-signal wait or health check) racing
/// against external cancellation.
enum StartupOutcome {
    Ok,
    Cancelled,
    Failed(String),
}

fn startup_abort_reason(outcome: &StartupOutcome) -> Option<&str> {
    match outcome {
        StartupOutcome::Ok => None,
        StartupOutcome::Cancelled => Some("cancelled during ready-signal wait"),
        StartupOutcome::Failed(msg) => Some(msg.as_str()),
    }
}

/// Replaces `${parameters:...}` tokens in mount paths with actual argument values.
///
/// Each `${parameters:<dot.path>}` is resolved against the runtime `arguments`.
/// Only `AnyType::String` values are accepted; other types produce an error.
///
/// After resolution, the source (host) portion of each mount path is validated
/// against the blocked system directories list.
pub(crate) fn resolve_mount_path_parameters(
    mount_paths: &[String],
    arguments: &BTreeMap<String, AnyType>,
) -> std::result::Result<Vec<String>, String> {
    let mut resolved = Vec::with_capacity(mount_paths.len());
    for mount in mount_paths {
        let mut result = String::with_capacity(mount.len());
        let mut remaining: &str = mount;

        while let Some(start) = remaining.find("${parameters:") {
            result.push_str(&remaining[..start]);
            let after_prefix = &remaining[start + "${parameters:".len()..];
            let end = after_prefix
                .find('}')
                .ok_or_else(|| format!("Unclosed parameter reference in mount path: {mount}"))?;
            let dot_path = &after_prefix[..end];

            match resolve_argument_path(arguments, dot_path) {
                Some(AnyType::String(value)) => {
                    result.push_str(value);
                }
                Some(other) => {
                    return Err(format!(
                        "Parameter `{dot_path}` in mount path must be a string, got {}",
                        other.type_name()
                    ));
                }
                None => {
                    return Err(format!(
                        "Parameter `{dot_path}` referenced in mount path not found in arguments"
                    ));
                }
            }
            remaining = &after_prefix[end + 1..];
        }
        result.push_str(remaining);

        // Validate the resolved source path against blocked system directories.
        let src = result.split(':').next().unwrap_or(&result);
        if config::node::is_blocked_mount_source(src) {
            return Err(format!(
                "Resolved mount path `{result}` uses a blocked system directory `{src}` as source; use a subdirectory instead"
            ));
        }

        resolved.push(result);
    }
    Ok(resolved)
}

struct NodeSignalTarget<'a> {
    messenger: &'a MessengerHandle,
    core_node_name: &'a str,
    caller_instance_id: &'a str,
    to_node_name: &'a str,
    to_node_tag: &'a str,
    target_core_node: &'a str,
    target_instance_id: &'a str,
}

/// Performs a health check on a newly started node instance.
/// Polls the node's health service with a timeout and returns Ok if the node responds.
/// Also monitors the child process to detect early exits.
async fn perform_health_check(
    target: &NodeSignalTarget<'_>,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeHealthRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node health request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to avoid a startup race where the node subscribes to
    // `node_health` after the first request has already been published.
    loop {
        // Check if the child process has exited
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited before becoming healthy (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target.target_instance_id.to_string()),
                service_name: NODE_HEALTH_SERVICE.to_string(),
            });
            return Err(format!("health check timed out: {err}"));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            target.messenger,
            target.core_node_name,
            target.caller_instance_id,
            SenderTarget::node_from_validated(target.to_node_name, target.to_node_tag),
            NODE_HEALTH_SERVICE,
            ServiceTarget::Producer(&peppylib::messaging::ProducerRef::new(
                target.target_core_node,
                target.target_instance_id,
            )),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

/// Waits for a node to signal it's ready (runner::run() has started).
/// Polls the node's ready service with a timeout and returns Ok when the node responds.
/// Also monitors the child process to detect early exits (e.g., compilation failures).
///
/// This is used during startup to wait for compilation to complete before
/// starting the health check timer.
async fn wait_for_ready_signal(
    target: &NodeSignalTarget<'_>,
    timeout: Duration,
    child: &mut Child,
) -> std::result::Result<(), String> {
    let request_payload = NodeReadyRequest::new()
        .encode()
        .map_err(|e| format!("failed to encode node ready request: {e}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_err: Option<PeppyError> = None;

    // Poll in short intervals to detect when the node becomes ready
    loop {
        // Check if the child process has exited (e.g., compilation failed)
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(format!(
                    "node process exited during startup (status={status})"
                ));
            }
            Ok(None) => {}
            Err(err) => return Err(format!("failed to query node process status: {err}")),
        }

        let now = Instant::now();
        if now >= deadline {
            let err = last_err.unwrap_or_else(|| PeppyError::ServiceTimeout {
                instance_id: Some(target.target_instance_id.to_string()),
                service_name: NODE_READY_SERVICE.to_string(),
            });
            return Err(format!(
                "startup timed out waiting for node to be ready (node may still be compiling): {err}"
            ));
        }

        let remaining = deadline - now;
        let attempt_timeout = remaining.min(Duration::from_millis(500));

        match ServiceMessenger::poll(
            target.messenger,
            target.core_node_name,
            target.caller_instance_id,
            SenderTarget::node_from_validated(target.to_node_name, target.to_node_tag),
            NODE_READY_SERVICE,
            ServiceTarget::Producer(&peppylib::messaging::ProducerRef::new(
                target.target_core_node,
                target.target_instance_id,
            )),
            request_payload.clone(),
            attempt_timeout,
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(err) => {
                last_err = Some(err);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        }
    }
}

struct HealthMonitorParams {
    messenger: MessengerHandle,
    core_node_name: String,
    caller_instance_id: String,
    to_node_name: String,
    target_core_node: String,
    target_instance_id: Name,
    node_tag: String,
    node_stack: Arc<NodeStack>,
    peppy_dirs: PeppyDirs,
    interval: Duration,
    timeout: Duration,
    shutdown_token: CancellationToken,
    /// Cancelled by the instance's exit watcher once its process exits on its
    /// own, so the monitor stops the moment the instance goes terminal rather
    /// than running one more probe (which would fail against the dead process
    /// and log a misleading "became unhealthy" for a node that simply finished).
    instance_done: CancellationToken,
}

/// Spawns a background task that periodically polls the node's health service
/// and records the outcome on the instance's health flag, which `stack list`
/// and `node info` surface. A failing probe marks the instance `unhealthy`; a
/// later passing probe marks it `healthy` again. This task never removes the
/// instance from the stack, so an unhealthy node stays visible until it
/// recovers or is stopped explicitly (e.g. `node stop`).
///
/// The task exits when the instance is no longer found in the stack (stopped
/// externally), when `instance_done` is cancelled (the instance's process
/// exited on its own and the exit watcher has moved it to a terminal state), or
/// when `shutdown_token` is cancelled (the daemon is shutting down, so the
/// monitored nodes are being torn down on purpose and must not be reported
/// unhealthy for it).
fn spawn_health_monitor(p: HealthMonitorParams) {
    tokio::spawn(async move {
        let instance_id_str = p.target_instance_id.as_str().to_owned();
        let request_payload = match NodeHealthRequest::new().encode() {
            Ok(payload) => payload,
            Err(e) => {
                tracing::warn!(
                    "Health monitor for '{}' failed to encode request: {}",
                    instance_id_str,
                    e
                );
                return;
            }
        };

        // Tracks the previously observed health so the monitor logs only on
        // transitions (a node going down or recovering), not on every failing
        // tick. Instances start healthy, matching the flag's initial value.
        let mut was_healthy = true;

        loop {
            // Wait out the probe interval, but bail the instant the daemon starts
            // shutting down: probing nodes that are intentionally being torn down
            // would log spurious "unhealthy" / "Session not initialized" warnings
            // for the whole teardown window.
            tokio::select! {
                _ = p.shutdown_token.cancelled() => return,
                _ = p.instance_done.cancelled() => return,
                _ = tokio::time::sleep(p.interval) => {}
            }

            // Resolve the monitored instance once per tick. If it was removed
            // externally (e.g. user ran `node stop`), our job is done: skip the
            // poll and exit. The returned clone shares the instance's health
            // flag (an `Arc<AtomicBool>`), so recording the probe result on it
            // after the poll still updates the tracked instance even though it
            // was resolved beforehand. Should the instance be removed during the
            // poll, that write lands on a now-detached flag no reader can reach,
            // so it is harmless.
            let Some(instance) = p.node_stack.find_by_instance_id(&p.target_instance_id) else {
                debug!(
                    "Health monitor: instance '{}' no longer in stack, exiting",
                    instance_id_str
                );
                return;
            };

            // Bound to a local so the borrow in `ServiceTarget::Producer` outlives
            // the `select!` expansion (a temporary would be dropped too early).
            let producer_ref = peppylib::messaging::ProducerRef::new(
                p.target_core_node.as_str(),
                p.target_instance_id.as_str(),
            );
            // Abandon an in-flight probe the moment shutdown starts, so a probe
            // racing the session close cannot emit a teardown-time warning.
            let poll_result = tokio::select! {
                biased;
                _ = p.shutdown_token.cancelled() => return,
                _ = p.instance_done.cancelled() => return,
                result = ServiceMessenger::poll(
                    &p.messenger,
                    &p.core_node_name,
                    &p.caller_instance_id,
                    SenderTarget::node_from_validated(&p.to_node_name, &p.node_tag),
                    NODE_HEALTH_SERVICE,
                    ServiceTarget::Producer(&producer_ref),
                    request_payload.clone(),
                    p.timeout,
                ) => result,
            };

            let probe_succeeded = poll_result.is_ok();
            let probe_error = poll_result.err();

            // If either cancellation fired while this probe was in flight, stop
            // here without recording or logging. `instance_done` means the
            // instance's process exited on its own and the exit watcher is moving
            // it to a terminal state, so a node that simply finished never
            // produces a trailing "became unhealthy". `shutdown_token` means the
            // daemon is tearing down on purpose, so a probe that raced the session
            // close must not emit a teardown-time warning.
            if p.instance_done.is_cancelled() || p.shutdown_token.is_cancelled() {
                return;
            }

            // Record the probe result so `stack list` and `node info` can report
            // health without re-probing. A failed probe flags the instance
            // unhealthy; a later successful probe clears the flag. The instance
            // is never removed here, so an unhealthy node stays visible in the
            // stack until it recovers or is stopped explicitly.
            instance.set_healthy(probe_succeeded);

            // Log only on health edges so a node that stays down
            // does not re-emit the same warning every tick.
            match (was_healthy, probe_succeeded) {
                // Down edge: alert once when a healthy node first fails.
                (true, false) => {
                    let reason = probe_error
                        .as_ref()
                        .map(|e| e.to_string())
                        .unwrap_or_default();
                    tracing::warn!(
                        "Health monitor: instance '{}' of node '{}:{}' became unhealthy: {}",
                        instance_id_str,
                        p.to_node_name,
                        p.node_tag,
                        reason
                    );
                    super::append_stack_log(
                        &p.peppy_dirs,
                        &format!(
                            "Instance '{}' of node '{}:{}' became unhealthy: \
                             failed health check ({})",
                            instance_id_str, p.to_node_name, p.node_tag, reason,
                        ),
                    );
                }
                // Up edge: the node came back.
                (false, true) => {
                    tracing::info!(
                        "Health monitor: instance '{}' of node '{}:{}' recovered",
                        instance_id_str,
                        p.to_node_name,
                        p.node_tag
                    );
                    super::append_stack_log(
                        &p.peppy_dirs,
                        &format!(
                            "Instance '{}' of node '{}:{}' recovered after a failed health check",
                            instance_id_str, p.to_node_name, p.node_tag,
                        ),
                    );
                }
                // No edge: a low-noise debug heartbeat while still failing; the
                // `if let` makes the still-healthy case a no-op.
                (false, false) | (true, true) => {
                    if let Some(err) = &probe_error {
                        debug!(
                            "Health monitor: instance '{}' health check still failing: {}",
                            instance_id_str, err
                        );
                    }
                }
            }

            was_healthy = probe_succeeded;
        }
    });
}

struct ExitWatcherParams {
    /// The committed node process, owned by the watcher from here on. `wait()`
    /// observes its exit and reaps it.
    child: Child,
    /// Handle to the entity that owns this instance. Holding it keeps the entity
    /// alive for the watcher's lifetime. The transition itself is guarded inside
    /// `mark_instance_exited`, which under the entity write lock only acts on an
    /// instance that is still `Running` and not being stopped, so a removal or
    /// an explicit stop that raced the exit is a clean no-op, not a clobber.
    entity_handle: EntityHandle,
    to_node_name: String,
    node_tag: String,
    target_instance_id: Name,
    peppy_dirs: PeppyDirs,
    /// Death auto-clears pairs: on a self-exit the watcher eagerly dissolves
    /// every pair involving this instance and notifies each live survivor.
    pairing: Arc<PairingCoordinator>,
    /// Cancelled once the process exits, to stop this instance's health monitor.
    instance_done: CancellationToken,
    shutdown_token: CancellationToken,
}

/// Spawns a background task that owns a committed node's [`Child`] and waits for
/// its process to exit on its own. Closes the gap where a node that finishes its
/// work and shuts itself down (a one-shot node), or that crashes, would
/// otherwise stay `Running` forever, with only the health monitor flipping it to
/// a misleading `unhealthy`. On a self-exit the instance is moved to a terminal
/// state: [`InstanceState::Finished`] for a clean exit (status code 0), or
/// [`InstanceState::Failed`] for a non-zero / signal exit (a crash).
///
/// The watcher does nothing when the exit was the daemon's doing: it bails on
/// `shutdown_token` (daemon teardown), and `mark_instance_exited` no-ops when the
/// instance was marked `stopping` by an explicit `node stop` / stack clear, so an
/// intentional force-kill is never relabeled as a crash. In those cases the stop
/// path owns removal. Either way it cancels `instance_done` so the health monitor
/// stops probing the now-dead process instead of logging a spurious "became
/// unhealthy" for a node that simply finished.
fn spawn_exit_watcher(p: ExitWatcherParams) {
    let ExitWatcherParams {
        mut child,
        entity_handle,
        to_node_name,
        node_tag,
        target_instance_id,
        peppy_dirs,
        pairing,
        instance_done,
        shutdown_token,
    } = p;

    tokio::spawn(async move {
        let instance_id_str = target_instance_id.as_str().to_owned();

        // Wait for the process to exit, watching for daemon shutdown racing it.
        // On shutdown (`None`) the whole stack is being torn down on purpose, so
        // this node must not be recorded as crashed and the stop path owns
        // removal; on a self-exit (`Some`) it moves to a terminal state below.
        let status = tokio::select! {
            biased;
            _ = shutdown_token.cancelled() => None,
            status = child.wait() => Some(status),
        };

        // The process is, or imminently will be, gone. Stop the health monitor
        // first, before any lock work, so it cannot squeeze in one more probe
        // against the dead process and log a misleading "became unhealthy".
        instance_done.cancel();

        // On daemon shutdown still drain `child.wait()` to reap the process the
        // teardown is SIGKILLing: this task owns the only handle that can reap it
        // (no `kill_on_drop`), so returning now would orphan a zombie. The stop
        // path owns removal, so do not record a terminal state here.
        let Some(status) = status else {
            let _ = child.wait().await;
            return;
        };

        // A `wait()` error (the OS refused to report the status, vanishingly
        // rare) is treated as an unclean exit, the conservative choice.
        let exited_cleanly = matches!(&status, Ok(s) if s.success());

        // Transition Running -> Finished/Failed. Returns None when a stop path
        // already claimed this instance (it owns removal) or it is no longer
        // Running, so an intentional stop is never relabeled as a self-exit.
        let Some(new_state) =
            NodeEntity::mark_instance_exited(&entity_handle, &target_instance_id, exited_cleanly)
        else {
            return;
        };

        // Death auto-clears pairs. The eager half of cleanup: dissolve this
        // instance's pairs and live-notify each survivor that its slot is
        // Unpaired (the registry's lazy prune-on-read is the backstop for
        // paths that never reach here).
        pairing
            .dissolve_for_instance(instance_id_str.as_str())
            .await;

        match new_state {
            InstanceState::Finished => {
                tracing::info!(
                    "Exit watcher: instance '{}' of node '{}:{}' finished (exited cleanly)",
                    instance_id_str,
                    to_node_name,
                    node_tag
                );
                super::append_stack_log(
                    &peppy_dirs,
                    &format!(
                        "Instance '{}' of node '{}:{}' finished: process exited cleanly",
                        instance_id_str, to_node_name, node_tag,
                    ),
                );
            }
            InstanceState::Failed => {
                let detail = match &status {
                    Ok(s) => format!("exit status {s}"),
                    Err(e) => format!("could not read exit status: {e}"),
                };
                tracing::warn!(
                    "Exit watcher: instance '{}' of node '{}:{}' exited unexpectedly ({})",
                    instance_id_str,
                    to_node_name,
                    node_tag,
                    detail
                );
                super::append_stack_log(
                    &peppy_dirs,
                    &format!(
                        "Instance '{}' of node '{}:{}' failed: process {}",
                        instance_id_str, to_node_name, node_tag, detail,
                    ),
                );
            }
            // `mark_instance_exited` only ever returns a terminal state.
            InstanceState::Starting | InstanceState::Running => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn runtime_config_for_test() -> RuntimeConfig {
        let instance_id = config::runtime::Name::new("camera_front").unwrap();
        RuntimeConfig::new(
            "127.0.0.1",
            7448,
            config::runtime::NodeInstanceConfig::new(instance_id),
            "camera",
            "v1",
            "core_node",
        )
        .expect("valid test runtime config")
    }

    /// `DaemonDefaults` with the given mode/buffers and arbitrary
    /// recognizable grace periods.
    fn daemon_defaults(mode: Mode, peer: PeerConfig) -> DaemonDefaults {
        DaemonDefaults {
            messaging_mode: mode,
            peer_buffer: peer,
            daemon_grace_secs: 123,
            shutdown_grace_secs: 17,
            organization_namespace: "local".to_string(),
        }
    }

    /// Peer mode (no container override) keeps gossip on and applies the
    /// configured buffer sizes.
    #[test]
    fn apply_daemon_defaults_peer_mode_enables_gossip() {
        let mut cfg = runtime_config_for_test();
        apply_daemon_defaults(
            &mut cfg,
            daemon_defaults(Mode::Peer, PeerConfig::default()),
            false,
        );
        assert!(cfg.discovery.gossip);
        assert_eq!(cfg.discovery.standard_buffer_size, 128);
        assert_eq!(cfg.discovery.high_throughput_buffer_size, 1024);
    }

    /// Router mode forces gossip off so all traffic relays through the router.
    #[test]
    fn apply_daemon_defaults_router_mode_disables_gossip() {
        let mut cfg = runtime_config_for_test();
        apply_daemon_defaults(
            &mut cfg,
            daemon_defaults(Mode::Router, PeerConfig::default()),
            false,
        );
        assert!(!cfg.discovery.gossip);
    }

    /// A separate-namespace container is forced onto the client path even in
    /// peer mode (the container override wins).
    #[test]
    fn apply_daemon_defaults_container_separate_ns_forces_client_even_in_peer_mode() {
        let mut cfg = runtime_config_for_test();
        apply_daemon_defaults(
            &mut cfg,
            daemon_defaults(Mode::Peer, PeerConfig::default()),
            true,
        );
        assert!(
            !cfg.discovery.gossip,
            "separate-namespace container must route through the router"
        );
    }

    /// Buffer sizes, both grace periods, and the organization namespace are
    /// applied regardless of mode or container placement.
    #[test]
    fn apply_daemon_defaults_always_applies_buffers_and_grace() {
        let peer = PeerConfig {
            standard_buffer_size: 64,
            high_throughput_buffer_size: 4096,
        };
        for (mode, container_separate_ns) in [
            (Mode::Peer, false),
            (Mode::Router, false),
            (Mode::Peer, true),
        ] {
            let mut cfg = runtime_config_for_test();
            apply_daemon_defaults(&mut cfg, daemon_defaults(mode, peer), container_separate_ns);
            assert_eq!(cfg.discovery.standard_buffer_size, 64);
            assert_eq!(cfg.discovery.high_throughput_buffer_size, 4096);
            assert_eq!(cfg.lifecycle.daemon_grace_secs, 123);
            assert_eq!(cfg.lifecycle.shutdown_grace_secs, 17);
            // The node is stamped with the daemon's namespace, so it opens its
            // session under the same routing-isolation prefix as the daemon.
            assert_eq!(cfg.discovery.organization_id.as_deref(), Some("local"));
        }
    }

    /// A logged-in daemon stamps the org id (not `local`) onto every node.
    #[test]
    fn apply_daemon_defaults_stamps_the_org_namespace() {
        let mut defaults = daemon_defaults(Mode::Peer, PeerConfig::default());
        defaults.organization_namespace = "550e8400-e29b-41d4-a716-446655440000".to_string();
        let mut cfg = runtime_config_for_test();
        apply_daemon_defaults(&mut cfg, defaults, false);
        assert_eq!(
            cfg.discovery.organization_id.as_deref(),
            Some("550e8400-e29b-41d4-a716-446655440000")
        );
    }

    #[test]
    fn test_resolve_mount_path_parameters_simple() {
        let mount_paths = vec!["${parameters:device_path}:/dev/video0:rw".to_string()];
        let mut arguments = BTreeMap::new();
        arguments.insert(
            "device_path".to_string(),
            AnyType::String("/dev/video0".to_string()),
        );

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/dev/video0:/dev/video0:rw"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_nested() {
        let mount_paths = vec!["${parameters:video.device_path}:/dev/video0:rw".to_string()];
        let mut video = BTreeMap::new();
        video.insert(
            "device_path".to_string(),
            AnyType::String("/dev/video1".to_string()),
        );
        let mut arguments = BTreeMap::new();
        arguments.insert("video".to_string(), AnyType::Object(video));

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/dev/video1:/dev/video0:rw"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_passthrough() {
        let mount_paths = vec!["/data/models:/opt/models:ro".to_string()];
        let arguments = BTreeMap::new();

        let resolved = resolve_mount_path_parameters(&mount_paths, &arguments).unwrap();
        assert_eq!(resolved, vec!["/data/models:/opt/models:ro"]);
    }

    #[test]
    fn test_resolve_mount_path_parameters_rejects_blocked_path() {
        let mount_paths = vec!["${parameters:path}:/container:rw".to_string()];
        let mut arguments = BTreeMap::new();
        arguments.insert("path".to_string(), AnyType::String("/tmp".to_string()));

        let result = resolve_mount_path_parameters(&mount_paths, &arguments);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("blocked system directory"));
    }

    // ---- FeedbackSync::wait_for_drain ----
    //
    // These run on a paused clock so the quiet-window settle and the backstop
    // resolve instantly and deterministically instead of sleeping.

    /// Drives the hook sequence a real output reader produces when it reads `n`
    /// lines, has them all published, and then catches up (goes idle).
    fn register_read_publish_idle(sync: &FeedbackSync, n: usize) {
        sync.register_reader();
        for _ in 0..n {
            sync.increment_read();
            sync.increment_published();
        }
        sync.reader_idle();
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_returns_once_readers_idle_and_published() {
        let sync = FeedbackSync::new();
        register_read_publish_idle(&sync, 1); // stdout reader
        register_read_publish_idle(&sync, 1); // stderr reader

        let drained = sync
            .wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(2))
            .await;
        assert!(
            drained,
            "should drain once every reader is idle and every line is published"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_blocks_until_publish_catches_up() {
        let sync = FeedbackSync::new();
        sync.register_reader();
        sync.increment_read(); // a line was read and the reader is caught up...
        sync.reader_idle();
        // ...but the forwarder has not copied it onto the external topic yet.

        let still_waiting = tokio::time::timeout(
            Duration::from_millis(50),
            sync.wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(10)),
        )
        .await;
        assert!(
            still_waiting.is_err(),
            "drain must not return while a read line is still unpublished"
        );

        sync.increment_published();
        let drained = sync
            .wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(2))
            .await;
        assert!(drained, "drain should complete once the line is published");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_blocks_while_a_reader_is_active() {
        let sync = FeedbackSync::new();
        sync.register_reader();
        sync.register_reader();
        sync.reader_idle(); // only one of the two readers is caught up

        let still_waiting = tokio::time::timeout(
            Duration::from_millis(50),
            sync.wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(10)),
        )
        .await;
        assert!(
            still_waiting.is_err(),
            "drain must wait until every live reader is idle"
        );

        sync.reader_idle(); // second reader is now caught up
        let drained = sync
            .wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(2))
            .await;
        assert!(drained, "drain completes once all readers are idle");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_requires_stdout_when_asked() {
        let sync = FeedbackSync::new();
        sync.register_reader();
        sync.reader_idle(); // caught up, but no stdout line has been seen

        // With require_stdout, an idle reader that never produced stdout must
        // hit the backstop instead of draining early.
        let drained = sync
            .wait_for_drain(Duration::from_millis(10), true, Duration::from_millis(50))
            .await;
        assert!(!drained, "require_stdout must block until stdout is seen");

        sync.signal_stdout();
        let drained = sync
            .wait_for_drain(Duration::from_millis(10), true, Duration::from_secs(2))
            .await;
        assert!(drained, "drain completes once stdout has been seen");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_with_no_live_readers_returns_immediately() {
        let sync = FeedbackSync::new();
        // No readers registered, e.g. a failure before the child was spawned.
        // The long windows would block forever if the fast path were missing.
        let drained = sync
            .wait_for_drain(Duration::from_secs(60), false, Duration::from_secs(60))
            .await;
        assert!(drained, "with no live readers there is nothing to drain");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_backstop_fires_when_a_reader_never_idles() {
        let sync = FeedbackSync::new();
        sync.register_reader(); // a reader that never reports idle (e.g. wedged)

        let drained = sync
            .wait_for_drain(Duration::from_millis(10), false, Duration::from_millis(50))
            .await;
        assert!(
            !drained,
            "the backstop must fire when a reader never drains"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn wait_for_drain_stops_counting_an_exited_reader() {
        let sync = FeedbackSync::new();
        sync.register_reader();
        sync.register_reader();
        sync.reader_idle(); // one reader is idle
        sync.reader_exit(false); // the other hits EOF while active and exits

        let drained = sync
            .wait_for_drain(Duration::from_millis(10), false, Duration::from_secs(2))
            .await;
        assert!(
            drained,
            "an exited reader must no longer be counted as live"
        );
    }
}
