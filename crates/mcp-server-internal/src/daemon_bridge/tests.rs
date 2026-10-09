//! The daemon bridge against a fake daemon: an ephemeral router, and a
//! fake core node that serves `stack_list` and runs `stack_join` and
//! `stack_remove` on peppylib's production goal engine, answering each when
//! the test tells it to. Every step waits on a peer's action, never on the
//! clock: a deadline only bounds the failure path. The progress window is
//! tested on a bridge whose clock moves only when the test advances it (see
//! [`PausedRuntime`]), and the bridge behind a real endpoint over HTTP at the
//! end of this file.

use super::stack_copies::ScopedStackCopies;
use super::stack_copies::StackCopies;
use super::{DaemonBridges, DaemonScopes, OwnDaemon};
use crate::bridges::prepare;
use crate::serve::ServeError;
use crate::test_support::{Events, PausedRuntime, ScriptedSurface};
use config::runtime::{CoreNodeName, Name};
use core_node_api::encoding::{
    CopyInfo, LaunchFeedback, LaunchFeedbackStep, LaunchGoalResponse, LaunchResult,
    NodeRunLogEntry, STACK_BUSY_REASON, StackJoinGoal, StackListRequest, StackListResponse,
    StackRemoveGoal,
};
use core_node_api::names::CORE_NODE_TAG;
use core_node_api::{ActionGoal, ServiceRequest};
use daemon_config::daemon_interface::{DaemonInterface, StackCopiesScope};
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use peppy_mcp_catalog::build_exposure_bundle;
use peppy_mcp_runtime::{ActionExit, CancelledGoal, ExposureServer, ExposureSet};
use peppylib::messaging::{GoalContext, MessengerHandle, ProducerRef, SenderTarget};
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

/// The scope `simulation_mcp` gives its framework endpoint.
fn simulation_scope() -> Value {
    json!({
        "max_copies": 4,
        "options": [
            { "option": "openarm_sim", "description": "A simulated OpenArm v2 standing on the floor" },
            { "option": "so101_sim", "description": "A simulated SO-101 clamped on the edge of a table" },
        ],
    })
}

/// A scope of one option, `so101_sim`, and one copy at most.
fn single_scope() -> Value {
    json!({
        "max_copies": 1,
        "options": [{ "option": "so101_sim", "description": "A simulated SO-101" }],
    })
}

fn scope_of(value: Value) -> StackCopiesScope {
    serde_json::from_value(value).expect("the scope parses")
}

