//! The daemon bridge against a fake daemon: an ephemeral router, and a
//! fake core node that serves `stack_list` and runs `stack_join` and
//! `stack_remove` on peppylib's production goal engine, answering each when
//! the test tells it to: admitted or refused with a failure kind, then
//! completed, failed with a failure kind, or cancelled at the bridge's
//! cancel. Every step waits on a peer's action, never on the
//! clock: a deadline only bounds the failure path. The progress window is
//! tested on a bridge whose clock moves only when the test advances it (see
//! [`PausedRuntime`]), and the bridge behind a real endpoint over HTTP at the
//! end of this file.

use super::stack_copies::STACK_BUSY_REASON;
use super::stack_copies::ScopedStackCopies;
use super::stack_copies::StackCopies;
use super::{DaemonBridges, DaemonScopes, OwnDaemon};
use crate::bridges::prepare;
use crate::serve::ServeError;
use crate::test_support::{Events, PausedRuntime, ScriptedSurface};
use config::failure_kind::PeppyFailureKind;
use config::runtime::{CopyName, CoreNodeName, Name};
use core_node_api::encoding::{
    CopyAction, CopyChange, CopyInfo, LaunchFeedback, LaunchFeedbackStep, LaunchGoalResponse,
    LaunchOutput, LaunchResult, NodeLogs, NodeRunLogEntry, StackJoinGoal, StackListRequest,
    StackListResponse, StackRemoveGoal,
};
use core_node_api::names::CORE_NODE_TAG;
use core_node_api::{ActionGoal, DaemonFailureKind, ServiceRequest};
use daemon_config::daemon_interface::{DaemonInterface, StackCopiesScope};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use peppy_mcp_catalog::build_exposure_bundle;
use peppy_mcp_runtime::{ActionExit, CancelledGoal, ExposureServer, ExposureSet, ProviderFailure};
use peppylib::messaging::{
    CancelCause, Failure, GoalContext, MessengerHandle, ProducerRef, SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, MockServiceCore, READINESS_TIMEOUT,
    wait_action_reachable, wait_service_reachable,
};
use serde_json::{Value, json};
use stack_goal::DEFAULT_BUDGETS;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The coordinator: the daemon's core node, which the server is bound to.
const COORDINATOR: &str = "cn-coordinator";
const DAEMON_INSTANCE: &str = "core_root";
const SERVER_INSTANCE: &str = "framework_controls_inst";
/// The log file of every goal the fake daemon admits.
const STACK_LOG: &str = "/home/peppy/.peppy/logs/stack/goal.log";
/// The progress window of `framework_controls:v1`.
const WINDOW: Duration = Duration::from_secs(660);
/// A step of the paused clock that stays inside [`WINDOW`].
const WITHIN_THE_WINDOW: Duration = WINDOW.saturating_sub(Duration::from_millis(1));

/// The design's `framework/framework_controls.json5`.
const FRAMEWORK_CONTROLS: &str = include_str!(
    "../../../daemon-config-internal/src/daemon_interface/fixtures/framework_controls.json5"
);

