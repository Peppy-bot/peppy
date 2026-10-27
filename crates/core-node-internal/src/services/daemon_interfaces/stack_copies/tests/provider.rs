//! The provider of `stack_copies:v1` against a fake daemon: an ephemeral
//! router, the provider served as the daemon serves it, a fake core node
//! that runs `stack_join` and `stack_remove` on peppylib's production goal
//! engine, under the producer address of the daemon, and a client that
//! calls the provider as an instance of the launch. The fake daemon admits,
//! refuses, feeds and ends each stack goal when the test tells it to. Every
//! step waits on a peer's action, never on the clock: a deadline only bounds
//! the failure path.

use super::super::{JOIN_BUSY_REASON, undo_unaccepted};
use crate::services::credentials::testing::{KnownKeys, client_key};
use crate::services::daemon_interfaces::{InterfaceContext, serve};
use crate::services::federation::SliceOwnership;
use crate::services::stack::ActiveLaunch;
use crate::services::stack::fixtures::{active_launch_with_daemon_targets, with_launched_copies};
use config::failure_kind::PeppyFailureKind;
use config::runtime::{CopyName, Name};
use core_node_api::encoding::{
    GoalClient, LaunchFeedback, LaunchFeedbackStep, LaunchGoalResponse, LaunchResult,
    StackJoinGoal, StackRemoveGoal,
};
use core_node_api::names::CORE_NODE_TAG;
use core_node_api::{ActionGoal, DaemonFailureKind};
use daemon_config::daemon_interface::DaemonInterface;
use node_stack::NodeStack;
use peppy_codec::consumer::{
    ActionClient, ConsumerIdentity, GoalHandle, GoalOutcome, MemberBinding, ServiceClient,
};
use peppylib::messaging::{
    CallerKind, DEFAULT_FEEDBACK_BOUND, Failure, GoalContext, MessengerHandle, ProducerRef,
    SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, READINESS_TIMEOUT, wait_action_reachable,
    wait_service_reachable,
};
use peppylib::types::Payload;
use serde_json::{Value, json};
use stack_goal::DaemonRoute;
use std::sync::Arc;

const CORE_NODE: &str = "cn-desk";
/// The per-boot instance id of the daemon: the producer address of the
/// provider and of the fake stack goals.
const DAEMON: &str = "core-boot";
/// The client: an instance of the launch whose `stack` target holds the
/// scope of [`scope`].
const MANAGER: &str = "manager_inst";
/// The log file of every stack goal that the fake daemon admits.
const STACK_LOG: &str = "/home/peppy/.peppy/logs/stack/goal.log";

const ROOT_CONFIG: &str = r#"{
    peppy_schema: "node/v1",
    manifest: { name: "cn_desk", tag: "v1" },
    interfaces: {},
    execution: {
        language: "rust",
        parameters: {},
        build_cmd: ["true"],
        run_cmd: ["true"],
    },
}"#;

/// The scope of the manager: one option, `so101_sim`, two copies at most.
fn scope() -> Value {
    json!({
        "max_copies": 2,
        "options": [
            { "option": "so101_sim", "model": "so101", "description": "An SO-101." }
        ]
    })
}

/// The router, the provider on the daemon's session, the fake stack goals
/// of the daemon, and the client's session.
struct Mesh {
    client: MessengerHandle,
    daemon: MessengerHandle,
    join: MockActionServerCore,
    _remove: MockActionServerCore,
    _served: tokio::task::JoinHandle<crate::Result<()>>,
    _ownership: Arc<SliceOwnership>,
    _router: EphemeralRouter,
    _directory: tempfile::TempDir,
}

fn provider() -> ProducerRef {
    ProducerRef::new(CORE_NODE, DAEMON)
}

fn stack_copies() -> SenderTarget {
    SenderTarget::contract("stack_copies", "v1").expect("a contract identity")
}

async fn mesh() -> Mesh {
    mesh_of(active_launch_with_daemon_targets(
        MANAGER,
        &[("stack", DaemonInterface::StackCopies, scope())],
    ))
    .await
}