fn name(name: &str) -> Name {
    Name::new(name).expect("a name")
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
fn copy(copy_name: &str, option: &str) -> CopyInfo {
    CopyInfo {
        name: name(copy_name),
        core_node: CoreNodeName::new(COORDINATOR).expect("a core node name"),
        instance_ids: vec![name(&format!("{copy_name}_arm_inst"))],
        selections: Vec::new(),
        option: option.to_owned(),
        set_members: Vec::new(),
    }
}

fn admitted() -> peppylib::types::Payload {
    LaunchGoalResponse::accepted(STACK_LOG)
        .encode()
        .expect("the admission encodes")
}

fn rejected(reason: &str) -> peppylib::types::Payload {
    LaunchGoalResponse::rejected(reason)
        .encode()
        .expect("the rejection encodes")
}

impl FakeDaemon {
    /// The daemon answers the next read of its stack with `copies`.
    fn lists(&self, copies: &[CopyInfo]) {
        let mut answer = StackListResponse::new("{}", COORDINATOR, DAEMON_INSTANCE, "host");
        answer.copies = copies.to_vec();
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

    /// Rejects the next join with `reason`.
    async fn rejects_join(&mut self, reason: &str) {
        self.join
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the join reaches the daemon")
            .reject(None, rejected(reason))
            .await
            .expect("the daemon rejects the join");
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
            StackRemoveGoal::new(name(copy_name))
        );
        pending
            .accept(admitted())
            .await
            .expect("the daemon admits the removal")
    }
}

/// The join the bridge sends for `copy_name` of `option`: no selection, no
/// argument, no environment, placed on the coordinator, under the default
/// budgets.
fn join_goal(copy_name: &str, option: &str) -> StackJoinGoal {
    StackJoinGoal::new(name(copy_name), option, DEFAULT_BUDGETS)
}

async fn publish(context: &GoalContext, feedback: &LaunchFeedback) {
    context
        .publish_feedback(feedback.encode().expect("the feedback encodes"))
        .await
        .expect("the daemon publishes its feedback");
}

async fn complete(context: &GoalContext, result: &LaunchResult) {
    context
        .complete(result.encode().expect("the result encodes"))
        .await
        .expect("the daemon ends the goal");
}

/// The progress of a join, one message per step.
fn join_progress() -> Vec<LaunchFeedback> {
    vec![
        LaunchFeedback::stdout(
            "Adding so101_sim as bravo",
            LaunchFeedbackStep::LauncherStep,
        ),
        LaunchFeedback::stdout(
            "fetching robot_initializer:v1",
            LaunchFeedbackStep::AddingNode,
        ),
        LaunchFeedback::stderr(
            "Compiling robot_initializer",
            LaunchFeedbackStep::BuildingNode,
        ),
        LaunchFeedback::stdout(
            "'bravo' does not stand yet",
            LaunchFeedbackStep::RunningNode,
        ),
    ]
}

fn join_input(copy_name: &str, option: &str) -> Value {
    json!({ "name": copy_name, "option": option })
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

fn refused(reason: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Failed(reason.to_owned()))
}

/// A call whose client stopped waiting for `subject`.
fn stopped_waiting(subject: &str) -> Result<Value, ActionExit> {
    Err(ActionExit::Cancelled(CancelledGoal {
        result: json!({
            "success": false,
            "message": format!("stopped waiting; {subject} continues on the stack"),
        }),
        reason: None,
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
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
    assert_eq!(
        ended(call).await,
        Ok(json!({
            "success": true,
            "message": format!("{copy_name} (so101_sim) is on the stack"),
        }))
    );
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
                { "option": "openarm_sim", "description": "A simulated OpenArm v2 standing on the floor" },
                { "option": "so101_sim", "description": "A simulated SO-101 clamped on the edge of a table" },
            ],
            "copies": [
                { "name": "bravo", "option": "openarm_sim" },
                { "name": "echo", "option": "so101_sim" },
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
    complete(&context, &LaunchResult::success(STACK_LOG)).await;

    assert_eq!(
        ended(call).await,
        Ok(json!({ "success": true, "message": "bravo (so101_sim) is on the stack" }))
    );
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

/// A join that ran and failed completes with `success` false, its error
/// and the log files: the stack log, then the log of the node that failed.
/// An undo that could not clean up says so in the error, which the message
/// keeps whole.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_join_completes_with_its_error_and_its_log_files() {
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
    let failed = LaunchResult::failure(STACK_LOG, error).with_node_logs(
        Vec::new(),
        Vec::new(),
        vec![
            run_log("bravo_arm_inst", false),
            run_log("bravo_robot_initializer_inst", true),
        ],
    );
    complete(&context, &failed).await;

    assert_eq!(
        ended(call).await,
        Ok(json!({
            "success": false,
            "message": format!(
                "the addition of bravo failed: {error}. Log files: {STACK_LOG}, \
                 /home/peppy/.peppy/logs/run/bravo_robot_initializer_inst.log"
            ),
        }))
    );
    assert!(idle_now(&bridge));
}

/// The daemon refuses after acceptance, for example a copy of that name that
/// joined since the bridge read the stack: the call completes with
/// `success` false and the stack log alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refusal_after_acceptance_completes_with_success_false() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    let error = "copy `bravo` already exists; choose another name";
    complete(&context, &LaunchResult::failure(STACK_LOG, error)).await;

    assert_eq!(
        ended(call).await,
        Ok(json!({
            "success": false,
            "message": format!("the addition of bravo failed: {error}. Log files: {STACK_LOG}"),
        }))
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
        refused("a copy `bravo` is already on the stack; choose another name")
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
            "the stack holds 4 copies of the options openarm_sim and so101_sim (bravo, charlie, \
             delta, echo), the most the scope of this endpoint allows; remove one first"
        )
    );

    let single = mesh.bridge(single_scope());
    daemon.lists(&[copy("bravo", "so101_sim"), copy("alpha", "openarm_sim")]);
    assert_eq!(
        join(&single, &surface, "charlie", "so101_sim").await,
        refused(
            "the stack holds 1 copy of the option so101_sim (bravo), the most the scope of this \
             endpoint allows; remove one first"
        )
    );
    assert!(idle_now(&bridge) && idle_now(&single));
    join_succeeds(&bridge, &mut daemon, "foxtrot").await;
}

/// The daemon rejects the goal at admission: the call is refused with the
/// daemon's reason, and the lock is free when the call hears of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_the_daemon_rejects_is_refused_with_its_reason() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);

    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    daemon.rejects_join(STACK_BUSY_REASON).await;
    assert_eq!(ended(call).await, refused(STACK_BUSY_REASON));
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
async fn a_join_that_finds_the_lock_held_is_refused_with_the_busy_text() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let first = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;

    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "openarm_sim").await,
        refused(STACK_BUSY_REASON)
    );
    assert_eq!(
        daemon.reads(),
        1,
        "the refused join does not read the stack"
    );

    complete(&context, &LaunchResult::success(STACK_LOG)).await;
    assert!(ended(first).await.is_ok());
    assert!(idle_now(&bridge));
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A cancel after the acceptance ends the call at once as cancelled, and
/// sends the daemon nothing. The bridge follows the join to its end, and
/// holds the lock until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_join_goes_on_and_holds_the_lock_until_it_ends() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    publish(&context, &join_progress()[0]).await;
    surface.reported(2).await;

    surface.cancel.cancel();
    assert_eq!(ended(call).await, stopped_waiting("the addition of bravo"));

    // The join runs on: its lock refuses another join, also after more
    // progress that no call reads.
    publish(&context, &join_progress()[1]).await;
    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "so101_sim").await,
        refused(STACK_BUSY_REASON)
    );
    assert!(!idle_now(&bridge));
    assert!(
        !context.is_cancelled(),
        "the bridge sends the daemon no cancel"
    );
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
    idle(&bridge).await;
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A call whose handler the runtime drops (a closed call, a whole-goal
/// deadline) leaves the join to the bridge, which follows it and holds the
/// lock until it ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_dropped_call_leaves_the_join_running_under_the_lock() {
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
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
    idle(&bridge).await;
    assert!(!context.is_cancelled());
    join_succeeds(&bridge, &mut daemon, "charlie").await;
}