/// The scope `simulation_mcp` gives its framework endpoint: the OpenArm
/// offers its brain and a recorder, the SO-101 no feature.
fn simulation_scope() -> Value {
    json!({
        "max_copies": 4,
        "options": [
            {
                "option": "openarm_sim",
                "model": "openarm_v2",
                "description": "A simulated OpenArm v2 standing on the floor",
                "features": [
                    { "feature": "ai_brain", "axis": "brain", "option": "ai_brain_vla",
                      "description": "An AI brain that finds items by words" },
                    { "feature": "recorder", "axis": "recorder", "option": "lerobot_recorder",
                      "description": "A recorder of episodes" },
                ],
            },
            { "option": "so101_sim", "model": "so101",
              "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

/// A scope of one option, `so101_sim`, and one copy at most.
fn single_scope() -> Value {
    json!({
        "max_copies": 1,
        "options": [{ "option": "so101_sim", "model": "so101", "description": "A simulated SO-101" }],
    })
}

fn scope_of(value: Value) -> StackCopiesScope {
    serde_json::from_value(value).expect("the scope parses")
}

fn name(name: &str) -> Name {
    Name::new(name).expect("a name")
}

fn copy_name(name: &str) -> CopyName {
    CopyName::new(name).expect("a copy name")
}

/// The fake daemon: its session, its stack list, and its two stack
/// changes.
struct FakeDaemon {
    list: MockServiceCore,
    join: MockActionServerCore,
    remove: MockActionServerCore,
    session: MessengerHandle,
}

/// One mesh per test: the router and the server's session.
struct Mesh {
    server: MessengerHandle,
    _router: EphemeralRouter,
}

impl Mesh {
    fn own_daemon(&self) -> OwnDaemon {
        OwnDaemon::new(self.server.clone(), COORDINATOR, SERVER_INSTANCE)
    }

    /// A bridge of `stack_copies` under `scope`, as the server holds it.
    fn bridge(&self, scope: Value) -> ScopedStackCopies {
        Arc::new(StackCopies::new(self.own_daemon())).scoped(scope_of(scope))
    }
}

/// Starts the mesh and the fake daemon, and returns once the server's
/// session reaches each of its functions.
async fn mesh() -> (Mesh, FakeDaemon) {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let session = router.connect().await.expect("the daemon connects");
    let server = router.connect().await.expect("the server connects");
    let identity = SenderTarget::node(COORDINATOR, CORE_NODE_TAG).expect("a core node identity");
    let producer = ProducerRef::new(COORDINATOR, DAEMON_INSTANCE);
    let list_service = StackListRequest::ID.name();
    let list = MockServiceCore::listen(
        &session,
        COORDINATOR,
        DAEMON_INSTANCE,
        identity.clone(),
        list_service,
    )
    .await
    .expect("the daemon serves stack_list");
    wait_service_reachable(
        &server,
        COORDINATOR,
        SERVER_INSTANCE,
        identity.clone(),
        list_service,
        &producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("stack_list becomes reachable");
    let mut changes = Vec::new();
    for action in [StackJoinGoal::ID.name(), StackRemoveGoal::ID.name()] {
        changes.push(
            MockActionServerCore::expose(
                &session,
                COORDINATOR,
                DAEMON_INSTANCE,
                identity.clone(),
                action,
                true,
            )
            .await
            .expect("the daemon exposes its stack change"),
        );
        wait_action_reachable(
            &server,
            COORDINATOR,
            SERVER_INSTANCE,
            identity.clone(),
            action,
            &producer,
            READINESS_TIMEOUT,
        )
        .await
        .expect("the stack change becomes reachable");
    }
    let remove = changes.pop().expect("stack_remove");
    let join = changes.pop().expect("stack_join");
    (
        Mesh {
            server,
            _router: router,
        },
        FakeDaemon {
            list,
            join,
            remove,
            session,
        },
    )
}

/// A copy of `option` on the stack, named `copy_name`.
fn copy(copy: &str, option: &str) -> CopyInfo {
    copy_on(copy, option, COORDINATOR, &[])
}

/// A copy of `option` named `copy` on `machine`, its own axes filled by
/// `selections`, as `stack list` gives them.
fn copy_on(copy: &str, option: &str, machine: &str, selections: &[&str]) -> CopyInfo {
    CopyInfo {
        name: copy_name(copy),
        core_node: CoreNodeName::new(machine).expect("a core node name"),
        instance_ids: vec![name(&format!("{copy}_arm_inst"))],
        selections: selections
            .iter()
            .map(|selection| selection.parse().expect("an axis selection"))
            .collect(),
        option: option.to_owned(),
        set_members: Vec::new(),
    }
}

fn admitted() -> peppylib::types::Payload {
    LaunchGoalResponse::new(STACK_LOG)
        .encode()
        .expect("the admission encodes")
}

/// The reason the scripted daemon refuses a goal with.
const DAEMON_REFUSAL: &str =
    "a stack change is in progress on this daemon; run this again once it ends";

/// The refusal of a daemon that runs another stack change.
fn busy() -> Failure {
    Failure::new(DaemonFailureKind::Busy, DAEMON_REFUSAL)
}

impl FakeDaemon {
    /// The daemon answers the next read of its stack with `copies`.
    fn lists(&self, copies: &[CopyInfo]) {
        self.lists_during(copies, None);
    }

    /// The daemon answers the next read of its stack with `copies` and the
    /// copy change `change` that holds it.
    fn lists_during(&self, copies: &[CopyInfo], change: Option<CopyChange>) {
        let mut answer = StackListResponse::new("{}", COORDINATOR, DAEMON_INSTANCE, "host");
        answer.copies = copies.to_vec();
        answer.copy_change = change;
        self.list
            .enqueue_response(answer.encode().expect("the answer encodes"));
    }

    /// How many times the bridge read the stack.
    fn reads(&self) -> usize {
        self.list.captured().len()
    }

    /// Admits the next join, which must be `sent`.
    async fn admits_join(&mut self, sent: &StackJoinGoal) -> Arc<GoalContext> {
        let pending = self
            .join
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the join reaches the daemon");
        assert_eq!(
            &StackJoinGoal::decode(pending.request_bytes()).expect("the join decodes"),
            sent
        );
        pending
            .accept(admitted())
            .await
            .expect("the daemon admits the join")
    }

    /// Refuses the next join with `refusal`.
    async fn refuses_join(&mut self, refusal: Failure) {
        self.join
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the join reaches the daemon")
            .refuse(refusal)
            .await
            .expect("the daemon refuses the join");
    }

    /// Admits the next removal, which must name `copy_name`.
    async fn admits_removal(&mut self, copy_name: &str) -> Arc<GoalContext> {
        let pending = self
            .remove
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the removal reaches the daemon");
        assert_eq!(
            StackRemoveGoal::decode(pending.request_bytes()).expect("the removal decodes"),
            StackRemoveGoal::new(self::copy_name(copy_name))
        );
        pending
            .accept(admitted())
            .await
            .expect("the daemon admits the removal")
    }
}

/// The change of copy `copy_name` of `option` that `action` makes.
fn copy_change(action: CopyAction, copy: &str, option: &str) -> CopyChange {
    CopyChange {
        action,
        name: copy_name(copy),
        option: option.to_owned(),
    }
}

/// Polls `future` once, in the calling task, and tells whether it is still
/// pending.
async fn pending_after_one_poll<F: std::future::Future + Unpin>(future: &mut F) -> bool {
    std::future::poll_fn(|context| {
        std::task::Poll::Ready(std::pin::Pin::new(&mut *future).poll(context).is_pending())
    })
    .await
}

/// The join the bridge sends for `copy_name` of `option` with no feature: no
/// word, no argument, no environment, on the coordinator, under the default
/// budgets.
fn join_goal(copy_name: &str, option: &str) -> StackJoinGoal {
    StackJoinGoal::new(
        format!("{option}:{copy_name}")
            .parse()
            .expect("a copy reference"),
        DEFAULT_BUDGETS,
    )
}

async fn publish(context: &GoalContext, output: &LaunchOutput) {
    publish_feedback(context, LaunchFeedback::Output(output.clone())).await;
}

async fn publish_feedback(context: &GoalContext, feedback: LaunchFeedback) {
    context
        .publish_feedback(feedback.encode().expect("the feedback encodes"))
        .await
        .expect("the daemon publishes its feedback");
}

/// The daemon completes the change.
async fn complete(context: &GoalContext) {
    context
        .complete(
            LaunchResult::default()
                .encode()
                .expect("the result encodes"),
        )
        .await
        .expect("the daemon ends the goal");
}

/// The daemon fails the change with `failure`, after the node logs `logs`
/// as the last feedback.
async fn fail(context: &GoalContext, failure: Failure, logs: NodeLogs) {
    publish_feedback(context, LaunchFeedback::NodeLogs(logs)).await;
    context
        .fail(failure)
        .await
        .expect("the daemon fails the goal");
}

/// The daemon ends the change cancelled, with the reason of the cancel it
/// received.
async fn end_cancelled(context: &GoalContext) {
    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the bridge's cancel reaches the daemon");
    let cause = context
        .cancel_cause()
        .expect("a cancelled goal has a cause");
    assert_eq!(cause, CancelCause::Caller);
    context
        .complete_cancelled(cause.reason(), peppylib::types::Payload::new())
        .await
        .expect("the daemon ends the goal cancelled");
}

/// The progress of a join, one message per step.
fn join_progress() -> Vec<LaunchOutput> {
    vec![
        LaunchOutput::stdout(
            "Adding so101_sim as bravo",
            LaunchFeedbackStep::LauncherStep,
        ),
        LaunchOutput::stdout(
            "fetching robot_initializer:v1",
            LaunchFeedbackStep::AddingNode,
        ),
        LaunchOutput::stderr(
            "Compiling robot_initializer",
            LaunchFeedbackStep::BuildingNode,
        ),
        LaunchOutput::stdout(
            "'bravo' does not stand yet",
            LaunchFeedbackStep::RunningNode,
        ),
    ]
}

fn join_input(copy_name: &str, option: &str) -> Value {
    json!({ "name": copy_name, "option": option, "core_node": COORDINATOR, "features": [] })
}

/// Joins `copy_name` of `option` through `bridge`, with the progress
/// window of the document.
async fn join(
    bridge: &ScopedStackCopies,
    surface: &ScriptedSurface,
    copy_name: &str,
    option: &str,
) -> Result<Value, ActionExit> {
    bridge
        .join(&join_input(copy_name, option), surface, Some(WINDOW))
        .await
}

/// Removes `copy_name` through `bridge`, with the progress window of the
/// document.
async fn remove(
    bridge: &ScopedStackCopies,
    surface: &ScriptedSurface,
    copy_name: &str,
) -> Result<Value, ActionExit> {
    bridge
        .remove(&json!({ "name": copy_name }), surface, Some(WINDOW))
        .await
}

type Call = tokio::task::JoinHandle<Result<Value, ActionExit>>;

/// Starts a join on a task of its own, so the test acts as the daemon
/// meanwhile.
fn spawn_join(
    bridge: &ScopedStackCopies,
    surface: &Arc<ScriptedSurface>,
    copy_name: &str,
    option: &str,
) -> Call {
    let bridge = bridge.clone();
    let surface = Arc::clone(surface);
    let (copy_name, option) = (copy_name.to_owned(), option.to_owned());
    tokio::spawn(async move { join(&bridge, &surface, &copy_name, &option).await })
}

/// Starts a removal on a task of its own.
fn spawn_remove(
    bridge: &ScopedStackCopies,
    surface: &Arc<ScriptedSurface>,
    copy_name: &str,
) -> Call {
    let bridge = bridge.clone();
    let surface = Arc::clone(surface);
    let copy_name = copy_name.to_owned();
    tokio::spawn(async move { remove(&bridge, &surface, &copy_name).await })
}

/// How a call the test started ended.
async fn ended(call: Call) -> Result<Value, ActionExit> {
    tokio::time::timeout(READINESS_TIMEOUT, call)
        .await
        .expect("the call ends")
        .expect("the call does not panic")
}

/// Returns once no `join` of `bridge` holds its lock.
async fn idle(bridge: &ScopedStackCopies) {
    tokio::time::timeout(READINESS_TIMEOUT, bridge.idle())
        .await
        .expect("the bridge's lock is released");
}

/// Whether no `join` of `bridge` holds its lock at this moment.
fn idle_now(bridge: &ScopedStackCopies) -> bool {
    bridge.is_idle()
}

/// A call that the bridge or the daemon refused with `kind` before the
/// change ran.
fn refused(kind: impl Into<DaemonFailureKind>, reason: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Refused(ProviderFailure::new(
        kind.into(),
        reason,
    )))
}

/// A call whose change the daemon failed with `kind`.
fn failed(kind: PeppyFailureKind, message: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Failed(ProviderFailure::new(kind, message)))
}

/// A call that ended in no end the daemon gave.
fn errored(message: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Error(message.to_owned()))
}

/// A call whose change the daemon refused because it runs another one.
fn refused_busy(subject: &str) -> Result<Value, ActionExit> {
    refused(
        DaemonFailureKind::Busy,
        &format!("the daemon refused {subject}: {DAEMON_REFUSAL}"),
    )
}

/// A call whose client cancelled `subject`, which the daemon then ended
/// cancelled.
fn cancelled(subject: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Cancelled(CancelledGoal {
        result: json!({}),
        reason: format!("{subject} was cancelled by its caller"),
    }))
}

/// Joins `copy_name` of `so101_sim` to its success. A test proves with it
/// that the bridge is free, and that no earlier join reached the daemon:
/// the daemon's next join must be this one.
async fn join_succeeds(bridge: &ScopedStackCopies, daemon: &mut FakeDaemon, copy_name: &str) {
    daemon.lists(&[]);
    let surface = Arc::new(ScriptedSurface::new());
    let call = spawn_join(bridge, &surface, copy_name, "so101_sim");
    let context = daemon.admits_join(&join_goal(copy_name, "so101_sim")).await;
    complete(&context).await;
    assert_eq!(ended(call).await, Ok(json!({})));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_answers_the_scope_and_its_copies_in_name_order() {
    let (mesh, daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    daemon.lists(&[
        copy("echo", "so101_sim"),
        copy("zulu", "web_commander"),
        copy("bravo", "openarm_sim"),
    ]);

    let listed = bridge
        .list(Duration::from_secs(5))
        .await
        .expect("the daemon lists the stack");
    assert_eq!(
        listed,
        json!({
            "options": [
                {
                    "option": "openarm_sim",
                    "model": "openarm_v2",
                    "description": "A simulated OpenArm v2 standing on the floor",
                    "features": [
                        { "feature": "ai_brain", "description": "An AI brain that finds items by words" },
                        { "feature": "recorder", "description": "A recorder of episodes" },
                    ],
                },
                {
                    "option": "so101_sim",
                    "model": "so101",
                    "description": "A simulated SO-101 clamped on the edge of a table",
                    "features": [],
                },
            ],
            "core_nodes": [COORDINATOR],
            "this_core_node": COORDINATOR,
            "copies": [
                { "name": "bravo", "option": "openarm_sim", "core_node": COORDINATOR, "features": [] },
                { "name": "echo", "option": "so101_sim", "core_node": COORDINATOR, "features": [] },
            ],
            "max_copies": 4,
        })
    );
    assert_eq!(
        daemon.list.captured()[0].message.instance_id(),
        SERVER_INSTANCE,
        "the server asks as itself"
    );
}

/// `list` reports the join or the removal that holds the stack when its
/// copy is of a scoped option, and leaves out the change of any other copy,
/// as it leaves out the copy.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_reports_the_change_of_a_scoped_copy_only() {
    let (mesh, daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    for action in [CopyAction::Join, CopyAction::Remove] {
        daemon.lists_during(&[], Some(copy_change(action, "bravo", "so101_sim")));
        let listed = bridge
            .list(Duration::from_secs(5))
            .await
            .expect("the daemon lists the stack");
        assert_eq!(
            listed["change"],
            json!({ "action": action.as_str(), "name": "bravo", "option": "so101_sim" })
        );
    }
    daemon.lists_during(
        &[copy("zulu", "web_commander")],
        Some(copy_change(CopyAction::Remove, "zulu", "web_commander")),
    );
    let listed = bridge
        .list(Duration::from_secs(5))
        .await
        .expect("the daemon lists the stack");
    assert_eq!(listed.get("change"), None, "{listed}");
}

/// A `list` while the daemon has not answered a join of the bridge waits
/// for the answer, so it reads the stack once the daemon reports the join
/// as its copy change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_waits_for_the_admission_of_a_join_of_the_bridge() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let pending = daemon
        .join
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the join reaches the daemon");

    let mut listing = Box::pin(bridge.list(Duration::from_secs(5)));
    assert!(
        pending_after_one_poll(&mut listing).await,
        "the list waits while the daemon has not answered the join"
    );
    assert_eq!(daemon.reads(), 1, "only the join read the stack");
    let joining = copy_change(CopyAction::Join, "bravo", "so101_sim");
    daemon.lists_during(&[], Some(joining));
    let context = pending
        .accept(admitted())
        .await
        .expect("the daemon admits the join");

    let listed = tokio::time::timeout(READINESS_TIMEOUT, listing)
        .await
        .expect("the list answers")
        .expect("the daemon lists the stack");
    assert_eq!(
        listed["change"],
        json!({ "action": "join", "name": "bravo", "option": "so101_sim" })
    );
    complete(&context).await;
    assert_eq!(ended(call).await, Ok(json!({})));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_reports_its_acceptance_and_each_line_then_succeeds() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("alpha", "openarm_sim")]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    // The acceptance reaches the client before any line of the join.
    surface.reported(1).await;
    assert_eq!(
        surface.feedback(),
        ["launch: the daemon accepted the addition of bravo (so101_sim)"]
    );
    for line in join_progress() {
        publish(&context, &line).await;
    }
    surface.reported(5).await;
    complete(&context).await;

    assert_eq!(ended(call).await, Ok(json!({})));
    assert_eq!(
        surface.feedback(),
        [
            "launch: the daemon accepted the addition of bravo (so101_sim)",
            "launch: Adding so101_sim as bravo",
            "add: fetching robot_initializer:v1",
            "build: Compiling robot_initializer",
            "run: 'bravo' does not stand yet",
        ]
    );
    assert_eq!(daemon.reads(), 1, "one read of the stack");
    assert!(
        idle_now(&bridge),
        "the lock goes before the call hears of the end"
    );
}

/// A join that ran and failed ends failed, with its failure kind, its
/// error and the log files: the stack log, then the log of the node that
/// failed. An undo that could not clean up says so in the error, which the
/// message keeps whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_join_ends_with_its_kind_its_error_and_its_log_files() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    let error = "instance `bravo_robot_initializer_inst` exited during its setup; cleanup failed: \
                 the stop timed out. The copy remains listed; retry peppy stack remove bravo";
    let run_log = |instance: &str, failed| NodeRunLogEntry {
        instance_id: instance.to_owned(),
        node_label: "robot_initializer:v1".to_owned(),
        log_path: PathBuf::from(format!("/home/peppy/.peppy/logs/run/{instance}.log")),
        failed,
        core_node: COORDINATOR.to_owned(),
    };
    let logs = NodeLogs {
        node_run_logs: vec![
            run_log("bravo_arm_inst", false),
            run_log("bravo_robot_initializer_inst", true),
        ],
        ..NodeLogs::default()
    };
    fail(
        &context,
        Failure::new(PeppyFailureKind::AttemptFailed, error),
        logs,
    )
    .await;

    assert_eq!(
        ended(call).await,
        failed(
            PeppyFailureKind::AttemptFailed,
            &format!(
                "the addition of bravo failed: {error}. Log files: {STACK_LOG}, \
                 /home/peppy/.peppy/logs/run/bravo_robot_initializer_inst.log"
            )
        )
    );
    assert!(idle_now(&bridge));
}

/// The daemon fails a change after acceptance for a state it finds, for
/// example a copy of that name that joined since the bridge read the stack:
/// the call ends failed with that kind and the stack log alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failure_after_acceptance_names_its_kind_and_the_stack_log() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    let error = "copy `bravo` already exists; choose another name";
    fail(
        &context,
        Failure::new(PeppyFailureKind::PreconditionFailed, error),
        NodeLogs::default(),
    )
    .await;

    assert_eq!(
        ended(call).await,
        failed(
            PeppyFailureKind::PreconditionFailed,
            &format!("the addition of bravo failed: {error}. Log files: {STACK_LOG}")
        )
    );
    assert!(idle_now(&bridge));
}

