//! The fleet rules against a fake daemon of the fleet and a fake arm: an
//! ephemeral router, the daemon's `check_move` and `changes` on peppylib's
//! production engines, and an arm whose `move_arm` has the mark
//! `moves_robot`. Every step waits on a peer's action, never on the clock: a
//! deadline only bounds the failure path.

use super::{MarkedSurface, MoveRefused, MoveRules};
use crate::bridges::{Binding, GoalFollow, PreparedTask, drive_goal};
use crate::test_support::ScriptedSurface;
use config::failure_kind::{FailureKindName, MemberFailureKinds, PeppyFailureKind};
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::daemon_interface::fleet::{ChangeKind, FleetChange, FleetListing};
use peppy_codec::MessageCodec;
use peppy_codec::consumer::{ActionClient, ConsumerIdentity, MemberBinding};
use peppy_mcp_runtime::{ActionExit, CancelledGoal, ProviderFailure};
use peppylib::messaging::{
    DEFAULT_FEEDBACK_BOUND, Failure, GoalContext, MessengerHandle, NonEmptyPayload, ProducerRef,
    SenderTarget,
};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, MockServiceCore, READINESS_TIMEOUT,
    wait_action_reachable, wait_service_reachable,
};
use peppylib::types::Payload;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

const CORE_NODE: &str = "cn-desk";
/// The per-boot instance id of the daemon of the fleet.
const DAEMON: &str = "core-boot";
const ARM: &str = "alpha_arm_inst";
const SERVER: &str = "framework_controls_inst";
/// The whole-goal deadline of the arm's tool: a bound on the failure path
/// only.
const DEADLINE: Duration = Duration::from_secs(30);

/// The router, a session per peer, the fake daemon of the fleet and the
/// fake arm.
struct Mesh {
    server: MessengerHandle,
    _peers: [MessengerHandle; 2],
    _router: EphemeralRouter,
    check_move: MockServiceCore,
    changes: MockActionServerCore,
    arm: MockActionServerCore,
}

fn fleet_contract() -> SenderTarget {
    SenderTarget::contract("fleet", "v1").expect("a valid contract identity")
}

fn arm_contract() -> SenderTarget {
    SenderTarget::contract("limb_motion", "v1").expect("a valid contract identity")
}

