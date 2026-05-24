use crate::types::Payload;
use config::node::QoSProfile;
use pmi::{MessengerBackend, ZenohAdapter, ZenohdInstance};
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::error::Error;
use crate::messaging::{
    ActionMessenger, MessengerHandle, SenderTarget, ServiceMessenger, TopicMessenger,
};

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, "v1").expect("test node target")
}

#[derive(Clone)]
struct ActionClientCase {
    client_id: String,
    goal: Payload,
    goal_response: Payload,
    feedback: Payload,
}

impl ActionClientCase {
    fn new(prefix: &str, idx: usize) -> Self {
        let client_id = format!("{prefix}_{idx}");
        let goal = Payload::from(format!("client={client_id};goal_request={idx}").into_bytes());
        let goal_response =
            Payload::from(format!("client={client_id};goal_response=accepted").into_bytes());
        let feedback =
            Payload::from(format!("client={client_id};feedback=progress-{idx}").into_bytes());

        Self {
            client_id,
            goal,
            goal_response,
            feedback,
        }
    }
}

struct TestRouterContext {
    instance: ZenohdInstance,
}

impl TestRouterContext {
    async fn start() -> Self {
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for tests");
        Self { instance }
    }

    fn host(&self) -> &str {
        &self.instance.host
    }

    fn port(&self) -> u16 {
        self.instance.port
    }

    fn connection_target(&self) -> (String, u16) {
        (self.instance.host.clone(), self.instance.port)
    }

    async fn messenger(&self) -> MessengerHandle {
        connect_messenger(self.host(), self.port()).await
    }