/// The mesh of [`mesh`] with `active` as the launch that the daemon
/// coordinates.
async fn mesh_of(active: ActiveLaunch) -> Mesh {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    // The client signs its calls with one key, which the check of the
    // daemon knows as the key of the manager and of the instance with no
    // target, so the scopes decide what each of them may call.
    let (client_key, client_signer) = client_key();
    let check = [MANAGER, "other_inst"]
        .into_iter()
        .fold(KnownKeys::default(), |known, instance| {
            known.knowing(CORE_NODE, instance, client_key, CallerKind::Instance)
        });
    let daemon = router
        .connect()
        .await
        .expect("the daemon connects")
        .checking_calls_with(Arc::new(check));
    let stack_goals = router.connect().await.expect("the stack goals connect");
    let client = router
        .connect()
        .await
        .expect("the client connects")
        .signing_calls_with(client_signer);
    let directory = tempfile::tempdir().unwrap();
    let root_config =
        config::node::NodeConfigParser::from_content(ROOT_CONFIG).expect("the root config parses");
    let node_stack = Arc::new(NodeStack::new(
        root_config,
        Some(Name::new(DAEMON).unwrap()),
        directory.path(),
    ));
    let ownership = SliceOwnership::new(CORE_NODE);
    *ownership.active.lock() = Some(active);

    let core_node = SenderTarget::node(CORE_NODE, CORE_NODE_TAG).expect("a core node identity");
    let mut changes = Vec::new();
    for action in [StackJoinGoal::ID.name(), StackRemoveGoal::ID.name()] {
        changes.push(
            MockActionServerCore::expose(
                &stack_goals,
                CORE_NODE,
                DAEMON,
                core_node.clone(),
                action,
                true,
            )
            .await
            .expect("the daemon exposes its stack change"),
        );
        wait_action_reachable(
            &daemon,
            CORE_NODE,
            DAEMON,
            core_node.clone(),
            action,
            &provider(),
            READINESS_TIMEOUT,
        )
        .await
        .expect("the stack change becomes reachable");
    }
    let remove = changes.pop().expect("stack_remove");
    let join = changes.pop().expect("stack_join");

    let served = serve(InterfaceContext::new(
        daemon.clone(),
        CORE_NODE,
        DAEMON,
        node_stack,
        Arc::clone(&ownership),
    ))
    .await
    .expect("the daemon serves its interfaces");
    for action in ["join", "remove"] {
        wait_action_reachable(
            &client,
            CORE_NODE,
            MANAGER,
            stack_copies(),
            action,
            &provider(),
            READINESS_TIMEOUT,
        )
        .await
        .expect("the provider becomes reachable");
    }
    wait_service_reachable(
        &client,
        CORE_NODE,
        MANAGER,
        stack_copies(),
        "list",
        &provider(),
        READINESS_TIMEOUT,
    )
    .await
    .expect("the provider's list becomes reachable");
    Mesh {
        client,
        daemon,
        join,
        _remove: remove,
        _served: served,
        _ownership: ownership,
        _router: router,
        _directory: directory,
    }
}

/// Sends the goal `goal` of the action `member` of `stack_copies:v1` to the
/// provider on `client`, as `instance_id`.
async fn send_as(
    client: &MessengerHandle,
    instance_id: &str,
    member: &str,
    goal: Value,
) -> GoalHandle {
    let document = DaemonInterface::StackCopies.document();
    ActionClient::of_contract(document, member)
        .expect("the interface declares the action")
        .fire_goal(
            client,
            &ConsumerIdentity {
                core_node: CORE_NODE.to_owned(),
                instance_id: instance_id.to_owned(),
            },
            &MemberBinding::of_contract(document, member).expect("a binding"),
            &provider(),
            &goal,
            DEFAULT_FEEDBACK_BOUND,
            READINESS_TIMEOUT,
        )
        .await
        .expect("the goal reaches the provider")
}