/// A cancel before the daemon answered the goal waits for that answer: a
/// goal the daemon rejects ends the call refused, since nothing goes on,
/// and an admitted one ends it cancelled at once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_before_the_daemon_answers_waits_for_the_answer() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    surface.cancel.cancel();

    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    daemon.rejects_join(STACK_BUSY_REASON).await;
    assert_eq!(ended(call).await, refused(STACK_BUSY_REASON));

    daemon.lists(&[]);
    let call = spawn_join(&bridge, &surface, "bravo", "so101_sim");
    let context = daemon.admits_join(&join_goal("bravo", "so101_sim")).await;
    assert_eq!(ended(call).await, stopped_waiting("the addition of bravo"));
    assert_eq!(
        surface.feedback(),
        ["launch: the daemon accepted the addition of bravo (so101_sim)"]
    );
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
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
        refused(&format!(
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
/// fails the call, and the bridge sends no cancel and goes on following the
/// join, under its lock, to its end.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_silent_daemon_fails_the_call_after_one_window_and_the_join_goes_on() {
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
        refused("the daemon sent nothing for 660 s; the addition of bravo can still be running")
    );
    assert!(
        !context.is_cancelled(),
        "the bridge sends the daemon no cancel"
    );
    let other = ScriptedSurface::new();
    assert_eq!(
        join(&bridge, &other, "charlie", "so101_sim").await,
        refused(STACK_BUSY_REASON),
        "the join goes on under the lock"
    );
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
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
            &LaunchFeedback::stdout(line, LaunchFeedbackStep::LauncherStep),
        )
        .await;
    }
    surface.reported(4).await;
    complete(&context, &LaunchResult::success(STACK_LOG)).await;

    assert_eq!(
        ended(call).await,
        Ok(json!({ "success": true, "message": "bravo is off the stack" }))
    );
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
            refused(&format!(
                "no copy `{refused_name}` of the options openarm_sim and so101_sim on the stack"
            ))
        );
    }
    let single = mesh.bridge(single_scope());
    daemon.lists(&[copy("alpha", "openarm_sim")]);
    assert_eq!(
        remove(&single, &surface, "alpha").await,
        refused("no copy `alpha` of the option so101_sim on the stack")
    );

    // The daemon's next removal is the first one the bridge sends.
    daemon.lists(&[copy("bravo", "so101_sim")]);
    let call = spawn_remove(&bridge, &surface, "bravo");
    let context = daemon.admits_removal("bravo").await;
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
    assert!(ended(call).await.is_ok());
}

