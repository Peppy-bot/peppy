//! The proof of the sender of the calls to a daemon, over the mock adapter:
//! a session with a signer signs each user request to a daemon, and a session
//! with a check verifies each call before its handler reads it.

use peppylib::PeppyError;
use peppylib::messaging::{
    ActionMessenger, CallCheck, CallerKind, CancelState, ConcurrentAction,
    DEFAULT_FEEDBACK_BOUND, MessengerHandle, ProducerRef, ReceivedCall, SenderTarget,
    ServiceMessenger, ServiceTarget, VerifiedCaller, decode_cancel_ack,
};
use peppylib::types::Payload;
use pmi::{
    ActionWireSender, ClientCredential, ClientKey, ClientPublicKey, Failure, Messenger,
    MessengerAdapter, MessengerBackend, MockAdapter,
};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;
use tokio::sync::Mutex;

const DAEMON_CORE_NODE: &str = "cn-desk";
const DAEMON_INSTANCE_ID: &str = "brave-otter-1234";
const CALL_TIMEOUT: Duration = Duration::from_secs(5);

/// A check that knows the key of each caller, and records whether each call
/// it saw carried a proof.
#[derive(Default)]
struct KeyCheck {
    keys: HashMap<(String, String), (ClientPublicKey, CallerKind)>,
    proofs_seen: StdMutex<Vec<bool>>,
}

impl KeyCheck {
    fn knowing(mut self, instance_id: &str, key: ClientPublicKey, kind: CallerKind) -> Self {
        self.keys.insert(
            (DAEMON_CORE_NODE.to_owned(), instance_id.to_owned()),
            (key, kind),
        );
        self
    }
}

impl CallCheck for KeyCheck {
    fn verify(&self, call: &ReceivedCall<'_>) -> Result<VerifiedCaller, Failure> {
        self.proofs_seen.lock().unwrap().push(call.proof.is_some());
        let refused = |why: &str| Failure::new(config::failure_kind::PeppyFailureKind::Rejected, why);
        let proof = call.proof.ok_or_else(|| refused("no signature"))?;
        let (key, kind) = self
            .keys
            .get(&(
                call.caller_core_node.to_owned(),
                call.caller_instance_id.to_owned(),
            ))
            .ok_or_else(|| refused("no key"))?;
        if !proof.verifies(key, &call.signed) {
            return Err(refused("the signature does not verify"));
        }
        Ok(VerifiedCaller::new(
            call.caller_core_node,
            call.caller_instance_id,
            *kind,
        ))
    }
}

/// One mock session, shared by every handle of a test.
async fn mock_session() -> Arc<Mutex<Messenger>> {
    let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
    messenger.start_session().await.unwrap();
    Arc::new(Mutex::new(messenger))
}

fn signing(handle: MessengerHandle, key: ClientKey) -> MessengerHandle {
    handle.signing_calls_with(ClientCredential::new(key, None).into_signer())
}

fn daemon_target() -> SenderTarget {
    SenderTarget::node(DAEMON_CORE_NODE, core_node_api::names::CORE_NODE_TAG).unwrap()
}

/// Serves the raw service `stack_reset` of the daemon on `daemon`, and counts
/// the requests that reach its handler.
async fn serve_reset(daemon: &MessengerHandle) -> Arc<AtomicUsize> {
    let mut endpoint = ServiceMessenger::listen(
        daemon,
        DAEMON_CORE_NODE,
        DAEMON_INSTANCE_ID,
        daemon_target(),
        "stack_reset",
    )
    .await
    .unwrap();
    let handled = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&handled);
    tokio::spawn(async move {
        endpoint
            .handle_requests(move |request| {
                counter.fetch_add(1, Ordering::SeqCst);
                let caller = request.verified_caller().cloned();
                async move {
                    let caller = caller.expect("a checked endpoint gives the verified caller");
                    Ok(Payload::from(
                        format!("{}:{:?}", caller.instance_id(), caller.kind()).into_bytes(),
                    ))
                }
            })
            .await
    });
    handled
}

async fn reset_as(client: &MessengerHandle, instance_id: &str) -> peppylib::PeppyResult<Payload> {
    ServiceMessenger::poll(
        client,
        DAEMON_CORE_NODE,
        instance_id,
        daemon_target(),
        "stack_reset",
        ServiceTarget::Any,
        Payload::from_static(b"reset"),
        CALL_TIMEOUT,
    )
    .await
    .map(|message| message.payload())
}

fn refusal_message(error: PeppyError) -> String {
    match error {
        PeppyError::ServiceFailed { failure, .. } => failure.message().to_owned(),
        other => panic!("expected the refusal of the call, got {other}"),
    }
}