/// A name that a copy of any option has is refused from the bridge's read
/// of the stack, and no goal reaches the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_on_the_stack_is_refused_before_any_goal() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = ScriptedSurface::new();
    daemon.lists(&[copy("bravo", "web_commander")]);

    assert_eq!(
        join(&bridge, &surface, "bravo", "so101_sim").await,
        refused(
            PeppyFailureKind::PreconditionFailed,
            "a copy `bravo` is already on the stack; choose another name"
        )
    );
    assert!(
        surface.feedback().is_empty(),
        "the call reports no progress"
    );
    assert!(
        idle_now(&bridge),
        "the lock goes before the call hears of the refusal"
    );
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// At `max_copies` copies of the scope's options, a join is refused naming
/// them; copies of other options do not count.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_at_the_limit_is_refused_naming_the_copies() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = ScriptedSurface::new();
    daemon.lists(&[
        copy("echo", "so101_sim"),
        copy("bravo", "openarm_sim"),
        copy("zulu", "web_commander"),
        copy("delta", "so101_sim"),
        copy("charlie", "openarm_sim"),
    ]);
    assert_eq!(
        join(&bridge, &surface, "foxtrot", "so101_sim").await,
        refused(
            PeppyFailureKind::PreconditionFailed,
            "the stack holds 4 copies of the options openarm_sim and so101_sim (bravo, charlie, \
             delta, echo), the most the scope of this endpoint allows; remove one first"
        )
    );

    let single = mesh.bridge(single_scope());
    daemon.lists(&[copy("bravo", "so101_sim"), copy("alpha", "openarm_sim")]);
    assert_eq!(
        join(&single, &surface, "charlie", "so101_sim").await,
        refused(
            PeppyFailureKind::PreconditionFailed,
            "the stack holds 1 copy of the option so101_sim (bravo), the most the scope of this \
             endpoint allows; remove one first"
        )
    );
    assert!(idle_now(&bridge) && idle_now(&single));
    join_succeeds(&bridge, &mut daemon, "foxtrot").await;
}