/// The answer of `list` to the manager.
async fn list(mesh: &Mesh) -> Value {
    let document = DaemonInterface::StackCopies.document();
    ServiceClient::of_contract(document, "list")
        .expect("the interface declares the service")
        .call(
            &mesh.client,
            &ConsumerIdentity {
                core_node: CORE_NODE.to_owned(),
                instance_id: MANAGER.to_owned(),
            },
            &MemberBinding::of_contract(document, "list").expect("a binding"),
            &provider(),
            &json!({}),
            READINESS_TIMEOUT,
        )
        .await
        .expect("the daemon lists the copies")
}

/// The stack join that the provider sends next, as `join`, the fake stack
/// join of the daemon, receives it.
async fn next_join_on(join: &mut MockActionServerCore) -> peppylib::testing::MockPendingGoal {
    join.next_goal(READINESS_TIMEOUT)
        .await
        .expect("the join reaches the daemon")
}

impl Mesh {
    /// Sends the goal `goal` of the action `member` of `stack_copies:v1` to
    /// the provider, as `instance_id`.
    async fn send_as(&self, instance_id: &str, member: &str, goal: Value) -> GoalHandle {
        send_as(&self.client, instance_id, member, goal).await
    }

    /// Sends the join of `name` of `so101_sim` on this machine, as the
    /// manager, for a join that the provider refuses before it reaches the
    /// daemon.
    async fn join(&self, name: &str) -> GoalHandle {
        self.send_as(MANAGER, "join", join_input(name)).await
    }

    /// Sends the join of `name` as the manager while the fake daemon admits
    /// the stack join that the provider sends for it: the provider answers
    /// the goal once the daemon admitted its change.
    async fn admitted_join(&mut self, name: &str) -> (GoalHandle, Arc<GoalContext>) {
        let (goal, context) = tokio::join!(
            send_as(&self.client, MANAGER, "join", join_input(name)),
            admits_join_on(&mut self.join, name)
        );
        assert!(goal.refusal().is_none(), "{:?}", goal.refusal());
        (goal, context)
    }
}

/// Admits the next stack join that `join`, the fake stack join of the
/// daemon, receives, which must add `name` for the manager.
async fn admits_join_on(join: &mut MockActionServerCore, name: &str) -> Arc<GoalContext> {
    let pending = next_join_on(join).await;
    let sent = StackJoinGoal::decode(pending.request_bytes()).expect("the join decodes");
    assert_eq!(sent.copy.name.as_str(), name);
    assert_eq!(sent.copy.option.as_str(), "so101_sim");
    assert_eq!(
        sent.client,
        Some(GoalClient {
            core_node: CORE_NODE.to_owned(),
            instance_id: MANAGER.to_owned(),
        }),
        "the join acts for the sender of the goal"
    );
    pending
        .accept(admitted())
        .await
        .expect("the daemon admits the join")
}

fn join_input(name: &str) -> Value {
    json!({ "name": name, "option": "so101_sim", "core_node": CORE_NODE, "features": [] })
}

fn admitted() -> Payload {
    LaunchGoalResponse::new(STACK_LOG)
        .encode()
        .expect("the admission encodes")
}

/// The daemon publishes one line of output of its change.
async fn publish_line(context: &GoalContext, line: &str, step: LaunchFeedbackStep) {
    context
        .publish_feedback(
            LaunchFeedback::stdout(line, step)
                .encode()
                .expect("the feedback encodes"),
        )
        .await
        .expect("the daemon publishes its feedback");
}

/// The daemon completes its change.
async fn complete(context: &GoalContext) {
    context
        .complete(
            LaunchResult::default()
                .encode()
                .expect("the result encodes"),
        )
        .await
        .expect("the daemon ends the change");
}

/// The daemon waits for the cancel of the provider, then ends its change
/// cancelled with the reason of the cancel.
async fn end_cancelled(context: &GoalContext) {
    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the cancel of the provider reaches the daemon");
    let cause = context
        .cancel_cause()
        .expect("a cancelled goal has a cause");
    context
        .complete_cancelled(cause.reason(), Payload::new())
        .await
        .expect("the daemon ends the change cancelled");
}