#[tokio::test]
async fn a_signed_call_to_a_daemon_reaches_its_handler_with_the_verified_caller() {
    let session = mock_session().await;
    let command_line = ClientKey::generate();
    let check = KeyCheck::default().knowing("peppy-cli", command_line.public_key(), CallerKind::CommandLine);
    let daemon = MessengerHandle::from_shared(Arc::clone(&session)).checking_calls_with(Arc::new(check));
    let handled = serve_reset(&daemon).await;

    let client = signing(MessengerHandle::from_shared(session), command_line);
    let answer = reset_as(&client, "peppy-cli").await.unwrap();
    assert_eq!(answer.as_ref(), b"peppy-cli:CommandLine");
    assert_eq!(handled.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_unsigned_call_and_a_call_under_another_id_are_refused_before_their_handler() {
    let session = mock_session().await;
    let command_line = ClientKey::generate();
    let arm = ClientKey::generate();
    let check = KeyCheck::default()
        .knowing("peppy-cli", command_line.public_key(), CallerKind::CommandLine)
        .knowing("arm_inst", arm.public_key(), CallerKind::Instance);
    let daemon = MessengerHandle::from_shared(Arc::clone(&session)).checking_calls_with(Arc::new(check));
    let handled = serve_reset(&daemon).await;

    let unsigned = MessengerHandle::from_shared(Arc::clone(&session));
    let refused = reset_as(&unsigned, "peppy-cli").await.unwrap_err();
    assert_eq!(refusal_message(refused), "no signature");

    let node = signing(MessengerHandle::from_shared(session), arm);
    let refused = reset_as(&node, "peppy-cli").await.unwrap_err();
    assert_eq!(
        refusal_message(refused),
        "the signature does not verify",
        "the node signs with its own key, which is not the key of the command line"
    );
    assert_eq!(handled.load(Ordering::SeqCst), 0, "no handler ran");

    assert_eq!(
        reset_as(&node, "arm_inst").await.unwrap().as_ref(),
        b"arm_inst:Instance",
        "the node calls as itself"
    );
}

#[tokio::test]
async fn a_call_to_a_node_carries_no_signature() {
    let session = mock_session().await;
    let check = Arc::new(KeyCheck::default());
    let node_target = SenderTarget::node("arm", "v1").unwrap();
    let observer = MessengerHandle::from_shared(Arc::clone(&session))
        .checking_calls_with(Arc::clone(&check) as Arc<dyn CallCheck>);
    let mut endpoint =
        ServiceMessenger::listen(&observer, "cn-desk", "arm_inst", node_target.clone(), "home")
            .await
            .unwrap();
    tokio::spawn(async move {
        endpoint
            .handle_requests(|_| async { Ok(Payload::new()) })
            .await
    });

    let client = signing(MessengerHandle::from_shared(session), ClientKey::generate());
    let _refused = ServiceMessenger::poll(
        &client,
        "cn-desk",
        "planner_inst",
        node_target,
        "home",
        ServiceTarget::Any,
        Payload::new(),
        CALL_TIMEOUT,
    )
    .await;
    assert_eq!(*check.proofs_seen.lock().unwrap(), vec![false]);
}

#[tokio::test]
async fn a_daemon_takes_the_cancel_of_a_goal_from_its_sender_alone() {
    let session = mock_session().await;
    let holder = ClientKey::generate();
    let other = ClientKey::generate();
    let check = KeyCheck::default()
        .knowing("holder_inst", holder.public_key(), CallerKind::Instance)
        .knowing("other_inst", other.public_key(), CallerKind::Instance);
    let daemon = MessengerHandle::from_shared(Arc::clone(&session)).checking_calls_with(Arc::new(check));
    let fleet = || SenderTarget::contract("fleet", "v1").unwrap();
    let mut action = ConcurrentAction::expose(
        &daemon,
        DAEMON_CORE_NODE,
        DAEMON_INSTANCE_ID,
        fleet(),
        "lease",
        false,
    )
    .await
    .unwrap();
    let producer = ProducerRef::new(DAEMON_CORE_NODE, DAEMON_INSTANCE_ID);

    let holder_client = signing(MessengerHandle::from_shared(Arc::clone(&session)), holder);
    let send = ActionMessenger::send_goal(
        &holder_client,
        DAEMON_CORE_NODE,
        "holder_inst",
        fleet(),
        "lease",
        Some(&producer),
        Payload::new(),
        DEFAULT_FEEDBACK_BOUND,
        CALL_TIMEOUT,
    );
    let accept = async {
        let pending = action.recv_next_goal().await.unwrap().unwrap();
        assert_eq!(
            pending.verified_caller().map(VerifiedCaller::instance_id),
            Some("holder_inst")
        );
        pending.accept(Payload::new()).await.unwrap()
    };
    let (goal, context) = tokio::join!(send, accept);
    let goal = goal.unwrap();

    let other_client = signing(MessengerHandle::from_shared(session), other);
    let other_sender = ActionWireSender::new(
        DAEMON_CORE_NODE,
        "other_inst",
        Some(&producer),
        fleet(),
        "lease",
    )
    .unwrap();
    let refused =
        ActionMessenger::cancel_with_sender(&other_client, &other_sender, goal.goal_id(), CALL_TIMEOUT)
            .await
            .unwrap_err();
    assert!(
        refusal_message(refused).contains("a goal of another client"),
        "another client cannot cancel the goal"
    );

    let cancelled = ActionMessenger::cancel_goal(&holder_client, &goal, CALL_TIMEOUT)
        .await
        .unwrap();
    assert_eq!(
        decode_cancel_ack(cancelled.payload().as_ref()).unwrap(),
        CancelState::Signalled
    );
    context.cancel_signal().await;
}