/// The daemon refuses the goal at admission: the call is refused with the
/// failure kind and the reason of the daemon, and the lock is free when the
/// call hears of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_the_daemon_refuses_is_refused_with_its_kind_and_reason() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    daemon.refuses_join(busy()).await;
    assert_eq!(ended(call).await, refused_busy("the addition of bravo"));
    assert!(surface.feedback().is_empty());
    assert!(
        idle_now(&bridge),
        "the lock goes before the call hears of the refusal"
    );
}

/// The bridge runs one join at a time: a second join while the daemon runs
/// the first is refused with the daemon's busy text before it reads the
/// stack, and a join after the first ended runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_that_finds_the_lock_held_is_refused_with_the_kind_busy() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let first = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;

    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "openarm_sim").await,
        refused(DaemonFailureKind::Busy, STACK_BUSY_REASON)
    );
    assert_eq!(
        daemon.reads(),
        1,
        "the refused join does not read the stack"
    );

    complete(&context).await;
    assert!(ended(first).await.is_ok());
    assert!(idle_now(&bridge));
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A cancel after the acceptance reaches the daemon: the daemon stops the
/// join and ends it cancelled, and the call ends cancelled with its reason
/// once it did. The bridge holds its lock until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_join_reaches_the_daemon_and_ends_cancelled() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    publish(&context, &join_progress()[0]).await;
    surface.reported(2).await;

    surface.cancel.cancel();
    surface.reported(3).await;
    // The undo runs under the lock: it refuses another join.
    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "so101_sim").await,
        refused(DaemonFailureKind::Busy, STACK_BUSY_REASON)
    );
    end_cancelled(&context).await;

    assert_eq!(ended(call).await, cancelled("the addition of bravo"));
    assert_eq!(
        surface.feedback(),
        [
            "launch: the daemon accepted the addition of bravo (so101_sim)",
            "launch: Adding so101_sim as bravo",
            "launch: the client cancelled the addition of bravo; the daemon stops it and undoes it",
        ]
    );
    assert!(
        idle_now(&bridge),
        "the lock goes before the call hears of the end"
    );
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A call whose handler the runtime drops (a whole-goal deadline, the TTL
/// of its task) sends the daemon a cancel of the join. The bridge follows
/// the join to the end the daemon gives it, and holds the lock until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_call_cancels_the_join_and_holds_the_lock_until_its_end() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    surface.followed().await;

    call.abort();
    assert!(
        call.await.expect_err("the call is dropped").is_cancelled(),
        "the handler's future is gone"
    );
    publish(&context, &join_progress()[0]).await;
    assert!(!idle_now(&bridge), "the join holds the lock");
    end_cancelled(&context).await;
    idle(&bridge).await;
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A cancel before the daemon answered the goal waits for that answer: a
/// goal the daemon refuses ends the call refused, since nothing ran, and an
/// admitted one gets the cancel at once and ends cancelled.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_before_the_daemon_answers_waits_for_the_answer() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    surface.cancel.cancel();

    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    daemon.refuses_join(busy()).await;
    assert_eq!(ended(call).await, refused_busy("the addition of bravo"));

    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    end_cancelled(&context).await;
    assert_eq!(ended(call).await, cancelled("the addition of bravo"));
    assert_eq!(
        surface.feedback(),
        [
            "launch: the daemon accepted the addition of bravo (so101_sim)",
            "launch: the client cancelled the addition of bravo; the daemon stops it and undoes it",
        ]
    );
    idle(&bridge).await;
}

/// A daemon that goes away during a join fails the call, with no result to
/// report, and frees the lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_goes_away_fails_the_join_and_frees_the_lock() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    publish(&context, &join_progress()[0]).await;
    surface.reported(2).await;

    let FakeDaemon {
        list,
        join,
        remove,
        session,
    } = daemon;
    join.stop();
    remove.stop();
    drop(list);
    drop(session);

    assert_eq!(
        ended(call).await,
        errored(&format!(
            "the addition of bravo failed: Join ended without a result: its daemon is gone. Log \
             files: {STACK_LOG}"
        ))
    );
    assert!(
        idle_now(&bridge),
        "the lock goes before the call hears of the end"
    );
    drop(context);
}

