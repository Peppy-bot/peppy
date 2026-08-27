//! The goal loop driven against a real provider: an ephemeral router, a
//! mock action server running peppylib's production goal engine, and the
//! bridge settling on each outcome a provider can reach. Every step waits
//! on the peer's action, never on the clock: the deadline only bounds the
//! failure path.

use super::{Binding, PreparedTask, TaskSurface, drive_goal};
use config::node::{MessageFormat, QoSProfile};
use message_codec::MessageCodec;
use message_codec::consumer::{ActionClient, ConsumerIdentity, MemberBinding};
use peppy_mcp_runtime::ActionExit;
use peppylib::messaging::{MessengerHandle, NonEmptyPayload, ProducerRef, SenderTarget};
use peppylib::testing::{
    EphemeralRouter, MockActionServerCore, READINESS_TIMEOUT, wait_action_reachable,
};
use peppylib::types::Payload;
use serde_json::{Value, json};
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

const CORE_NODE: &str = "test_core";
const PROVIDER_INSTANCE: &str = "backbone_inst";
const BRIDGE_INSTANCE: &str = "commander_inst";
const LINK_ID: &str = "limb_motion";
const MEMBER: &str = "move_gripper";
/// The whole-goal deadline: a bound on the failure path only, every step
/// below settles the moment the provider acts.
const DEADLINE: Duration = Duration::from_secs(10);

/// The runtime surface, scripted: cancellation fires when the test says so
/// and every feedback message is kept for the assertions.
struct ScriptedSurface {
    cancel: CancellationToken,
    feedback: Mutex<Vec<String>>,
}

impl ScriptedSurface {
    fn new() -> Self {
        Self {
            cancel: CancellationToken::new(),
            feedback: Mutex::new(Vec::new()),
        }
    }

    fn feedback(&self) -> Vec<String> {
        self.feedback
            .lock()
            .expect("no test panicked with the lock")
            .clone()
    }
}

impl TaskSurface for ScriptedSurface {
    fn report_feedback(&self, message: String) {
        self.feedback
            .lock()
            .expect("no test panicked with the lock")
            .push(message);
    }

    fn cancel_requested(&self) -> impl Future<Output = ()> + Send {
        self.cancel.cancelled()
    }
}

/// One mesh per test: the router, a session per side, and the provider's
/// identity as the launcher would have bound it to the task's target.
struct Mesh {
    bridge_messenger: MessengerHandle,
    _provider_messenger: MessengerHandle,
    _router: EphemeralRouter,
    contract: SenderTarget,
    producer: ProducerRef,
}

/// Starts the mesh and exposes `MEMBER` on it, returning once the bridge's
/// session can reach the provider.
async fn mesh(has_feedback: bool) -> (Mesh, MockActionServerCore) {
    let router = EphemeralRouter::start()
        .await
        .expect("the ephemeral router starts");
    let provider_messenger = router.connect().await.expect("the provider connects");
    let bridge_messenger = router.connect().await.expect("the bridge connects");
    let contract = SenderTarget::contract("limb_motion", "v1").expect("a valid contract identity");
    let provider = MockActionServerCore::expose(
        &provider_messenger,
        CORE_NODE,
        PROVIDER_INSTANCE,
        contract.clone(),
        MEMBER,
        has_feedback,
    )
    .await
    .expect("the provider exposes the action");
    let producer = ProducerRef::new(CORE_NODE, PROVIDER_INSTANCE);
    wait_action_reachable(
        &bridge_messenger,
        CORE_NODE,
        BRIDGE_INSTANCE,
        contract.clone(),
        MEMBER,
        &producer,
        READINESS_TIMEOUT,
    )
    .await
    .expect("the action becomes reachable");
    (
        Mesh {
            bridge_messenger,
            _provider_messenger: provider_messenger,
            _router: router,
            contract,
            producer,
        },
        provider,
    )
}

fn codec(label: &str, format: Value) -> MessageCodec {
    let format: MessageFormat = serde_json::from_value(format).expect("the format parses");
    MessageCodec::new(label, format).expect("the format lays out")
}

fn result_codec() -> MessageCodec {
    codec("move_gripper_result", json!({ "success": "bool" }))
}

fn encoded(codec: &MessageCodec, value: Value) -> Payload {
    Payload::from(codec.encode(&value).expect("the value fits the format"))
}

/// The task as `prepare` lays it out for an action with a result and,
/// optionally, a feedback message.
fn prepared_task(mesh: &Mesh, feedback: Option<MessageCodec>) -> PreparedTask {
    PreparedTask {
        name: "openarm.move_gripper".to_owned(),
        binding: Binding {
            target: LINK_ID.to_owned(),
            contract: mesh.contract.clone(),
            member: MEMBER.to_owned(),
        },
        reports_feedback: feedback.is_some(),
        client: ActionClient::new(None, feedback, Some(result_codec())),
        feedback_qos: QoSProfile::Reliable,
        deadline: DEADLINE,
    }
}

/// Drives one goal through the bridge exactly as `run_task` does once the
/// node runner has resolved the binding.
async fn drive(
    mesh: &Mesh,
    task: &PreparedTask,
    surface: &ScriptedSurface,
) -> Result<Value, ActionExit> {
    let identity = ConsumerIdentity {
        core_node: CORE_NODE.to_owned(),
        instance_id: BRIDGE_INSTANCE.to_owned(),
    };
    let binding = MemberBinding {
        target: mesh.contract.clone(),
        member: MEMBER.to_owned(),
        producers: vec![mesh.producer.clone()],
    };
    drive_goal(
        task,
        &mesh.bridge_messenger,
        &identity,
        &binding,
        &mesh.producer,
        json!({}),
        surface,
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_on_its_completed_result() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        context
            .complete(encoded(&result_codec(), json!({ "success": true })))
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(json!({ "success": true })));
    assert_eq!(surface.feedback(), Vec::<String>::new());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_feedback_less_goal_settles_cancelled_once_the_provider_honors_the_cancel() {
    let (mesh, mut provider) = mesh(false).await;
    let task = prepared_task(&mesh, None);
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        // The client cancels while the goal runs: the bridge forwards it,
        // and the provider settles the goal cancelled once it observes it.
        surface.cancel.cancel();
        context.cancel_signal().await;
        context
            .complete_cancelled(Payload::new())
            .await
            .expect("the provider settles the goal cancelled");
        context
    });

    assert_eq!(outcome, Err(ActionExit::Cancelled));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_with_feedback_reports_it_and_settles_on_its_result() {
    let (mesh, mut provider) = mesh(true).await;
    let feedback = codec("move_gripper_feedback", json!({ "percent": "u8" }));
    let task = prepared_task(&mesh, Some(feedback.clone()));
    let surface = ScriptedSurface::new();

    let (outcome, _context) = tokio::join!(drive(&mesh, &task, &surface), async {
        let goal = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        let context = goal
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        let progress = NonEmptyPayload::try_new(encoded(&feedback, json!({ "percent": 50 })))
            .expect("an encoded message is never empty");
        context
            .publish_feedback(progress)
            .await
            .expect("the provider publishes feedback");
        context
            .complete(encoded(&result_codec(), json!({ "success": true })))
            .await
            .expect("the provider completes the goal");
        context
    });

    assert_eq!(outcome, Ok(json!({ "success": true })));
    assert_eq!(surface.feedback(), vec![r#"{"percent":50}"#.to_owned()]);
}