    async fn shutdown(mut self) {
        self.instance
            .messenger()
            .stop_router()
            .await
            .expect("Failed to shutdown router");
    }
}

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    let mut last_error = None;
    for attempt in 0..MAX_RETRIES {
        match MessengerHandle::from_host_port(host, port).await {
            Ok(handle) => return handle,
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < MAX_RETRIES {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    panic!(
        "failed to connect messenger to {host}:{port} after {MAX_RETRIES} attempts: {:?}",
        last_error.unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_no_from_instance_id() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";
    let payload = Payload::from_static(b"A message");

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_handle = router.messenger().await;
    let subscriber_instance_id = "subscriber_instance";
    let mut subscription = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        None,
        topic,
        None, // Accepts any core node that emits
        None, // Accepts any instance id that emits
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Allow subscription to propagate before publishing
    tokio::time::sleep(Duration::from_millis(50)).await;

    let emitter_core_node = "core_node_emit";
    let emitter_instance_id = "emitter_instance";
    let emitter_handle = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle,
        emitter_core_node,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    assert_eq!(received.instance_id(), emitter_instance_id);
    assert_eq!(received.core_node(), emitter_core_node);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_with_from_instance_id() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";

    // Use the same core_node for both emitters to isolate instance_id filtering
    let emitter_core_node = "core_node_emit";

    // The messages emitted from this instance_id will never be received by any subscriber
    let emitter_instance_id1 = "emitter_instance1";

    // The messages emitted from this instance_id will be received by a subscriber
    let emitter_instance_id2 = "emitter_instance2";

    let payload = Payload::from_static(b"A message");

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_handle = router.messenger().await;

    let subscriber_instance_id1 = "subscriber_instance1";
    let mut subscription1 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id1,
        Some(test_node_target(node_name)),
        None,
        topic,
        Some(emitter_core_node),
        Some(emitter_instance_id1),
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Only this subscriber will receive a message
    let subscriber_instance_id2 = "subscriber_instance2";
    let mut subscription2 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id2,
        Some(test_node_target(node_name)),
        None,
        topic,
        Some(emitter_core_node),
        Some(emitter_instance_id2),
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Allow subscriptions to propagate before publishing
    tokio::time::sleep(Duration::from_millis(50)).await;

    let emitter_handle1 = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle1,
        emitter_core_node,
        emitter_instance_id2,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription2.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    // The first subscriber should never receive a message
    let timeout_result =
        tokio::time::timeout(Duration::from_secs(2), subscription1.on_next_message()).await;
    assert!(
        timeout_result.is_err(),
        "subscription1 should not receive any message"
    );

    // Only receive from emitter with instance_id2
    assert_eq!(received.core_node(), emitter_core_node);
    assert_eq!(received.instance_id(), emitter_instance_id2);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_with_from_core_node() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";

    // The messages emitted from this one will never be received by any subscriber
    let emitter_core_node1 = "core_node_emit1";
    let emitter_instance_id = "emitter_instance";

    // The messages emitted from this one will be received by a subscriber
    let emitter_core_node2 = "core_node_emit2";

    let payload = Payload::from_static(b"A message");

    // Same instance_id for every subscriber
    let subscriber_instance_id = "subscriber_instance";
    let subscriber_handle = router.messenger().await;

    let subscriber_core_node1 = "core_node_subscribe1";
    let mut subscription1 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node1,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        None,
        topic,
        Some(emitter_core_node1),
        Some(emitter_instance_id),
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Only this subscriber will receive a message
    let subscriber_core_node2 = "core_node_subscribe2";
    let mut subscription2 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node2,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        None,
        topic,
        Some(emitter_core_node2),
        Some(emitter_instance_id),
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Allow subscriptions to propagate before publishing
    tokio::time::sleep(Duration::from_millis(50)).await;

    let emitter_handle1 = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle1,
        emitter_core_node2,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription2.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    // The first subscriber should never receive a message
    let timeout_result =
        tokio::time::timeout(Duration::from_secs(2), subscription1.on_next_message()).await;
    assert!(
        timeout_result.is_err(),
        "subscription1 should not receive any message"
    );

    // Only receive from emitter 2
    assert_eq!(received.core_node(), emitter_core_node2);
    assert_eq!(received.instance_id(), emitter_instance_id);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn topic_publish_reliable_5000hz_messages() {
    let router = TestRouterContext::start().await;

    let node_name = "uvc_camera";
    let topic = "video_stream";
    let qos = QoSProfile::Reliable;

    let sender_handle = router.messenger().await;
    let receiver_handle = router.messenger().await;

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_instance_id = "subscriber_instance";
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        subscriber_core_node,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        None,
        topic,
        None,
        None,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Allow subscription to propagate before publishing
    tokio::time::sleep(Duration::from_millis(50)).await;

    let message_count = 5000;
    let emitter_core_node = "emitter_core_node";
    let emitter_instance_id = "emitter_instance";
    let mut message_ids: Vec<u32> = (0..message_count as u32).collect();
    let mut rng = rand::rng();
    message_ids.shuffle(&mut rng);

    for &message_id in &message_ids {
        let payload = Payload::from(message_id.to_le_bytes().to_vec());
        TopicMessenger::emit(
            &sender_handle,
            emitter_core_node,
            emitter_instance_id,
            test_node_target(node_name),
            topic,
            qos.clone(),
            payload,
        )
        .await
        .expect("Should send the payload");
    }

    // Identity check runs once on the first received message — the wire-format
    // contract is pinned in `pmi::wire::zenoh_format::tests`, so this loop only needs
    // to verify peppylib-level addressing and ordering.
    let mut received_ids: Vec<u32> = Vec::with_capacity(message_count);
    let mut identity_checked = false;
    for _ in 0..message_count {
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("Timed out waiting for a message")
            .expect("Subscription closed before receiving all messages");

        if !identity_checked {
            assert_eq!(message.core_node(), emitter_core_node);
            assert_eq!(message.instance_id(), emitter_instance_id);
            identity_checked = true;
        }

        let payload = message.payload();
        let payload_bytes = payload.as_ref();
        assert_eq!(
            payload_bytes.len(),
            std::mem::size_of::<u32>(),
            "Payload should encode the message index"
        );

        let mut id_bytes = [0u8; std::mem::size_of::<u32>()];
        id_bytes.copy_from_slice(payload_bytes.as_ref());
        let received_id = u32::from_le_bytes(id_bytes);

        received_ids.push(received_id);
    }

    assert_eq!(
        received_ids.len(),
        message_count,
        "should receive exactly {} messages",
        message_count
    );

    let mut expected_sorted = message_ids.clone();
    expected_sorted.sort_unstable();
    received_ids.sort_unstable();

    assert_eq!(
        received_ids, expected_sorted,
        "should receive every published message exactly once"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_communication_poll_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx1, service_ready_rx1) = oneshot::channel();
    let (service_ready_tx2, service_ready_rx2) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    let listener_core_node1 = "listener_core_node1";
    let listener_instance_id1 = "listener_instance1";
    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task1 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node1,
            listener_instance_id1,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx1.send(()).unwrap();
            // The handler may or may not be invoked depending on which
            // listener wins the discovery probe race; both outcomes are
            // valid. The `call_count == 1` assertion at the end verifies
            // that exactly one of the two listener handlers ran.
            let _ = tokio::time::timeout(service_wait_timeout, handler).await;

            Ok::<(), Error>(())
        })
    };

    // Second listener with the same service shape. Discovery sends a probe
    // to both listeners; the probe is auto-replied in the request loop
    // before the user handler runs, so the winner is whichever probe reply
    // reaches the caller first — a race with no inherent ordering.
    // Whichever listener loses, its user handler simply never executes.
    let listener_core_node2 = "listener_core_node2";
    let listener_instance_id2 = "listener_instance2";
    let service_task2 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node2,
            listener_instance_id2,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx2.send(()).unwrap();
            let _ = tokio::time::timeout(service_wait_timeout, handler).await;

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx1)
        .await
        .expect("service 1 should signal readiness before timeout")
        .expect("service 1 should signal readiness");

    tokio::time::timeout(service_ready_timeout, service_ready_rx2)
        .await
        .expect("service 2 should signal readiness before timeout")
        .expect("service 2 should signal readiness");

    // Allow services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            None, // Here we don't specify any node
            None, // We don't specify any instance_id target either
            request_payload.clone(),
            Duration::from_secs(2),
        )
        .await
        .expect("caller should receive response");

        // Discovery picks whichever listener replies to the probe first;
        // that is a wire-level race with no inherent ordering, so either
        // listener is a valid winner. We assert the response matches the
        // winning listener's identity and that exactly one user handler
        // ran (see `call_count` check below).
        let winning_core_node = if response.instance_id() == listener_instance_id1 {
            listener_core_node1
        } else if response.instance_id() == listener_instance_id2 {
            listener_core_node2
        } else {
            panic!(
                "response should come from one of the two listeners, got instance_id={}",
                response.instance_id()
            );
        };
        assert_eq!(response.core_node(), winning_core_node);
        assert_eq!(response.payload(), &response_payload);
    }

    tokio::time::timeout(service_task_timeout, service_task1)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, service_task2)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Only the fastest responder ran its user handler. `ServiceMessenger::poll`'s
    // discover-then-pin sequence sends a lightweight probe first (filtered
    // server-side before the user handler runs), then dispatches the real
    // request pinned to the first responding producer. Without discovery,
    // both producers' handlers would have run.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "only the discovered producer should run the user handler",
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_communication_poll_specific_instance_id() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx1, service_ready_rx1) = oneshot::channel();
    let (service_ready_tx2, service_ready_rx2) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_secs(1);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    // This listener is not our target
    let listener_core_node1 = "listener_core_node1";
    let listener_instance_id1 = "listener_instance1";
    let service_task1 = {
        let service_expose_handle = router.messenger().await;
        // This listener is not supposed to receive any message
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node1,
            listener_instance_id1,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        tokio::spawn(async move {
            service_ready_tx1.send(()).unwrap();

            let outcome = tokio::time::timeout(
                service_wait_timeout,
                service.handle_next_request(|_request| async {
                    Ok(Payload::from_static(b"unexpected response"))
                }),
            )
            .await;

            if outcome.is_err() {
                return Ok(()); // Timeout is expected - no request should be received
            }
            outcome.unwrap().map_or_else(Err, |handled| {
                panic!("non-targeted service should not receive the request (handled={handled})")
            })
        })
    };

    // Creates a second listener with a different ID (emulates a second instance). This is our target
    let listener_core_node2 = "listener_core_node2";
    let listener_instance_id2 = "listener_instance2";
    let service_task2 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node2,
            listener_instance_id2,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    // This second service instance is a bit slow for processing, but since it's been targeted, it's gonna be the one that responds
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(response_payload)
                }
            });

            service_ready_tx2.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler)
                .await
                .expect("service handler timed out");
            let handled = handled.expect("service should receive exactly one request");

            assert!(
                handled,
                "service subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx1)
        .await
        .expect("service 1 should signal readiness before timeout")
        .expect("service 1 should signal readiness");
    tokio::time::timeout(service_ready_timeout, service_ready_rx2)
        .await
        .expect("service 2 should signal readiness before timeout")
        .expect("service 2 should signal readiness");

    // Allow services to fully establish their listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            None,                        // Here we don't specify any target core node
            Some(listener_instance_id2), // We specify listener_instance_id2 as the target
            request_payload.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("caller should receive response");

        // Listener instance 2 is supposed to have responded since it's the target
        assert_eq!(response.instance_id(), listener_instance_id2);
        assert_eq!(response.core_node(), listener_core_node2);
        assert_eq!(response.payload(), &response_payload);
    }

    tokio::time::timeout(service_task_timeout, service_task1)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, service_task2)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Ensure the service callback was called exactly once (otherwise that means both services received the request)
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "service callback should have been called exactly once"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_wrong_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|_request| {
                let response_payload = Payload::from_static(b"ack");
                async move {
                    // This closure should never be called in this test since
                    // we're targeting the wrong node
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler).await;

            // Timeout is expected since the service should not receive a request
            assert!(
                handled.is_err(),
                "service handler should have timed out waiting for request"
            );

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx)
        .await
        .expect("service should signal readiness before timeout")
        .expect("service should signal readiness");

    // Allow the service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let err = {
            let result = ServiceMessenger::poll(
                &caller_handle,
                CALLER_CORE_NODE,
                CALLER_INSTANCE_ID,
                test_node_target(listener_node_name),
                None,
                listener_service_name,
                None,               // target_core_node
                Some("wrong_node"), // Use a wrong instance_id here
                request_payload.clone(),
                Duration::from_secs(1),
            )
            .await;

            let Err(err) = result else {
                panic!("service call should fail when targeting the wrong node");
            };

            err
        };

        let Error::ServiceUnreachable {
            instance_id: err_instance_id,
            service_name: err_service_name,
        } = &err
        else {
            panic!(
                "expected ServiceUnreachable error, received unexpected error: {:?}",
                err
            );
        };

        assert_eq!(err_instance_id.as_deref(), Some("wrong_node"));
        assert_eq!(err_service_name.as_str(), listener_service_name);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "service should not be called when targeting the wrong instance"
        );
    }

    // Ensure the service callback was not called at all
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "service callback should not have been called"
    );

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_wrong_core_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");

    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|_request| {
                let response_payload = Payload::from_static(b"ack");
                async move {
                    // This closure should never be called in this test since
                    // we're targeting the wrong node
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler).await;

            // Timeout is expected since the service should not receive a request
            assert!(
                handled.is_err(),
                "service handler should have timed out waiting for request"
            );

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");

        // Allow the service to fully establish its listener
        tokio::time::sleep(Duration::from_millis(50)).await;

        let caller_handle = router.messenger().await;
        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            Some("wrong_core_node"), // target_core_node - wrong one!
            None,                    // no specific target_instance_id
            request_payload.clone(),
            Duration::from_millis(200),
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when targeting the wrong core node");
        };

        err
    };

    let Error::ServiceUnreachable {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = &err
    else {
        panic!(
            "expected ServiceUnreachable error, received unexpected error: {:?}",
            err
        );
    };

    assert_eq!(err_instance_id.as_deref(), None); // No instance_id was targeted
    assert_eq!(err_service_name.as_str(), listener_service_name);

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_service_not_started() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_handle = router.messenger().await;

        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            None,
            None,
            Payload::from_static(b"enable=true"),
            Duration::from_secs(1),
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when service is not started");
        };

        err
    };

    let Error::ServiceUnreachable {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = err
    else {
        panic!(
            "expected ServiceUnreachable error, received unexpected error: {:?}",
            err
        );
    };

    assert_eq!(err_instance_id, None);
    assert_eq!(err_service_name, listener_service_name);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_service_timeouts() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout * 2 + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let response_delay = Duration::from_millis(200);

        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);
        let expected_requests = 2;

        tokio::spawn(async move {
            service_ready_tx.send(()).unwrap();

            for _ in 0..expected_requests {
                let response_payload = response_payload.clone();
                let call_count = Arc::clone(&call_count);

                let handled = tokio::time::timeout(
                    service_wait_timeout,
                    service.handle_next_request(|request| async move {
                        assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                        assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(response_delay).await;
                        Ok(response_payload)
                    }),
                )
                .await
                .expect("service handler timed out")
                .expect("service should receive expected number of requests");

                assert!(
                    handled,
                    "service subscription closed before handling request"
                );
            }

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx)
        .await
        .expect("service should signal readiness before timeout")
        .expect("service should signal readiness");

    // Allow the service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let request_payload = Payload::from_static(b"enable=true");
        let caller_success_timeout = Duration::from_millis(500);
        let caller_failure_timeout = Duration::from_millis(50);

        let caller_handle = router.messenger().await;

        let success_response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            None,
            None,
            request_payload.clone(),
            caller_success_timeout,
        )
        .await
        .expect("caller should receive response before timeout");
        assert_eq!(success_response.payload(), response_payload);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "service should have processed the successful request exactly once"
        );

        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_service_name,
            None,
            None,
            request_payload,
            caller_failure_timeout,
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when response exceeds timeout");
        };

        err
    };

    let Error::ServiceTimeout {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = &err
    else {
        panic!(
            "expected ServiceTimeout error for timeout, received: {:?}",
            err
        );
    };

    assert_eq!(
        err_instance_id.as_deref(),
        Some(listener_instance_id),
        "discover-then-pin resolves the wildcard target before the real poll, \
         so the timeout error carries the discovered instance_id",
    );
    assert_eq!(err_service_name.as_str(), listener_service_name);

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "service should have processed both requests"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn service_handle_request_processes_multiple_messages() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let expected_requests = 500;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let host = host.clone();

    let service_task = {
        let service_expose_handle = connect_messenger(&host, port).await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            service_ready_tx.send(()).unwrap();

            tokio::select! {
                result = service.handle_requests(|request| {
                    let call_count = Arc::clone(&call_count);
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message().payload())
                    }
                }) => result,
                _ = shutdown_rx => Ok(()),
            }
        })
    };

    service_ready_rx
        .await
        .expect("service should signal readiness");

    // Allow the service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        for i in 0..expected_requests {
            let request_payload = Payload::from(format!("enable=true;request={i}").into_bytes());
            let response = ServiceMessenger::poll(
                &caller_handle,
                CALLER_CORE_NODE,
                CALLER_INSTANCE_ID,
                test_node_target(listener_node_name),
                None,
                listener_service_name,
                None,
                Some(listener_instance_id),
                request_payload.clone(),
                Duration::from_secs(2),
            )
            .await
            .expect("caller should receive response");
            assert_eq!(
                response.payload(),
                request_payload,
                "response should match the originating request payload"
            );
        }
    }

    shutdown_tx.send(()).unwrap();

    let service_result = service_task.await.expect("service task panicked");
    service_result.expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        expected_requests,
        "service should process all requests"
    );

    router.shutdown().await;
}