/// The silence of the daemon, on a bridge whose clock the test moves: each
/// line of progress starts a new window; one whole window without a line
/// fails the call, and the bridge sends the daemon a cancel and follows the
/// join, under its lock, to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_daemon_fails_the_call_after_one_window_and_the_join_is_cancelled() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let mut paused = PausedRuntime::spawn(mesh.server.clone(), module_path!(), {
        let bridge = bridge.clone();
        let surface = Arc::clone(&surface);
        move || async move { join(&bridge, &surface, "bravo", "so101_sim").await }
    });
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    surface.followed().await;

    // Two steps of just under one window, with a line before the second:
    // no silence lasts a window, so the second line still reaches the
    // client.
    paused.advance(WITHIN_THE_WINDOW).await;
    publish(&context, &join_progress()[0]).await;
    surface.reported(2).await;
    paused.advance(WITHIN_THE_WINDOW).await;
    publish(&context, &join_progress()[1]).await;
    surface.reported(3).await;

    paused.advance(WINDOW).await;
    assert_eq!(
        paused.ended().await,
        errored("the daemon sent nothing for 660 s; the endpoint cancelled the addition of bravo")
    );
    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "so101_sim").await,
        refused(DaemonFailureKind::Busy, STACK_BUSY_REASON),
        "the join goes on under the lock"
    );
    end_cancelled(&context).await;
    idle(&bridge).await;
    drop(paused);
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_reports_each_stop_and_takes_the_copy_off() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("bravo", "so101_sim")]);

    let call = spawn_remove(&bridge, &surface, "bravo");
    let context = daemon.admits_removal("bravo").await;
    for line in [
        "Stopping instance `bravo_camera_inst` of copy `bravo`",
        "Stopping instance `bravo_arm_inst` of copy `bravo`",
        "Copy `bravo` removed",
    ] {
        publish(
            &context,
            &LaunchOutput::stdout(line, LaunchFeedbackStep::LauncherStep),
        )
        .await;
    }
    surface.reported(4).await;
    complete(&context).await;

    assert_eq!(ended(call).await, Ok(json!({})));
    assert_eq!(
        surface.feedback(),
        [
            "launch: the daemon accepted the removal of bravo",
            "launch: Stopping instance `bravo_camera_inst` of copy `bravo`",
            "launch: Stopping instance `bravo_arm_inst` of copy `bravo`",
            "launch: Copy `bravo` removed",
        ]
    );
}

/// `remove` removes a copy of a scoped option only, and sends no goal for
/// another name.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_of_a_name_that_is_no_scoped_copy_is_refused() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    for refused_name in ["zulu", "nobody"] {
        daemon.lists(&[copy("zulu", "web_commander"), copy("bravo", "so101_sim")]);
        assert_eq!(
            remove(&bridge, &surface, refused_name).await,
            refused(
                PeppyFailureKind::Rejected,
                &format!(
                    "no copy `{refused_name}` of the options openarm_sim and so101_sim on the \
                     stack"
                )
            )
        );
    }
    let single = mesh.bridge(single_scope());
    daemon.lists(&[copy("alpha", "openarm_sim")]);
    assert_eq!(
        remove(&single, &surface, "alpha").await,
        refused(
            PeppyFailureKind::Rejected,
            "no copy `alpha` of the option so101_sim on the stack"
        )
    );

    // The daemon's next removal is the first one the bridge sends.
    daemon.lists(&[copy("bravo", "so101_sim")]);
    let call = spawn_remove(&bridge, &surface, "bravo");
    let context = daemon.admits_removal("bravo").await;
    complete(&context).await;
    assert!(ended(call).await.is_ok());
}

/// A removal that the daemon fails after acceptance, for example a copy
/// that the stack links to, ends failed with its kind and the stack log.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_the_daemon_fails_after_acceptance_ends_with_its_kind() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("bravo", "so101_sim")]);

    let call = spawn_remove(&bridge, &surface, "bravo");
    let context = daemon.admits_removal("bravo").await;
    let error = "removing `bravo` would leave `recorder_inst` without a member in `arms`";
    fail(
        &context,
        Failure::new(PeppyFailureKind::PreconditionFailed, error),
        NodeLogs::default(),
    )
    .await;

    assert_eq!(
        ended(call).await,
        failed(
            PeppyFailureKind::PreconditionFailed,
            &format!("the removal of bravo failed: {error}. Log files: {STACK_LOG}")
        )
    );
}

/// A removal takes no lock of the bridge: while a join runs, the removal
/// reaches the daemon, which refuses it with its busy kind and text. A
/// cancelled removal reaches the daemon as a cancelled join does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_takes_no_lock_and_a_cancel_reaches_the_daemon() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("alpha", "so101_sim")]);
    let joining = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let join_context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;

    daemon.lists(&[copy("alpha", "so101_sim")]);
    let removing = spawn_remove(&bridge, &Arc::new(ScriptedSurface::new()), "alpha");
    daemon
        .remove
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the removal reaches the daemon")
        .refuse(busy())
        .await
        .expect("the daemon refuses the removal");
    assert_eq!(ended(removing).await, refused_busy("the removal of alpha"));
    complete(&join_context).await;
    assert!(ended(joining).await.is_ok());

    let cancelling = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("alpha", "so101_sim")]);
    let removing = spawn_remove(&bridge, &cancelling, "alpha");
    let context = daemon.admits_removal("alpha").await;
    cancelling.followed().await;
    cancelling.cancel.cancel();
    end_cancelled(&context).await;
    assert_eq!(ended(removing).await, cancelled("the removal of alpha"));
}

// --- Features and machines.

/// The join the bridge sends for `goal_input`, admitted and ended, so the
/// test reads the exact goal the daemon decodes.
async fn sent_join(
    bridge: &ScopedStackCopies,
    daemon: &mut FakeDaemon,
    goal_input: Value,
) -> StackJoinGoal {
    daemon.lists(&[]);
    let surface = Arc::new(ScriptedSurface::new());
    let call = {
        let bridge = bridge.clone();
        let surface = Arc::clone(&surface);
        tokio::spawn(async move { bridge.join(&goal_input, &*surface, Some(WINDOW)).await })
    };
    let pending = daemon
        .join
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the join reaches the daemon");
    let goal = StackJoinGoal::decode(pending.request_bytes()).expect("the join decodes");
    let context = pending
        .accept(admitted())
        .await
        .expect("the daemon admits the join");
    complete(&context).await;
    ended(call).await.expect("the join succeeds");
    goal
}

/// The bridge sends the copy on its machine, with the word of each feature:
/// a machine equal to this one travels as the coordinator, another one by
/// its name, and a feature on the copy's machine names no machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_sends_the_word_of_each_feature_on_its_machine() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let goal = |copy: &str, words: &[&str]| StackJoinGoal {
        selections: words.iter().map(|word| word.parse().unwrap()).collect(),
        ..StackJoinGoal::new(copy.parse().unwrap(), DEFAULT_BUDGETS)
    };
    for (input, expected) in [
        (
            json!({ "name": "alpha", "option": "openarm_sim", "core_node": COORDINATOR, "features": [] }),
            goal("openarm_sim:alpha", &[]),
        ),
        (
            json!({ "name": "bravo", "option": "openarm_sim", "core_node": "cn-jetson-1", "features": [] }),
            goal("openarm_sim:bravo@cn-jetson-1", &[]),
        ),
        (
            json!({ "name": "charlie", "option": "openarm_sim", "core_node": COORDINATOR,
                    "features": [{ "feature": "ai_brain", "core_node": "cn-gpu-box" }] }),
            goal("openarm_sim:charlie", &["brain=ai_brain_vla@cn-gpu-box"]),
        ),
        (
            json!({ "name": "delta", "option": "openarm_sim", "core_node": "cn-jetson-1",
                    "features": [
                        { "feature": "recorder", "core_node": "cn-jetson-1" },
                        { "feature": "ai_brain", "core_node": COORDINATOR },
                    ] }),
            goal(
                "openarm_sim:delta@cn-jetson-1",
                &["recorder=lerobot_recorder", "brain=ai_brain_vla@self"],
            ),
        ),
        (
            json!({ "name": "echo", "option": "openarm_sim", "core_node": COORDINATOR,
                    "features": [{ "feature": "ai_brain", "core_node": COORDINATOR }] }),
            goal("openarm_sim:echo", &["brain=ai_brain_vla"]),
        ),
    ] {
        assert_eq!(
            sent_join(&bridge, &mut daemon, input.clone()).await,
            expected,
            "{input}"
        );
    }
}