/// The next feedback of `goal`, as `{ step, line }`.
async fn next_line(goal: &mut GoalHandle) -> (String, String) {
    let feedback = tokio::time::timeout(READINESS_TIMEOUT, goal.next_feedback())
        .await
        .expect("a line comes")
        .expect("the feedback reads")
        .expect("the stream is open");
    (
        feedback["step"].as_str().expect("a step").to_owned(),
        feedback["line"].as_str().expect("a line").to_owned(),
    )
}

/// The end of `goal`.
async fn end_of(mesh: &Mesh, goal: &GoalHandle) -> GoalOutcome {
    goal.result(&mesh.client, READINESS_TIMEOUT)
        .await
        .expect("the goal ends")
}

/// Joins `name` from start to end while the fake daemon admits and
/// completes it: the proof that the join lock is free.
async fn join_succeeds(mesh: &mut Mesh, name: &str) {
    let (goal, context) = mesh.admitted_join(name).await;
    complete(&context).await;
    assert_eq!(end_of(mesh, &goal).await, GoalOutcome::Completed(json!({})));
}

/// A join runs on the daemon for the sender of its goal: the client reads
/// the acceptance and each line of the change as `{ step, line }`, and the
/// goal completes as the change does.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_runs_for_its_sender_and_ends_as_its_change_ends() {
    let mut mesh = mesh().await;
    let (mut goal, context) = mesh.admitted_join("bravo").await;
    assert_eq!(
        next_line(&mut goal).await,
        (
            "launch".to_owned(),
            "the daemon accepted the addition of bravo (so101_sim)".to_owned()
        )
    );
    publish_line(
        &context,
        "fetching robot_initializer:v1",
        LaunchFeedbackStep::AddingNode,
    )
    .await;
    assert_eq!(
        next_line(&mut goal).await,
        ("add".to_owned(), "fetching robot_initializer:v1".to_owned())
    );
    complete(&context).await;
    assert_eq!(
        end_of(&mesh, &goal).await,
        GoalOutcome::Completed(json!({}))
    );
}

/// The provider runs one join at a time: a join while the daemon runs the
/// first one is refused with the kind `busy` before it reaches the daemon,
/// and a join after the first one ended runs.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_while_another_runs_is_refused_with_the_kind_busy() {
    let mut mesh = mesh().await;
    let (first, context) = mesh.admitted_join("bravo").await;

    let second = mesh.join("charlie").await;
    let refusal = second.refusal().expect("the join lock is held");
    assert_eq!(refusal.kind(), &DaemonFailureKind::Busy.into());
    assert_eq!(refusal.message(), JOIN_BUSY_REASON);

    complete(&context).await;
    assert_eq!(
        end_of(&mesh, &first).await,
        GoalOutcome::Completed(json!({}))
    );
    join_succeeds(&mut mesh, "charlie").await;
}

/// A join that the daemon refuses at its admission is refused with the
/// kind and the reason of the daemon, and frees the join lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_that_the_daemon_refuses_is_refused_with_its_kind_and_frees_the_lock() {
    let mut mesh = mesh().await;
    let (goal, ()) = tokio::join!(
        send_as(&mesh.client, MANAGER, "join", join_input("bravo")),
        async {
            next_join_on(&mut mesh.join)
                .await
                .refuse(Failure::new(
                    DaemonFailureKind::Leased,
                    "robot `bravo` is leased by `director_inst` on `cn-desk` under the name `run`",
                ))
                .await
                .expect("the daemon refuses the join");
        }
    );
    let refusal = goal.refusal().expect("the provider refuses the goal");
    assert_eq!(refusal.kind(), &DaemonFailureKind::Leased.into());
    assert_eq!(
        refusal.message(),
        "the daemon refused the addition of bravo: robot `bravo` is leased by `director_inst` \
         on `cn-desk` under the name `run`"
    );
    join_succeeds(&mut mesh, "bravo").await;
}