/// Ensures a unique request returns its unique response
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn single_service_communication_multiple_polls_and_callers() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller core node (shared by all callers)
    const CALLER_CORE_NODE: &str = "caller_core_node";

    // TODO: 500 callers saturate Zenohd, it shouldn't
    let caller_count = 100;
    let requests_per_caller = 5;
    let total_requests = caller_count * requests_per_caller;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task: tokio::task::JoinHandle<Result<(), Error>> = {
        let service_expose_handle = router.messenger().await;

        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let mut in_flight = Vec::with_capacity(total_requests);
            service_ready_tx.send(()).unwrap();

            for _ in 0..total_requests {
                let call_count = Arc::clone(&call_count);

                let handle = service
                    .spawn_next_request_handler(move |request| async move {
                        assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message().payload())
                    })
                    .await
                    .expect("service should receive expected number of requests")
                    .expect("service subscription closed before handling request");

                in_flight.push(handle);
            }

            for handle in in_flight {
                handle
                    .await
                    .expect("service handler task panicked")
                    .expect("service handler task returned error");
            }

            Ok(())
        })
    };

    service_ready_rx
        .await
        .expect("service should signal readiness");

    // Allow the service to fully establish its listener
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let mut expected_payloads = HashMap::with_capacity(total_requests);
        let mut caller_requests = Vec::with_capacity(caller_count);

        for caller_idx in 0..caller_count {
            let caller_name = format!("vision_pipeline_{caller_idx}");
            let mut requests = Vec::with_capacity(requests_per_caller);
            for request_idx in 0..requests_per_caller {
                let payload = Payload::from(
                    format!("caller={caller_name};request={request_idx}").into_bytes(),
                );
                expected_payloads.insert((caller_name.clone(), request_idx), payload.clone());
                requests.push((request_idx, payload));
            }
            caller_requests.push((caller_name, requests));
        }

        let mut rng = rand::rng();
        caller_requests.shuffle(&mut rng);

        let mut handles = Vec::with_capacity(caller_count);
        for (caller_id, mut requests) in caller_requests {
            requests.shuffle(&mut rng);
            let host = host.clone();
            let poll_service = tokio::spawn(async move {
                let caller_handle = connect_messenger(&host, port).await;

                let mut caller_results = Vec::with_capacity(requests.len());
                for (request_idx, request_payload) in requests {
                    let response = ServiceMessenger::poll(
                        &caller_handle,
                        CALLER_CORE_NODE,
                        &caller_id,
                        test_node_target(listener_node_name),
                        None,
                        listener_service_name,
                        None,
                        Some(listener_instance_id),
                        request_payload.clone(),
                        Duration::from_secs(1),
                    )
                    .await
                    .expect("caller should receive response");

                    caller_results.push((
                        caller_id.clone(),
                        request_idx,
                        request_payload.clone(),
                        response,
                    ));
                }

                caller_results
            });
            handles.push(poll_service);
        }

        let mut results = Vec::with_capacity(total_requests);
        for handle in handles {
            let mut caller_results = handle.await.expect("poll_service task should not panic");
            results.append(&mut caller_results);
        }

        for (caller_id, request_idx, request_payload, response) in &results {
            let expected_payload = expected_payloads
                .remove(&(caller_id.clone(), *request_idx))
                .expect("expected payload should exist for caller/request pair");

            assert_eq!(
                request_payload, &expected_payload,
                "stored request payload should match expected value for `{caller_id}` request {request_idx}"
            );
            assert_eq!(
                response.payload(),
                expected_payload,
                "response for `{caller_id}` request {request_idx} should match the originating request payload"
            );
        }

        assert!(
            expected_payloads.is_empty(),
            "all expected caller/request pairs should have been validated"
        );
    };

    service_task
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    let actual_count = call_count.load(Ordering::SeqCst);
    assert_eq!(
        actual_count, total_requests,
        "service should have been called {total_requests} times"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let result_payload = Payload::from_static(b"done");

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // The factory unwraps the envelope and declares a per-goal
            // publisher in one async call via declare_from_wire.
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(expected_goal_response_payload)
                }
            });

            // Create the result handler future
            let result_handler = action.result_service.handle_next_request(move |request| {
                let response_payload = result_payload_server.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert!(request.message().payload().is_empty());

                    Ok(response_payload)
                }
            });

            // Signal ready after handler is set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            feedback_publisher
                .publish(
                    crate::messaging::NonEmptyPayload::try_new(feedback_payload_server.clone())
                        .expect("test feedback payload is non-empty"),
                )
                .await
                .expect("action should publish feedback");

            let handled_result = tokio::time::timeout(Duration::from_secs(5), result_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_result,
                "result subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // Allow the action server to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_action_name,
            None, // No target core_id
            None, // No target instance_id
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE);
        assert_eq!(
            goal_handle.goal_response().instance_id(),
            LISTENER_INSTANCE_ID
        );
        assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .on_next_feedback()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.payload(), &feedback_payload);
        assert_eq!(feedback_message.core_node(), LISTENER_CORE_NODE);
        assert_eq!(feedback_message.instance_id(), LISTENER_INSTANCE_ID);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::request_result(
            &caller_handle,
            &goal_handle,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.payload(), result_payload);
        assert_eq!(result_response.core_node(), LISTENER_CORE_NODE);
        assert_eq!(result_response.instance_id(), LISTENER_INSTANCE_ID);
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_with_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";

    const LISTENER_CORE_NODE1: &str = "listener_core_node1";
    const LISTENER_INSTANCE_ID1: &str = "listener_instance1";

    const LISTENER_CORE_NODE2: &str = "listener_core_node2";
    const LISTENER_INSTANCE_ID2: &str = "listener_instance2";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let result_payload = Payload::from_static(b"done");

    let call_count = Arc::new(AtomicUsize::new(0));

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    // This listener should not receive any message
    let server_task1 = {
        let expected_goal_response_payload = goal_response_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE1,
                LISTENER_INSTANCE_ID1,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let call_count = Arc::clone(&call_count);
            let call_count_for_closure = Arc::clone(&call_count);

            // Create the goal handler future first (this sets up the subscription)
            let goal_handler =
                action
                    .goal_service
                    .handle_next_request(move |_request| async move {
                        // This should never be reached
                        call_count_for_closure.fetch_add(1, Ordering::SeqCst);
                        Ok(expected_goal_response_payload)
                    });

            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler).await;

            assert!(
                handled_goal.is_err(),
                "server_task1 should not receive a goal request - timeout expected"
            );
            assert_eq!(
                call_count.load(Ordering::SeqCst),
                0,
                "goal handler should not have been called"
            );
            Ok::<(), Error>(())
        })
    };

    let server_task2 = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE2,
                LISTENER_INSTANCE_ID2,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // The factory unwraps the envelope and declares a per-goal
            // publisher in one async call via declare_from_wire.
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(expected_goal_response_payload)
                }
            });

            // Create the result handler future
            let result_handler = action.result_service.handle_next_request(move |request| {
                let response_payload = result_payload_server.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert!(request.message().payload().is_empty());

                    Ok(response_payload)
                }
            });

            // Signal ready after handler is set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            feedback_publisher
                .publish(
                    crate::messaging::NonEmptyPayload::try_new(feedback_payload_server.clone())
                        .expect("test feedback payload is non-empty"),
                )
                .await
                .expect("action should publish feedback");

            let handled_result = tokio::time::timeout(Duration::from_secs(5), result_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_result,
                "result subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // Allow the action server to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            None,
            listener_action_name,
            Some(LISTENER_CORE_NODE2),
            Some(LISTENER_INSTANCE_ID2),
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE2);
        assert_eq!(
            goal_handle.goal_response().instance_id(),
            LISTENER_INSTANCE_ID2
        );
        assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .on_next_feedback()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.payload(), &feedback_payload);
        assert_eq!(feedback_message.core_node(), LISTENER_CORE_NODE2);
        assert_eq!(feedback_message.instance_id(), LISTENER_INSTANCE_ID2);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::request_result(
            &caller_handle,
            &goal_handle,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.payload(), result_payload);
        assert_eq!(result_response.core_node(), LISTENER_CORE_NODE2);
        assert_eq!(result_response.instance_id(), LISTENER_INSTANCE_ID2);
    }

    server_task1
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    server_task2
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_goal_cancelled() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_action_name = "enable_camera";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let cancel_response_payload = Payload::from_static(b"cancelled");

    let goal_call_count = Arc::new(AtomicUsize::new(0));
    let cancel_call_count = Arc::new(AtomicUsize::new(0));

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let cancel_response_payload_server = cancel_response_payload.clone();
        let goal_call_count = Arc::clone(&goal_call_count);
        let cancel_call_count = Arc::clone(&cancel_call_count);

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // Create the goal handler future first (this sets up the subscription)
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let goal_call_count = Arc::clone(&goal_call_count);
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    goal_call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(expected_goal_response_payload)
                }
            });

            // Signal ready after handlers are set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let stop_feedback = Arc::new(tokio::sync::Notify::new());
            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            let feedback_task = {
                let stop_feedback = Arc::clone(&stop_feedback);
                let feedback_publisher = feedback_publisher.clone();
                let feedback_payload = feedback_payload_server.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_millis(50));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let stop_notified = stop_feedback.notified();
                    tokio::pin!(stop_notified);
                    loop {
                        tokio::select! {
                            biased;
                            _ = stop_notified.as_mut() => break,
                            _ = ticker.tick() => {
                                feedback_publisher
                                    .publish(
                                        crate::messaging::NonEmptyPayload::try_new(
                                            feedback_payload.clone(),
                                        )
                                        .expect("test feedback payload is non-empty"),
                                    )
                                    .await?;
                            }
                        }
                    }
                    Ok::<(), Error>(())
                })
            };

            let (cancel_context, cancel_responder) = tokio::time::timeout(
                Duration::from_secs(5),
                action.cancel_service.recv_next_request(),
            )
            .await
            .expect("timed out waiting for cancel request")
            .expect("action should receive cancel request")
            .expect("cancel subscription should not be closed");

            assert_eq!(cancel_context.message().core_node(), CALLER_CORE_NODE);
            assert_eq!(cancel_context.message().instance_id(), CALLER_INSTANCE_ID);
            assert!(
                cancel_context.message().payload().is_empty(),
                "cancel service should receive empty payload"
            );

            cancel_call_count.fetch_add(1, Ordering::SeqCst);

            // Stop feedback publication before acknowledging cancellation to reduce
            // flakiness caused by in-flight feedback after cancellation.
            stop_feedback.notify_waiters();
            feedback_task.await.expect("feedback loop task panicked")?;

            cancel_responder
                .respond(cancel_response_payload_server)
                .await?;

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // Allow the action server to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let caller_handle = router.messenger().await;

    let mut goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        CALLER_CORE_NODE,
        CALLER_INSTANCE_ID,
        test_node_target(listener_node_name),
        None,
        listener_action_name,
        Some(LISTENER_CORE_NODE),
        Some(LISTENER_INSTANCE_ID),
        goal_payload,
        QoSProfile::Reliable,
        Duration::from_millis(1000),
    )
    .await
    .expect("caller should send goal");

    assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE);
    assert_eq!(
        goal_handle.goal_response().instance_id(),
        LISTENER_INSTANCE_ID
    );
    assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

    let first_feedback = goal_handle
        .on_next_feedback()
        .await
        .expect("caller should receive initial feedback");

    assert_eq!(first_feedback.payload(), &feedback_payload);
    assert_eq!(first_feedback.core_node(), LISTENER_CORE_NODE);
    assert_eq!(first_feedback.instance_id(), LISTENER_INSTANCE_ID);

    let second_feedback =
        tokio::time::timeout(Duration::from_secs(1), goal_handle.on_next_feedback())
            .await
            .expect("feedback stream should continue delivering updates before cancellation")
            .expect("feedback stream closed unexpectedly before cancellation");

    assert_eq!(second_feedback.payload(), &feedback_payload);
    assert_eq!(second_feedback.core_node(), LISTENER_CORE_NODE);
    assert_eq!(second_feedback.instance_id(), LISTENER_INSTANCE_ID);

    let cancel_response =
        ActionMessenger::cancel_goal(&caller_handle, &goal_handle, Duration::from_millis(500))
            .await
            .expect("caller should receive cancel acknowledgement");

    assert_eq!(cancel_response.payload(), cancel_response_payload);
    assert_eq!(cancel_response.core_node(), LISTENER_CORE_NODE);
    assert_eq!(cancel_response.instance_id(), LISTENER_INSTANCE_ID);

    // Check that feedback eventually goes quiet after cancellation; allow a short window for
    // buffered/in-flight feedback messages to be drained.
    let quiet_for = Duration::from_millis(200);
    let overall_timeout = Duration::from_secs(2);
    let start = tokio::time::Instant::now();
    let mut quiet_deadline = start + quiet_for;

    loop {
        let now = tokio::time::Instant::now();
        if now >= quiet_deadline {
            break;
        }
        if now.duration_since(start) >= overall_timeout {
            panic!(
                "feedback did not stop within {:?} after cancellation",
                overall_timeout
            );
        }

        let remaining = quiet_deadline
            .checked_duration_since(now)
            .unwrap_or_default();

        match tokio::time::timeout(remaining, goal_handle.on_next_feedback()).await {
            Ok(Ok(_)) => {
                quiet_deadline = tokio::time::Instant::now() + quiet_for;
            }
            Ok(Err(Error::ActionFeedbackChannelClosed)) => break,
            Ok(Err(err)) => panic!("unexpected feedback error after cancellation: {err:?}"),
            Err(_) => break,
        }
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    assert_eq!(
        goal_call_count.load(Ordering::SeqCst),
        1,
        "goal handler should have been called exactly once"
    );
    assert_eq!(
        cancel_call_count.load(Ordering::SeqCst),
        1,
        "cancel handler should have been called exactly once"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_action_communication_multiple_polls() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_action_name = "enable_camera";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    let caller_prefix = "the_brain";

    const CLIENT_COUNT: usize = 8;
    let cases: Vec<_> = (0..CLIENT_COUNT)
        .map(|idx| ActionClientCase::new(caller_prefix, idx))
        .collect();
    let cases = Arc::new(cases);

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    // Launch a background task that plays the role of the action server.
    let server_task = {
        let action_handle = router.messenger().await;
        let action_ready_tx = Some(action_ready_tx);
        let cases = Arc::clone(&cases);

        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let crate::messaging::ActionCreation {
                mut goal_service,
                cancel_service: _,
                feedback_publisher_factory,
                mut result_service,
            } = action;
            let feedback_publisher_factory = Arc::new(feedback_publisher_factory);

            if let Some(tx) = action_ready_tx {
                let _ = tx.send(());
            }

            let client_total = cases.len();

            let mut goal_handlers = Vec::with_capacity(client_total);
            for _ in 0..client_total {
                let cases = Arc::clone(&cases);
                let factory = Arc::clone(&feedback_publisher_factory);

                let handler = goal_service
                    .spawn_next_request_handler(move |request| {
                        let cases = Arc::clone(&cases);
                        let factory = Arc::clone(&factory);

                        async move {
                            let declared = factory
                                .declare_from_wire("_", request.message().payload().into_inner())
                                .await
                                .expect("declare from wire");
                            let payload_str = std::str::from_utf8(&declared.user_payload)
                                .expect("goal payload should be valid UTF-8");

                            let client_id = payload_str
                                .split(';')
                                .find_map(|part| part.strip_prefix("client="))
                                .expect("goal payload should contain client identifier")
                                .to_string();

                            let case = cases
                                .iter()
                                .find(|case| case.client_id == client_id)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "goal handler received unexpected client id `{client_id}`"
                                    )
                                });

                            assert_eq!(
                                declared.user_payload,
                                case.goal.as_ref(),
                                "goal payload for `{client_id}` should match expected value"
                            );

                            declared
                                .publisher
                                .publish(
                                    crate::messaging::NonEmptyPayload::try_new(
                                        case.feedback.clone(),
                                    )
                                    .expect("test case feedback payload is non-empty"),
                                )
                                .await?;

                            Ok(case.goal_response.clone())
                        }
                    })
                    .await
                    .expect("action should spawn goal handler")
                    .expect("goal subscription closed before handling request");

                goal_handlers.push(handler);
            }

            for handler in goal_handlers {
                handler
                    .await
                    .expect("goal handler task panicked")
                    .expect("goal handler returned error");
            }

            let mut result_handlers = Vec::with_capacity(client_total);
            for _ in 0..client_total {
                let handler = result_service
                    .spawn_next_request_handler(move |request| async move {
                        assert!(request.message().payload().is_empty());

                        Ok(Payload::from_static(b"result=done"))
                    })
                    .await
                    .expect("action should spawn result handler")
                    .expect("result subscription closed before handling request");

                result_handlers.push(handler);
            }

            for handler in result_handlers {
                handler
                    .await
                    .expect("result handler task panicked")
                    .expect("result handler returned error");
            }

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // Allow the action server to fully establish its listeners
    tokio::time::sleep(Duration::from_millis(50)).await;

    let total_clients = cases.len();
    let mut shuffled_cases = cases.as_ref().clone();
    let mut rng = rand::rng();
    shuffled_cases.shuffle(&mut rng);

    let mut client_handles = Vec::with_capacity(total_clients);
    for case in shuffled_cases {
        let host = host.clone();
        let feedback_search_limit = total_clients;

        let handle = tokio::spawn(async move {
            let caller_handle = connect_messenger(&host, port).await;

            let mut goal_handle = ActionMessenger::send_goal(
                &caller_handle,
                CALLER_CORE_NODE,
                &case.client_id,
                test_node_target(listener_node_name),
                None,
                listener_action_name,
                None,
                None,
                case.goal.clone(),
                QoSProfile::Reliable,
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should send goal");

            assert_eq!(
                goal_handle.goal_response().payload(),
                case.goal_response.clone(),
                "goal response should match expected payload for `{}`",
                case.client_id
            );

            let mut feedback_matched = false;
            for _ in 0..feedback_search_limit {
                let feedback_message = goal_handle
                    .on_next_feedback()
                    .await
                    .expect("caller should receive feedback message");

                if feedback_message.payload() == case.feedback {
                    feedback_matched = true;
                    break;
                }
            }

            assert!(
                feedback_matched,
                "caller `{}` should observe its corresponding feedback payload",
                case.client_id
            );

            let result_response = ActionMessenger::request_result(
                &caller_handle,
                &goal_handle,
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should receive result response");

            assert_eq!(
                result_response.payload(),
                Payload::from_static(b"result=done"),
                "result response should match expected payload for `{}`",
                case.client_id
            );
        });

        client_handles.push(handle);
    }

    for handle in client_handles {
        handle.await.expect("caller task should not panic");
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

// ─── link_id queryable dispatch ────────────────────────────────────────────
//
// These tests pin the producer-side dispatch behavior: a single queryable
// per `listen_service` call (`*` at the link_id slot) with the adapter's
// `handle_queryable` claiming a concrete bound link_id per inbound request
// via `ParsedInboundQuery::choose_link_id`. Cross-talk, ACK ordering,
// per-goal feedback routing, and `from_any` single-invocation are the
// failure modes the design must not introduce; each gets a dedicated
// test below.

const LINK_LEFT: &str = "wrist_left";
const LINK_TORSO: &str = "torso";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_listen_drops_request_for_unbound_link_id_without_ack() {
    // Producer binds only LINK_LEFT. A consumer pins to LINK_TORSO.
    // The producer's queryable now carries `*` at the link_id slot, so
    // the consumer's `torso` literal selector intersects and the query
    // does reach the producer's `handle_queryable`. The dispatcher then
    // checks the parsed link_id against the bound set (`["wrist_left"]`),
    // finds no match, and drops the query silently — no reply, no ACK.
    // The consumer surfaces `ServiceUnreachable` and the user handler
    // must NEVER fire.
    let router = TestRouterContext::start().await;
    let server_handle = router.messenger().await;
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        "start_recording",
    )
    .await
    .expect("listen should succeed");

    let handler_fired = Arc::new(AtomicUsize::new(0));
    let handler_fired_clone = Arc::clone(&handler_fired);
    let server_task = tokio::spawn(async move {
        // The wildcard queryable matches the `torso` selector first, so the
        // request reaches the producer's dispatcher; `choose_link_id` then
        // returns `None` because `torso` isn't in the bound set and the
        // request is dropped without being handed to `handle_next_request`.
        // That leaves this call blocked indefinitely, so we race it against
        // a wall-clock budget; the counter trips only if the handler runs.
        let _ = tokio::time::timeout(
            Duration::from_millis(500),
            endpoint.handle_next_request(move |_ctx| {
                let fired = Arc::clone(&handler_fired_clone);
                async move {
                    fired.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from_static(b"unexpected"))
                }
            }),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = router.messenger().await;
    let err = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "caller_inst_torso",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        Some(LINK_TORSO),
        "start_recording",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"go"),
        Duration::from_millis(250),
    )
    .await
    .expect_err("poll to unbound link_id must not succeed");
    match err {
        Error::ServiceUnreachable { .. } => {}
        other => panic!("expected ServiceUnreachable, got {other:?}"),
    }
    assert_eq!(
        handler_fired.load(Ordering::SeqCst),
        0,
        "user handler must not run for an unbound link_id"
    );
    server_task.await.expect("server task panicked");
    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_duplicate_from_any_subscription_is_rejected() {
    // The wire-level dedupe for wildcard topic subscribers (the
    // primary/secondary attachment plus the sibling-exclusion filter)
    // depends on the manifest validator's "at most one from_any consumer
    // per (name, tag)" invariant. Anything that bypasses the validator
    // (a test, a tooling integration, a future runtime-deps feature) can
    // install state that violates it and corrupt aggregator state with
    // silent duplicate deliveries.
    //
    // This asserts the runtime guard at `MessengerHandle::subscribe`
    // catches the violation: two from_any topic subscriptions on the same
    // `(name, tag)` cannot coexist. After dropping the first the slot is
    // released so a later subscription succeeds, and the surviving
    // from_any subscription still receives exactly one delivery per
    // multi-link emit (proving the existing dedupe still works under the
    // enforced invariant).
    let router = TestRouterContext::start().await;
    let subscriber_handle = router.messenger().await;

    let target = || SenderTarget::interface("depth_camera", "v1").expect("iface target");

    let sub_one = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_one",
        Some(target()),
        None,
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("first from_any subscribe should succeed");

    let second = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_two",
        Some(target()),
        None,
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await;
    match second {
        Err(Error::DuplicateFromAnyConsumer { ref name, ref tag })
            if name == "depth_camera" && tag == "v1" => {}
        Err(other) => panic!("unexpected error rejecting second from_any: {other:?}"),
        Ok(_) => panic!("second from_any subscribe must be rejected, got Ok"),
    }

    // A pinned subscription on the same (name, tag) is unaffected — only
    // from_any subs take the slot.
    let _sub_pinned = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_pinned",
        Some(target()),
        Some(LINK_LEFT),
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("pinned subscribe on same (name, tag) must coexist with from_any");

    // Drop the first from_any sub; its guard releases the slot and the
    // next from_any subscribe should succeed.
    drop(sub_one);

    let mut sub_three = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_three",
        Some(target()),
        None,
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("from_any subscribe should succeed after the first guard dropped");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let emitter_handle = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle,
        "pub_core",
        "pub_inst",
        target(),
        "frames",
        QoSProfile::Reliable,
        Payload::from_static(b"frame-0"),
    )
    .await
    .expect("emit should succeed");

    // Exactly one delivery on the surviving from_any sub. The degenerate
    // sibling map intentionally fails to claim either bound link_id, so
    // the existing dedupe (primary/secondary attachment) must do the work
    // alone — and the runtime guard ensures it isn't asked to do more.
    let first = tokio::time::timeout(Duration::from_secs(2), sub_three.on_next_message())
        .await
        .expect("from_any subscriber should not time out")
        .expect("from_any subscriber should receive a message");
    assert_eq!(first.payload().as_ref(), b"frame-0");
    let dup = tokio::time::timeout(Duration::from_millis(300), sub_three.on_next_message()).await;
    assert!(
        dup.is_err(),
        "from_any subscriber must not receive a duplicate (got {:?})",
        dup.ok().flatten().map(|m| m.payload().as_ref().to_vec())
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn action_from_any_send_goal_runs_handler_on_winner_only() {
    // Two producer processes expose the same action and a consumer sends
    // a wildcard goal. With discover-then-pin, only ONE producer's goal
    // handler must run — the one that responds first to the discovery
    // probe. The loser sees the probe (filtered internally, no handler
    // invocation) but never sees the real goal. The subsequent
    // `cancel_goal` also targets only the winner because the wire sender
    // was pinned at discovery time.
    let router = TestRouterContext::start().await;

    let server_a_core = "server_a_core";
    let server_a_inst = "server_a_inst";
    let server_b_core = "server_b_core";
    let server_b_inst = "server_b_inst";
    let action_target = SenderTarget::interface("manipulator", "v1").expect("iface target");
    let action_name = "abort_safe";

    struct ProducerSpec {
        core: &'static str,
        inst: &'static str,
        target: SenderTarget,
        action_name: &'static str,
    }

    struct ProducerCounters {
        goal: Arc<AtomicUsize>,
        cancel: Arc<AtomicUsize>,
    }

    async fn spawn_producer(
        router: &TestRouterContext,
        spec: ProducerSpec,
        counters: ProducerCounters,
        ready: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = router.messenger().await;
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                spec.core,
                spec.inst,
                spec.target,
                spec.action_name,
            )
            .await
            .expect("expose should succeed");

            let mut goal_service = action.goal_service;
            let mut cancel_service = action.cancel_service;
            ready.send(()).expect("ready");

            // The loser must time out here; the winner returns immediately.
            match tokio::time::timeout(Duration::from_millis(800), goal_service.recv_next_request())
                .await
            {
                Ok(Ok(Some((_ctx, goal_responder)))) => {
                    counters.goal.fetch_add(1, Ordering::SeqCst);
                    goal_responder
                        .respond(Payload::from(spec.inst.as_bytes().to_vec()))
                        .await
                        .expect("goal respond");
                }
                _ => {
                    // No goal arrived within the budget; producer must be
                    // the loser of the discovery race.
                    return;
                }
            }

            // Only the winner reaches this point. Wait for the cancel
            // that send_goal's pinned sender will direct here.
            if let Ok(Ok(Some((_ctx, responder)))) = tokio::time::timeout(
                Duration::from_millis(800),
                cancel_service.recv_next_request(),
            )
            .await
            {
                counters.cancel.fetch_add(1, Ordering::SeqCst);
                let _ = responder.respond(Payload::from_static(b"cancelled")).await;
            }
        })
    }

    let goal_a = Arc::new(AtomicUsize::new(0));
    let goal_b = Arc::new(AtomicUsize::new(0));
    let cancel_a = Arc::new(AtomicUsize::new(0));
    let cancel_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();

    let task_a = spawn_producer(
        &router,
        ProducerSpec {
            core: server_a_core,
            inst: server_a_inst,
            target: action_target.clone(),
            action_name,
        },
        ProducerCounters {
            goal: Arc::clone(&goal_a),
            cancel: Arc::clone(&cancel_a),
        },
        ready_a_tx,
    )
    .await;
    let task_b = spawn_producer(
        &router,
        ProducerSpec {
            core: server_b_core,
            inst: server_b_inst,
            target: action_target.clone(),
            action_name,
        },
        ProducerCounters {
            goal: Arc::clone(&goal_b),
            cancel: Arc::clone(&cancel_b),
        },
        ready_b_tx,
    )
    .await;

    ready_a_rx.await.expect("server A ready");
    ready_b_rx.await.expect("server B ready");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = router.messenger().await;
    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        action_target,
        None,
        action_name,
        None, // wildcard target_core_node
        None, // wildcard target_instance_id
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    let winner_inst = goal_handle.goal_response().instance_id().to_string();
    let winner_core = goal_handle.goal_response().core_node().to_string();
    assert!(
        winner_inst == server_a_inst || winner_inst == server_b_inst,
        "goal_response identity must come from one of the producers, got {winner_inst:?}",
    );
    assert!(
        winner_core == server_a_core || winner_core == server_b_core,
        "goal_response core_node must come from one of the producers, got {winner_core:?}",
    );

    let _ = ActionMessenger::cancel_goal(&caller_handle, &goal_handle, Duration::from_secs(1))
        .await
        .expect("cancel_goal should reach the latched producer");

    task_a.await.expect("server A task panicked");
    task_b.await.expect("server B task panicked");

    let (winner_goal, loser_goal, winner_cancel, loser_cancel) = if winner_inst == server_a_inst {
        (
            goal_a.load(Ordering::SeqCst),
            goal_b.load(Ordering::SeqCst),
            cancel_a.load(Ordering::SeqCst),
            cancel_b.load(Ordering::SeqCst),
        )
    } else {
        (
            goal_b.load(Ordering::SeqCst),
            goal_a.load(Ordering::SeqCst),
            cancel_b.load(Ordering::SeqCst),
            cancel_a.load(Ordering::SeqCst),
        )
    };

    assert_eq!(
        winner_goal, 1,
        "winning producer ({winner_inst}) should have run its goal handler exactly once",
    );
    assert_eq!(
        loser_goal, 0,
        "losing producer must NOT run its goal handler — discovery pins to the winner before the real goal is sent",
    );
    assert_eq!(
        winner_cancel, 1,
        "winning producer should have received the cancel",
    );
    assert_eq!(
        loser_cancel, 0,
        "losing producer must NOT receive the cancel — sender was pinned at discovery time",
    );

    router.shutdown().await;
}