async fn mesh() -> Mesh {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let daemon = router.connect().await.expect("the daemon connects");
    let arm_session = router.connect().await.expect("the arm connects");
    let server = router.connect().await.expect("the server connects");
    let check_move =
        MockServiceCore::listen(&daemon, CORE_NODE, DAEMON, fleet_contract(), "check_move")
            .await
            .expect("the daemon serves check_move");
    let changes = MockActionServerCore::expose(
        &daemon,
        CORE_NODE,
        DAEMON,
        fleet_contract(),
        "changes",
        true,
    )
    .await
    .expect("the daemon serves changes");
    let arm = MockActionServerCore::expose(
        &arm_session,
        CORE_NODE,
        ARM,
        arm_contract(),
        "move_arm",
        false,
    )
    .await
    .expect("the arm serves move_arm");
    let daemon_producer = ProducerRef::new(CORE_NODE, DAEMON);
    wait_service_reachable(
        &server,
        CORE_NODE,
        SERVER,
        fleet_contract(),
        "check_move",
        &daemon_producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("check_move becomes reachable");
    wait_action_reachable(
        &server,
        CORE_NODE,
        SERVER,
        fleet_contract(),
        "changes",
        &daemon_producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("changes becomes reachable");
    wait_action_reachable(
        &server,
        CORE_NODE,
        SERVER,
        arm_contract(),
        "move_arm",
        &arm_producer(),
        READINESS_TIMEOUT,
    )
    .await
    .expect("move_arm becomes reachable");
    Mesh {
        server,
        _peers: [daemon, arm_session],
        _router: router,
        check_move,
        changes,
        arm,
    }
}

fn identity() -> ConsumerIdentity {
    ConsumerIdentity {
        core_node: CORE_NODE.to_owned(),
        instance_id: SERVER.to_owned(),
    }
}

fn arm_producer() -> ProducerRef {
    ProducerRef::new(CORE_NODE, ARM)
}

fn rules(mesh: &Mesh) -> MoveRules {
    MoveRules::new(
        mesh.server.clone(),
        identity(),
        ProducerRef::new(CORE_NODE, DAEMON),
    )
    .expect("the codecs of fleet:v1 lay out")
}

/// The codec of one side of a member of `fleet:v1`.
fn fleet_codec(member: &str, side: &str) -> MessageCodec {
    let document = DaemonInterface::Fleet.document();
    let format = match side {
        "response" => document
            .service(member)
            .and_then(|service| service.response_message_format.as_ref()),
        "request" => document
            .service(member)
            .and_then(|service| service.request_message_format.as_ref()),
        _ => document
            .action(member)
            .and_then(|action| action.feedback_topic.as_ref())
            .map(|feedback| &feedback.message_format),
    };
    MessageCodec::of_payload(&format!("test_{member}_{side}"), format)
        .expect("the format lays out")
        .expect("the side has a format")
}

/// The answer of `check_move`.
fn check_move_answer(refusal: Option<(&str, &str)>) -> Payload {
    let value = match refusal {
        None => json!({}),
        Some((kind, message)) => {
            json!({ "refusal": { "failure_kind": kind, "message": message } })
        }
    };
    Payload::from(
        fleet_codec("check_move", "response")
            .encode(&value)
            .expect("the answer fits the format"),
    )
}

/// One feedback of `changes`: a listing that gives the halt of `halted_by`.
fn change(kind: ChangeKind, halted_by: Option<&str>) -> NonEmptyPayload {
    let value = serde_json::to_value(FleetChange {
        change: kind,
        robot: String::new(),
        listing: FleetListing {
            halted: halted_by.is_some(),
            halted_by: halted_by.unwrap_or_default().to_owned(),
            ..FleetListing::default()
        },
    })
    .expect("a change serializes");
    NonEmptyPayload::try_new(Payload::from(
        fleet_codec("changes", "feedback")
            .encode(&value)
            .expect("the change fits the format"),
    ))
    .expect("an encoded message is never empty")
}

/// The follow of the changes, as the daemon admits it.
async fn followed_changes(mesh: &mut Mesh) -> Arc<GoalContext> {
    mesh.changes
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the server follows the changes")
        .accept(Payload::new())
        .await
        .expect("the daemon admits the follow")
}

/// The goal of `move_arm`, as the arm accepts it.
async fn arm_goal(mesh: &mut Mesh) -> Arc<GoalContext> {
    mesh.arm
        .next_goal(READINESS_TIMEOUT)
        .await
        .expect("the goal reaches the arm")
        .accept(Payload::new())
        .await
        .expect("the arm accepts the goal")
}

async fn cancel_seen(context: &GoalContext) {
    tokio::time::timeout(READINESS_TIMEOUT, context.cancel_signal())
        .await
        .expect("a cancel reaches the provider");
}

/// The task of `move_arm`, a member with the mark `moves_robot`.
fn move_arm() -> PreparedTask {
    PreparedTask {
        name: "arm.move_arm".to_owned(),
        binding: Binding {
            target: "arm".to_owned(),
            contract: arm_contract(),
            member: "move_arm".to_owned(),
        },
        client: ActionClient::new(None, None, None, MemberFailureKinds::peppy_only()),
        follow: GoalFollow::WholeGoal {
            deadline: DEADLINE,
            reports_feedback: false,
        },
        feedback_bound: DEFAULT_FEEDBACK_BOUND,
        moves_robot: true,
    }
}

fn move_arm_binding() -> MemberBinding {
    MemberBinding {
        target: arm_contract(),
        member: "move_arm".to_owned(),
    }
}

/// The server asks the daemon about the instance it calls, and makes the
/// call only when the daemon allows it; a refusal gives the kind and the
/// message of the daemon, and a check that the daemon does not answer
/// refuses the call too.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_marked_call_goes_on_only_when_the_daemon_allows_it() {
    let mesh = mesh().await;
    let rules = rules(&mesh);

    mesh.check_move.enqueue_response(check_move_answer(None));
    assert_eq!(rules.check(&arm_producer()).await, Ok(()));
    let asked = &mesh.check_move.captured()[0];
    assert_eq!(
        fleet_codec("check_move", "request")
            .decode(asked.message.payload_bytes().as_ref())
            .expect("the request decodes"),
        json!({ "instance": ARM }),
        "the server names the instance that it calls"
    );

    mesh.check_move.enqueue_response(check_move_answer(Some((
        "leased",
        "`director_inst` leases `alpha` for the run `tidy`",
    ))));
    assert_eq!(
        rules.check(&arm_producer()).await,
        Err(MoveRefused::Refused(ProviderFailure::new(
            FailureKindName::parse("leased").unwrap(),
            "`director_inst` leases `alpha` for the run `tidy`",
        )))
    );

    let arm = arm_producer();
    let (refused, ()) = tokio::join!(rules.check(&arm), async {
        let (_request, responder) = mesh
            .check_move
            .next_request(READINESS_TIMEOUT)
            .await
            .expect("the check reaches the daemon");
        responder
            .respond_failure(Failure::new(
                PeppyFailureKind::PreconditionFailed,
                "the daemon is busy",
            ))
            .await
            .expect("the daemon answers with an error");
    });
    assert!(
        matches!(&refused, Err(MoveRefused::Unanswered(message)) if message.contains("the daemon did not answer the move check of `alpha_arm_inst`")),
        "{refused:?}"
    );
}

/// The server follows the changes while a goal on a marked member runs, sees
/// the halt that a listing gives, and stops the follow once the last such
/// goal ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_changes_are_followed_while_a_marked_goal_runs() {
    let mut mesh = mesh().await;
    let rules = rules(&mesh);

    let first = rules.follow_marked_goal();
    let second = first.clone();
    let follow = followed_changes(&mut mesh).await;
    follow
        .publish_feedback(change(ChangeKind::Listing, None))
        .await
        .expect("the daemon gives the listing");
    follow
        .publish_feedback(change(
            ChangeKind::HaltStart,
            Some("`peppy-cli` on `cn-desk`"),
        ))
        .await
        .expect("the daemon gives the halt");
    tokio::time::timeout(READINESS_TIMEOUT, first.halted())
        .await
        .expect("the server sees the halt");

    drop(first);
    assert!(
        !follow.is_cancelled(),
        "a goal still runs, so the follow goes on"
    );
    drop(second);
    cancel_seen(&follow).await;
}