/// A feature of another option, and a feature named twice, are refused
/// before the bridge reads the stack or sends a goal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feature_outside_the_option_or_named_twice_is_refused_before_the_daemon() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = ScriptedSurface::new();
    for (input, reason) in [
        (
            json!({ "name": "foxtrot", "option": "so101_sim", "core_node": COORDINATOR,
                    "features": [{ "feature": "ai_brain", "core_node": COORDINATOR }] }),
            "`ai_brain` is not a feature of option `so101_sim`, which has no feature",
        ),
        (
            json!({ "name": "golf", "option": "openarm_sim", "core_node": COORDINATOR,
                    "features": [{ "feature": "camera", "core_node": COORDINATOR }] }),
            "`camera` is not a feature of option `openarm_sim`; its features: ai_brain and \
             recorder",
        ),
        (
            json!({ "name": "hotel", "option": "openarm_sim", "core_node": COORDINATOR,
                    "features": [
                        { "feature": "ai_brain", "core_node": COORDINATOR },
                        { "feature": "ai_brain", "core_node": "cn-gpu-box" },
                    ] }),
            "`features` names feature `ai_brain` twice; name each feature once",
        ),
    ] {
        assert_eq!(
            bridge.join(&input, &surface, Some(WINDOW)).await,
            refused(PeppyFailureKind::Rejected, reason),
            "{input}"
        );
    }
    assert_eq!(daemon.reads(), 0, "no refusal reads the stack");
    assert!(idle_now(&bridge), "no refusal takes the lock");
    join_succeeds(&bridge, &mut daemon, "india").await;
}

/// `list` gives each robot the features its selections hold, in the
/// scope's order, each with the machine of its part: the robot's machine
/// when the part runs with it. A part that no listed feature selects, and a
/// word on a feature's axis that selects another option, give no feature.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_gives_each_robot_its_features_and_their_machines() {
    let (mesh, daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    daemon.lists(&[
        copy_on(
            "delta",
            "openarm_sim",
            COORDINATOR,
            &["brain=ai_brain_vla@cn-gpu-box", "recorder=lerobot_recorder"],
        ),
        // A robot the command line added with `--with ai_brain_vla@cn-gpu-box`.
        copy_on(
            "echo",
            "openarm_sim",
            "cn-jetson-1",
            &["hardware_version=v2", "brain=ai_brain_vla@cn-gpu-box"],
        ),
        copy_on(
            "golf",
            "openarm_sim",
            COORDINATOR,
            &["camera_rig=cameras_sim@cn-gpu-box", "brain=other_brain"],
        ),
    ]);
    let listed = bridge
        .list(Duration::from_secs(5))
        .await
        .expect("the daemon lists the stack");
    assert_eq!(
        listed["copies"],
        json!([
            { "name": "delta", "option": "openarm_sim", "core_node": COORDINATOR, "features": [
                { "feature": "ai_brain", "core_node": "cn-gpu-box" },
                { "feature": "recorder", "core_node": COORDINATOR },
            ] },
            { "name": "echo", "option": "openarm_sim", "core_node": "cn-jetson-1", "features": [
                { "feature": "ai_brain", "core_node": "cn-gpu-box" },
            ] },
            { "name": "golf", "option": "openarm_sim", "core_node": COORDINATOR, "features": [] },
        ])
    );
}

/// `core_nodes` is this machine and every machine presence lists, each
/// once, in name order, read on every call: a daemon that joins the
/// federation after a first `list` is in the next one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_reads_the_live_machines_on_every_call() {
    let (mesh, daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    daemon.lists(&[]);
    let listed = bridge
        .list(Duration::from_secs(5))
        .await
        .expect("the daemon lists the stack");
    assert_eq!(listed["core_nodes"], json!([COORDINATOR]));
    assert_eq!(listed["this_core_node"], json!(COORDINATOR));

    let watch = peppylib::CoreNodePresenceMessenger::watch(&mesh.server, "cn-gpu-box")
        .await
        .expect("the server watches the machine");
    let mut tokens = Vec::new();
    for generation in ["gen-1", "gen-2"] {
        tokens.push(
            peppylib::CoreNodePresenceMessenger::declare(&daemon.session, "cn-gpu-box", generation)
                .await
                .expect("the machine declares its presence"),
        );
        let event = tokio::time::timeout(READINESS_TIMEOUT, watch.rx.recv_async())
            .await
            .expect("the presence arrives")
            .expect("the watch is open");
        assert!(
            matches!(event, peppylib::LivelinessEvent::Alive(_)),
            "{event:?}"
        );
    }
    tokens.push(
        peppylib::CoreNodePresenceMessenger::declare(&daemon.session, COORDINATOR, "gen-1")
            .await
            .expect("this machine declares its presence"),
    );
    let own = peppylib::CoreNodePresenceMessenger::watch(&mesh.server, COORDINATOR)
        .await
        .expect("the server watches its own machine");
    tokio::time::timeout(READINESS_TIMEOUT, own.rx.recv_async())
        .await
        .expect("its presence arrives")
        .expect("the watch is open");
    daemon.lists(&[]);
    let listed = bridge
        .list(Duration::from_secs(5))
        .await
        .expect("the daemon lists the stack");
    assert_eq!(
        listed["core_nodes"],
        json!(["cn-coordinator", "cn-gpu-box"]),
        "a machine two daemons claim, and this machine, are listed once"
    );
    drop(tokens);
}

/// A presence read that fails fails `list`, with what failed.
#[test]
fn a_failed_presence_read_fails_the_listing() {
    let error = super::stack_copies::live_machines_or_refusal(Err(peppylib::PeppyError::Io(
        std::io::Error::other("the session is closed"),
    )))
    .expect_err("the listing fails");
    assert_eq!(
        error.to_string(),
        peppy_mcp_runtime::ToolCallError::Error(
            "the live machines of the federation could not be read: the session is closed"
                .to_owned()
        )
        .to_string()
    );
}

/// A join the daemon ends with a failure, such as a machine that is not
/// live, ends failed with the daemon's failure kind and reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_on_a_machine_that_is_not_live_ends_with_the_daemons_reason() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    daemon.lists(&[]);
    let surface = Arc::new(ScriptedSurface::new());
    let input =
        json!({ "name": "kilo", "option": "openarm_sim", "core_node": "cn-gone", "features": [] });
    let call = {
        let bridge = bridge.clone();
        let surface = Arc::clone(&surface);
        tokio::spawn(async move { bridge.join(&input, &*surface, Some(WINDOW)).await })
    };
    let context = daemon
        .admits_join(&StackJoinGoal::new(
            "openarm_sim:kilo@cn-gone".parse().unwrap(),
            DEFAULT_BUDGETS,
        ))
        .await;
    let reason = "these core nodes are not live on the federation: `cn-gone`. Live right now: \
                  `cn-coordinator`.";
    fail(
        &context,
        Failure::new(PeppyFailureKind::ResourceUnavailable, reason),
        NodeLogs::default(),
    )
    .await;
    let Err(ActionExit::Failed(failure)) = ended(call).await else {
        panic!("a failed join ends failed");
    };
    assert_eq!(
        failure.kind().as_str(),
        PeppyFailureKind::ResourceUnavailable.name()
    );
    assert!(failure.message().contains(reason), "{failure}");
}

// --- The scopes the server starts with.

fn targets() -> BTreeMap<String, DaemonInterface> {
    BTreeMap::from([("stack".to_owned(), DaemonInterface::StackCopies)])
}

fn scopes(entries: &[(&str, Value)]) -> BTreeMap<String, Value> {
    entries
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect()
}

fn scope_refusal(scopes: &BTreeMap<String, Value>) -> String {
    match DaemonScopes::parse(SERVER_INSTANCE, &targets(), scopes) {
        Ok(_) => panic!("the server must not start"),
        Err(error @ ServeError::DaemonScopes { .. }) => error.to_string(),
        Err(other) => panic!("expected a scope refusal, got {other}"),
    }
}