/// A change that the daemon fails ends the goal failed, with the failure
/// kind of the daemon, its message and the log file of the change.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_that_fails_ends_failed_with_its_kind_and_its_log_file() {
    let mut mesh = mesh().await;
    let (goal, context) = mesh.admitted_join("bravo").await;
    context
        .fail(Failure::new(
            PeppyFailureKind::AttemptFailed,
            "robot_initializer did not start.",
        ))
        .await
        .expect("the daemon fails the change");
    let GoalOutcome::Failed(failure) = end_of(&mesh, &goal).await else {
        panic!("the goal fails");
    };
    assert_eq!(failure.kind(), &PeppyFailureKind::AttemptFailed.into());
    assert_eq!(
        failure.message(),
        format!(
            "the addition of bravo failed: robot_initializer did not start. Log files: {STACK_LOG}"
        )
    );
    join_succeeds(&mut mesh, "charlie").await;
}

/// A cancel of the client reaches the change: the daemon stops it and ends
/// it cancelled, and the goal ends cancelled once it did. The join lock
/// holds until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancel_of_the_client_reaches_the_change_and_the_lock_holds_until_its_end() {
    let mut mesh = mesh().await;
    let (mut goal, context) = mesh.admitted_join("bravo").await;
    let _accepted = next_line(&mut goal).await;
    goal.cancel(&mesh.client, READINESS_TIMEOUT)
        .await
        .expect("the provider takes the cancel");
    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the cancel reaches the daemon");

    let during_the_undo = mesh.join("charlie").await;
    assert_eq!(
        during_the_undo.refusal().map(|refusal| refusal.message()),
        Some(JOIN_BUSY_REASON),
        "the join lock holds while the daemon undoes the change"
    );

    end_cancelled(&context).await;
    let GoalOutcome::Cancelled { reason, .. } = end_of(&mesh, &goal).await else {
        panic!("the goal ends cancelled");
    };
    assert!(reason.starts_with("the addition of bravo was "), "{reason}");
    join_succeeds(&mut mesh, "charlie").await;
}

/// A daemon that ends its change with no end, as a daemon that goes away
/// does, fails the goal, and the join lock is free once it did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_with_no_end_fails_the_goal_and_frees_the_lock() {
    let mut mesh = mesh().await;
    let (goal, context) = mesh.admitted_join("bravo").await;
    drop(context);
    let outcome = end_of(&mesh, &goal).await;
    assert!(
        matches!(&outcome, GoalOutcome::Failed(failure) if failure.message().starts_with("the addition of bravo failed")),
        "{outcome:?}"
    );
    join_succeeds(&mut mesh, "charlie").await;
}

/// The provider refuses a sender with no `stack_copies` target before
/// anything reaches the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sender_with_no_scope_is_refused() {
    let mut mesh = mesh().await;
    let goal = mesh
        .send_as("other_inst", "join", join_input("bravo"))
        .await;
    let refusal = goal.refusal().expect("the provider refuses the goal");
    assert_eq!(refusal.kind(), &PeppyFailureKind::Rejected.into());
    assert!(
        refusal
            .message()
            .contains("has no `stack_copies` target with a scope"),
        "{}",
        refusal.message()
    );
    join_succeeds(&mut mesh, "bravo").await;
}

/// A removal of a name that is no copy of a scoped option is refused, also
/// while a join holds the join lock.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_removal_of_a_name_that_is_no_scoped_copy_is_refused() {
    let mut mesh = mesh().await;
    let (first, context) = mesh.admitted_join("bravo").await;
    let removal = mesh
        .send_as(MANAGER, "remove", json!({ "name": "alpha" }))
        .await;
    let refusal = removal
        .refusal()
        .expect("no copy alpha of the scope is on the stack");
    assert_eq!(refusal.kind(), &PeppyFailureKind::Rejected.into());
    assert_eq!(
        refusal.message(),
        "no copy `alpha` of the option so101_sim on the stack"
    );
    complete(&context).await;
    assert_eq!(
        end_of(&mesh, &first).await,
        GoalOutcome::Completed(json!({}))
    );
}