/// A halt cancels a goal on a marked member as its client does, and the call
/// ends with the end that the provider gives.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_halt_cancels_a_running_marked_goal() {
    let mut mesh = mesh().await;
    let rules = rules(&mesh);
    let task = move_arm();
    let client = ScriptedSurface::new();
    let marked = rules.follow_marked_goal();
    let surface = MarkedSurface {
        client: &client,
        marked: &marked,
    };
    let (server, identity, binding, arm) = (
        mesh.server.clone(),
        identity(),
        move_arm_binding(),
        arm_producer(),
    );
    let drive = drive_goal(
        &task,
        &server,
        &identity,
        &binding,
        &arm,
        json!({}),
        &surface,
        Some(marked.clone()),
    );
    let provider = async {
        let follow = followed_changes(&mut mesh).await;
        let goal = arm_goal(&mut mesh).await;
        client.followed().await;
        follow
            .publish_feedback(change(
                ChangeKind::HaltStart,
                Some("`peppy-cli` on `cn-desk`"),
            ))
            .await
            .expect("the daemon gives the halt");
        cancel_seen(&goal).await;
        goal.complete_cancelled("the arm stopped on the cancel", Payload::new())
            .await
            .expect("the arm ends the goal cancelled");
        follow
    };
    let (ended, follow) = tokio::join!(drive, provider);
    assert_eq!(
        ended,
        Err(ActionExit::Cancelled(CancelledGoal {
            result: json!({}),
            reason: "the arm stopped on the cancel".to_owned(),
        }))
    );
    drop(marked);
    cancel_seen(&follow).await;
}

/// A goal on a marked member that its client stops waiting for gets no
/// cancel: the server follows it on until it ends, and the follow of the
/// changes runs until then.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_marked_goal_that_its_client_stops_waiting_for_is_followed_until_it_ends() {
    let mut mesh = mesh().await;
    let rules = rules(&mesh);
    let marked = rules.follow_marked_goal();
    let server = mesh.server.clone();
    let client = Arc::new(ScriptedSurface::new());
    let drive = {
        let marked = marked.clone();
        let client = Arc::clone(&client);
        tokio::spawn(async move {
            let surface = MarkedSurface {
                client: client.as_ref(),
                marked: &marked,
            };
            drive_goal(
                &move_arm(),
                &server,
                &identity(),
                &move_arm_binding(),
                &arm_producer(),
                json!({}),
                &surface,
                Some(marked.clone()),
            )
            .await
        })
    };
    let follow = followed_changes(&mut mesh).await;
    let goal = arm_goal(&mut mesh).await;
    client.followed().await;
    drop(marked);

    drive.abort();
    assert!(
        drive.await.expect_err("the call stops").is_cancelled(),
        "the client of the call stopped waiting"
    );
    goal.complete(Payload::new())
        .await
        .expect("the arm reaches its target");
    cancel_seen(&follow).await;
    assert!(
        !goal.is_cancelled(),
        "the goal ran on to its end with no cancel"
    );
}