/// A server whose daemon target has no scope stops, and says which target
/// lacks one and how to give it.
#[test]
fn a_server_without_a_scope_does_not_start() {
    assert_eq!(
        scope_refusal(&BTreeMap::new()),
        "the daemon targets cannot be served:\n  - daemon target `stack` (stack_copies:v1) has \
         no scope; give it one in the `daemon_scopes` of instance `framework_controls_inst`, or \
         with a `set_daemon_scopes` adjustment on `framework_controls_inst` in the launcher\nA \
         launch and a join check every scope before they start the server; a server started \
         another way, for example with `peppy node run`, gets its scopes from the runtime \
         configuration of its instance only"
    );
}

/// A scope that does not parse and a scope that names no daemon target stop
/// the server, every problem at once.
#[test]
fn a_server_with_a_bad_scope_does_not_start() {
    let refusal = scope_refusal(&scopes(&[
        ("stack", json!({ "max_copies": 4, "options": [] })),
        ("recorder", json!({})),
    ]));
    for expected in [
        "\n  - the scope of daemon target `stack` (stack_copies:v1) does not parse: `options` \
         names no option",
        "; correct it in the `daemon_scopes` of instance `framework_controls_inst`, or in the \
         `set_daemon_scopes` adjustment on `framework_controls_inst` in the launcher",
        "\n  - `daemon_scopes.recorder` of instance `framework_controls_inst` names no daemon \
         target of this server; its daemon targets: `stack`",
    ] {
        assert!(
            refusal.contains(expected),
            "{expected}\nmissing from: {refusal}"
        );
    }

    DaemonScopes::parse(
        SERVER_INSTANCE,
        &targets(),
        &scopes(&[("stack", simulation_scope())]),
    )
    .expect("a scope that parses starts the server");
}

// --- The bridge behind a real endpoint.

/// `framework_controls:v1` served as `serve` serves it, on a loopback port:
/// validated against the interface, prepared, its schemas narrowed by the
/// scope, and each entry bound to the daemon bridge.
struct Endpoint {
    url: String,
    shutdown: CancellationToken,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Endpoint {
    async fn serve(mesh: &Mesh) -> Self {
        let document =
            PeppyMcpExposureParser::from_content(FRAMEWORK_CONTROLS).expect("the fixture parses");
        let validated =
            build_exposure_bundle(&document, &[], &[DaemonInterface::StackCopies.resolved()])
                .expect("the fixture validates against the interface");
        let mut exposure = prepare(vec![validated])
            .expect("the exposure prepares")
            .remove(0);
        assert!(
            exposure.tools.is_empty() && exposure.tasks.is_empty(),
            "no entry goes to a node bridge"
        );
        let scopes = DaemonScopes::parse(
            SERVER_INSTANCE,
            &targets(),
            &scopes(&[("stack", simulation_scope())]),
        )
        .expect("the scope parses");
        let daemon = DaemonBridges::new(mesh.own_daemon(), scopes);
        daemon.narrow(&mut exposure.bundle);
        let mut builder = ExposureServer::builder(exposure.bundle);
        for tool in &exposure.daemon_tools {
            builder = builder.with_tool(tool.name.clone(), daemon.tool(tool).expect("binds"));
        }
        for task in &exposure.daemon_tasks {
            builder = builder.with_task(task.name.clone(), daemon.task(task).expect("binds"));
        }
        let server = builder.build().expect("the catalog and the handlers agree");
        let path = server.endpoint_path();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("an OS-assigned loopback port binds");
        let address = listener
            .local_addr()
            .expect("a bound listener has an address");
        let set = ExposureSet::new(vec![server]).expect("one exposure composes");
        let shutdown = CancellationToken::new();
        let served = tokio::spawn(set.serve(listener, shutdown.clone()));
        Self {
            url: format!("http://{address}{path}"),
            shutdown,
            served,
        }
    }

    /// Calls `tool` with `arguments` as a client without the tasks
    /// extension, with `progress_token` when one is given. Returns the
    /// event stream of the call once its headers arrive.
    fn open_call(
        &self,
        tool: &str,
        arguments: Value,
        progress_token: Option<&str>,
    ) -> impl Future<Output = Events> + Send + 'static {
        let mut meta = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        if let Some(token) = progress_token {
            meta["progressToken"] = json!(token);
        }
        let request = reqwest::Client::new()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", tool)
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": tool, "arguments": arguments, "_meta": meta }
                })
                .to_string(),
            );
        async move {
            let response = tokio::time::timeout(READINESS_TIMEOUT, request.send())
                .await
                .expect("the response headers arrive")
                .expect("the call opens");
            Events::of(response)
        }
    }

    /// Calls `tool` with `arguments` as a client without the tasks
    /// extension and without a progress token, and returns the JSON-RPC
    /// answer.
    async fn call(&self, tool: &str, arguments: Value) -> Value {
        let mut events = self.open_call(tool, arguments, None).await;
        loop {
            let message = events.next_message().await;
            if message.get("id").is_some() {
                return message;
            }
        }
    }

    /// The outcome and the message the call record keeps of each call,
    /// oldest first.
    async fn recorded(&self) -> Vec<(String, Value)> {
        let answer = self.call("stack.recent_calls", json!({})).await;
        let calls = answer["result"]["structuredContent"]["calls"]
            .as_array()
            .unwrap_or_else(|| panic!("the record answers its calls: {answer}"))
            .clone();
        calls
            .iter()
            .rev()
            .map(|call| {
                (
                    call["outcome"].as_str().expect("an outcome").to_owned(),
                    call["message"].clone(),
                )
            })
            .collect()
    }

    /// The outcome and the message of the newest call of the record, once
    /// that call is no longer running. The wait is bounded by
    /// [`READINESS_TIMEOUT`] and driven by the endpoint's answers.
    async fn ended_newest_call(&self) -> (String, Value) {
        tokio::time::timeout(READINESS_TIMEOUT, async {
            loop {
                let newest = self.recorded().await.pop();
                if let Some(call) = newest.filter(|(outcome, _)| outcome != "running") {
                    return call;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the newest call ends")
    }

    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(READINESS_TIMEOUT, self.served)
            .await
            .expect("the serve task ends once the token is cancelled")
            .expect("the serve task does not panic")
            .expect("serving the endpoint succeeds");
    }
}

/// Input outside the narrowed schema never reaches the bridge, and the
/// record writes it `refused`; a refusal of the bridge is a goal its
/// provider rejected, `failed`; a join that ran completes.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_endpoint_refuses_input_outside_the_scope_and_records_each_outcome() {
    let (mesh, mut daemon) = mesh().await;
    let endpoint = Endpoint::serve(&mesh).await;

    let goal = |name: &str, option: &str, core_node: &str, features: Value| json!({ "name": name, "option": option, "core_node": core_node, "features": features });
    for (tool, arguments) in [
        (
            "stack.join",
            goal("bravo", "web_commander", COORDINATOR, json!([])),
        ),
        (
            "stack.join",
            goal("self", "so101_sim", COORDINATOR, json!([])),
        ),
        (
            "stack.join",
            goal("bad/name", "so101_sim", COORDINATOR, json!([])),
        ),
        ("stack.join", goal("bravo", "so101_sim", "self", json!([]))),
        (
            "stack.join",
            json!({ "name": "bravo", "option": "so101_sim" }),
        ),
        (
            "stack.join",
            goal(
                "bravo",
                "openarm_sim",
                COORDINATOR,
                json!([{ "feature": "camera", "core_node": COORDINATOR }]),
            ),
        ),
        ("stack.remove", json!({ "name": "self" })),
    ] {
        let answer = endpoint.call(tool, arguments.clone()).await;
        assert!(
            answer.get("error").is_some(),
            "{tool} {arguments} is outside the narrowed schema: {answer}"
        );
    }
    assert_eq!(daemon.reads(), 0, "no refused input reaches the bridge");

    // A feature the scope offers, on an option that does not offer it: the
    // schema admits it, and the bridge refuses it before the daemon.
    let answer = endpoint
        .call(
            "stack.join",
            goal(
                "bravo",
                "so101_sim",
                COORDINATOR,
                json!([{ "feature": "ai_brain", "core_node": COORDINATOR }]),
            ),
        )
        .await;
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "the provider refused the goal: rejected: `ai_brain` is not a feature of option \
         `so101_sim`, which has no feature"
    );
    assert_eq!(
        daemon.reads(),
        0,
        "the bridge refuses a feature before it reads the stack"
    );

    daemon.lists(&[copy("bravo", "so101_sim")]);
    let answer = endpoint
        .call(
            "stack.join",
            goal("bravo", "openarm_sim", COORDINATOR, json!([])),
        )
        .await;
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "the provider refused the goal: precondition_failed: a copy `bravo` is already on the \
         stack; choose another name"
    );

    daemon.lists(&[copy_on(
        "bravo",
        "openarm_sim",
        "cn-jetson-1",
        &["brain=ai_brain_vla@cn-gpu-box"],
    )]);
    let listed = endpoint.call("stack.list", json!({})).await;
    let copies = &listed["result"]["structuredContent"]["copies"];
    assert_eq!(
        copies,
        &json!([{
            "name": "bravo",
            "option": "openarm_sim",
            "core_node": "cn-jetson-1",
            "features": [{ "feature": "ai_brain", "core_node": "cn-gpu-box" }],
        }]),
        "{listed}"
    );
    // A listed robot is a valid join input: the schema admits it, and the
    // bridge refuses it only because its name is taken.
    daemon.lists(&[copy("bravo", "so101_sim")]);
    let answer = endpoint.call("stack.join", copies[0].clone()).await;
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "the provider refused the goal: precondition_failed: a copy `bravo` is already on the \
         stack; choose another name",
        "{answer}"
    );

    daemon.lists(&[]);
    let call = endpoint.call(
        "stack.join",
        goal("charlie", "so101_sim", COORDINATOR, json!([])),
    );
    let daemon_side = async {
        let context = daemon.admits_join(&join_goal("charlie", "so101_sim")).await;
        complete(&context).await;
        context
    };
    let (joined, _context) = tokio::join!(call, daemon_side);
    assert_eq!(
        joined["result"]["structuredContent"],
        json!({}),
        "a completed join gives no result: {joined}"
    );

    let recorded = endpoint.recorded().await;
    let outcomes: Vec<&str> = recorded
        .iter()
        .map(|(outcome, _)| outcome.as_str())
        .collect();
    assert_eq!(
        outcomes,
        [
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "refused",
            "completed"
        ],
        "{recorded:?}"
    );
    assert_eq!(
        recorded[7].1,
        "`ai_brain` is not a feature of option `so101_sim`, which has no feature"
    );
    assert_eq!(
        recorded[8].1,
        "a copy `bravo` is already on the stack; choose another name"
    );
    endpoint.stop().await;
}