/// A removal that the daemon refuses after acceptance, for example a copy
/// that the stack links to, completes with `success` false.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_the_daemon_refuses_after_acceptance_completes_with_success_false() {
    let (mesh, mut daemon) = mesh().await;
    let bridge = mesh.bridge(simulation_scope());
    let surface = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("bravo", "so101_sim")]);

    let call = spawn_remove(&bridge, &surface, "bravo");
    let context = daemon.admits_removal("bravo").await;
    let error = "removing `bravo` would leave `recorder_inst` without a member in `arms`";
    complete(&context, &LaunchResult::failure(STACK_LOG, error)).await;

    assert_eq!(
        ended(call).await,
        Ok(json!({
            "success": false,
            "message": format!("the removal of bravo failed: {error}. Log files: {STACK_LOG}"),
        }))
    );
}

/// A removal takes no lock of the bridge: while a join runs, the removal
/// reaches the daemon, which refuses it with its busy text. A cancelled
/// removal goes on as a cancelled join does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_takes_no_lock_and_a_cancel_stops_its_wait_only() {
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
        .reject(None, rejected(STACK_BUSY_REASON))
        .await
        .expect("the daemon rejects the removal");
    assert_eq!(ended(removing).await, refused(STACK_BUSY_REASON));
    complete(&join_context, &LaunchResult::success(STACK_LOG)).await;
    assert!(ended(joining).await.is_ok());

    let cancelling = Arc::new(ScriptedSurface::new());
    daemon.lists(&[copy("alpha", "so101_sim")]);
    let removing = spawn_remove(&bridge, &cancelling, "alpha");
    let context = daemon.admits_removal("alpha").await;
    cancelling.followed().await;
    cancelling.cancel.cancel();
    assert_eq!(
        ended(removing).await,
        stopped_waiting("the removal of alpha")
    );
    assert!(
        !context.is_cancelled(),
        "the bridge sends the daemon no cancel"
    );
    complete(&context, &LaunchResult::success(STACK_LOG)).await;
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
    /// extension and without a progress token, and returns the JSON-RPC
    /// answer.
    async fn call(&self, tool: &str, arguments: Value) -> Value {
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
                    "params": {
                        "name": tool,
                        "arguments": arguments,
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
                            "io.modelcontextprotocol/clientCapabilities": {},
                        }
                    }
                })
                .to_string(),
            );
        let response = tokio::time::timeout(READINESS_TIMEOUT, request.send())
            .await
            .expect("the response headers arrive")
            .expect("the call opens");
        let mut events = Events::of(response);
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

    for (tool, arguments) in [
        (
            "stack.join",
            json!({ "name": "bravo", "option": "web_commander" }),
        ),
        (
            "stack.join",
            json!({ "name": "self", "option": "so101_sim" }),
        ),
        (
            "stack.join",
            json!({ "name": "bad/name", "option": "so101_sim" }),
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

    daemon.lists(&[copy("bravo", "so101_sim")]);
    let answer = endpoint
        .call(
            "stack.join",
            json!({ "name": "bravo", "option": "openarm_sim" }),
        )
        .await;
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert_eq!(
        answer["result"]["content"][0]["text"],
        "the action failed: a copy `bravo` is already on the stack; choose another name"
    );

    daemon.lists(&[copy("bravo", "so101_sim")]);
    let listed = endpoint.call("stack.list", json!({})).await;
    assert_eq!(
        listed["result"]["structuredContent"]["copies"],
        json!([{ "name": "bravo", "option": "so101_sim" }]),
        "{listed}"
    );

    daemon.lists(&[]);
    let call = endpoint.call(
        "stack.join",
        json!({ "name": "charlie", "option": "so101_sim" }),
    );
    let daemon_side = async {
        let context = daemon.admits_join(&join_goal("charlie", "so101_sim")).await;
        complete(&context, &LaunchResult::success(STACK_LOG)).await;
        context
    };
    let (joined, _context) = tokio::join!(call, daemon_side);
    assert_eq!(
        joined["result"]["structuredContent"],
        json!({ "success": true, "message": "charlie (so101_sim) is on the stack" }),
        "{joined}"
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
            "failed",
            "completed"
        ],
        "{recorded:?}"
    );
    assert_eq!(
        recorded[4].1,
        "the action failed: a copy `bravo` is already on the stack; choose another name"
    );
    assert_eq!(recorded[5].1, "charlie (so101_sim) is on the stack");
    endpoint.stop().await;
}