/// The change of a goal that its client did not take is cancelled and
/// followed to the end that the daemon gives it: the follow ends only once
/// the daemon ended the change, whatever lines come before.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_change_whose_goal_was_not_taken_is_cancelled_and_followed_to_its_end() {
    let mut mesh = mesh().await;
    let route = DaemonRoute {
        messenger: &mesh.daemon,
        caller_core_node: CORE_NODE,
        caller_instance_id: DAEMON,
        daemon_core_node: CORE_NODE,
    };
    let goal = StackJoinGoal::new(
        "so101_sim:bravo".parse().expect("a copy reference"),
        stack_goal::DEFAULT_BUDGETS,
    );
    let (sent, context) = tokio::join!(stack_goal::send(route, &goal), async {
        next_join_on(&mut mesh.join)
            .await
            .accept(admitted())
            .await
            .expect("the daemon admits the join")
    });
    let mut running = sent.expect("the daemon runs the join");
    let change = super::super::Change::Addition {
        name: CopyName::new("bravo").unwrap(),
        option: Name::new("so101_sim").unwrap(),
    };
    let (ended_tx, mut ended_rx) = tokio::sync::oneshot::channel();
    let follow = tokio::spawn(async move {
        undo_unaccepted(&mut running, &change).await;
        let _ = ended_tx.send(());
    });

    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("the cancel reaches the daemon");
    publish_line(
        &context,
        "stopping bravo_arm_inst",
        LaunchFeedbackStep::RunningNode,
    )
    .await;
    assert!(
        ended_rx.try_recv().is_err(),
        "the follow goes on until the daemon ends the change"
    );
    context
        .complete_cancelled("the client cancelled the join", Payload::new())
        .await
        .expect("the daemon ends the change cancelled");
    follow
        .await
        .expect("the follow ends once the daemon ended the change");
}

/// `list` gives the launch copies of the scope's options in name order,
/// each in the shape of a `join` goal with each feature on the machine the
/// launch gave its part, one that a removal took off the stack included, and
/// leaves out a launch copy of another option.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn list_gives_the_launch_copies_of_the_scope_as_join_goals() {
    let scope = json!({
        "max_copies": 4,
        "options": [{
            "option": "arm", "model": "arm", "description": "An arm.",
            "features": [{ "feature": "ai_brain", "axis": "brain", "option": "ai_brain_vla",
                           "description": "A brain that drives the arm." }],
        }],
    });
    let launched = with_launched_copies(
        active_launch_with_daemon_targets(
            MANAGER,
            &[("stack", DaemonInterface::StackCopies, scope)],
        ),
        &["alpha.ai_brain_vla@cn-gpu-box", "charlie.ai_brain_vla"],
        &["arm:alpha@cn-jetson-1", "spare:zulu", "arm:charlie"],
        &["alpha"],
    );
    let mesh = mesh_of(launched).await;
    let listing = list(&mesh).await;
    let item = |name: &str, machine: &str, features: Value| {
        json!({
            "name": name,
            "option": "arm",
            "core_node": machine,
            "features": features,
        })
    };
    let brain_on = |machine: &str| json!([{ "feature": "ai_brain", "core_node": machine }]);
    let bravo = item("bravo", CORE_NODE, json!([]));
    let charlie = item("charlie", CORE_NODE, brain_on(CORE_NODE));
    let running = |mut copy: Value| {
        copy["running"] = json!(false);
        copy
    };
    assert_eq!(
        listing["copies"],
        json!([running(bravo.clone()), running(charlie.clone())]),
        "{listing}"
    );
    assert_eq!(
        listing["launch_copies"],
        json!([
            item("alpha", "cn-jetson-1", brain_on("cn-gpu-box")),
            bravo,
            charlie
        ]),
        "{listing}"
    );
}