/// The daemon's refusal of a join and of a removal while other work holds
/// its stack, and the endpoint's refusal of a second join, reach the client
/// as a tool error with the kind `busy`, which `stack_copies:v1` declares,
/// and the record keeps each call refused with that kind.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_busy_refusal_reaches_the_client_with_the_kind_busy() {
    let (mesh, mut daemon) = mesh().await;
    let endpoint = Endpoint::serve(&mesh).await;
    let join_input = |name: &str| json!({ "name": name, "option": "so101_sim", "core_node": COORDINATOR, "features": [] });
    let busy_answer = |text: &str, message: &str| {
        json!({
            "text": text,
            "structured": { "outcome": "refused", "failure_kind": "busy", "message": message },
        })
    };
    let answer_of = |answer: &Value| {
        assert_eq!(answer["result"]["isError"], true, "{answer}");
        json!({
            "text": answer["result"]["content"][0]["text"],
            "structured": answer["result"]["structuredContent"],
        })
    };

    daemon.lists(&[]);
    let (joined, ()) = tokio::join!(endpoint.call("stack.join", join_input("bravo")), async {
        daemon.refuses_join(busy()).await
    });
    let message = format!("the daemon refused the addition of bravo: {DAEMON_REFUSAL}");
    assert_eq!(
        answer_of(&joined),
        busy_answer(
            &format!("the provider refused the goal: busy: {message}"),
            &message
        )
    );

    daemon.lists(&[copy("alpha", "so101_sim")]);
    let (removed, ()) = tokio::join!(
        endpoint.call("stack.remove", json!({ "name": "alpha" })),
        async {
            daemon
                .remove
                .next_goal(READINESS_TIMEOUT)
                .await
                .expect("the removal reaches the daemon")
                .refuse(busy())
                .await
                .expect("the daemon refuses the removal");
        }
    );
    let message = format!("the daemon refused the removal of alpha: {DAEMON_REFUSAL}");
    assert_eq!(
        answer_of(&removed),
        busy_answer(
            &format!("the provider refused the goal: busy: {message}"),
            &message
        )
    );

    daemon.lists(&[]);
    let first = endpoint.call("stack.join", join_input("charlie"));
    let second = async {
        let context = daemon.admits_join(&join_goal("charlie", "so101_sim")).await;
        let second = endpoint.call("stack.join", join_input("delta")).await;
        complete(&context).await;
        second
    };
    let (first, second) = tokio::join!(first, second);
    assert_eq!(first["result"]["structuredContent"], json!({}), "{first}");
    assert_eq!(
        answer_of(&second),
        busy_answer(
            &format!("the provider refused the goal: busy: {STACK_BUSY_REASON}"),
            STACK_BUSY_REASON
        )
    );

    let answer = endpoint.call("stack.recent_calls", json!({})).await;
    let kinds: Vec<(Value, Value)> = answer["result"]["structuredContent"]["calls"]
        .as_array()
        .unwrap_or_else(|| panic!("the record answers its calls: {answer}"))
        .iter()
        .rev()
        .map(|call| (call["outcome"].clone(), call["failure_kind"].clone()))
        .collect();
    assert_eq!(
        kinds,
        [
            (json!("refused"), json!("busy")),
            (json!("refused"), json!("busy")),
            (json!("completed"), Value::Null),
            (json!("refused"), json!("busy")),
        ],
        "{answer}"
    );
    endpoint.stop().await;
}

/// A client cancels a call by closing it: a closed `join` sends the daemon
/// a cancel, the daemon undoes the change, and the record keeps the call
/// cancelled with the daemon's reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_a_join_call_cancels_the_daemons_goal() {
    let (mesh, mut daemon) = mesh().await;
    let endpoint = Endpoint::serve(&mesh).await;

    daemon.lists(&[]);
    // The headers of a call with a progress token come with its first
    // progress, once the daemon admitted the join.
    let opening = tokio::spawn(endpoint.open_call(
        "stack.join",
        json!({ "name": "bravo", "option": "so101_sim", "core_node": COORDINATOR, "features": [] }),
        Some("join-1"),
    ));
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    let mut events = opening.await.expect("the call does not panic");
    let accepted = events.next_message().await;
    assert_eq!(accepted["method"], "notifications/progress", "{accepted}");
    assert_eq!(
        accepted["params"]["message"],
        "launch: the daemon accepted the addition of bravo (so101_sim)"
    );
    drop(events);

    end_cancelled(&context).await;
    assert_eq!(
        endpoint.ended_newest_call().await,
        (
            "cancelled".to_owned(),
            json!("the addition of bravo was cancelled by its caller")
        )
    );
    endpoint.stop().await;
}
