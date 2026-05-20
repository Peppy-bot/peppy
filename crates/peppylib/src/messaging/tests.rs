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
    ActionMessenger, MessengerHandle, NonEmptyPayload, SenderTarget, ServiceMessenger,
    TopicMessenger,
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
        &[],
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
        &[],
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
        &[],
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
            &[],
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
            &[],
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

    // Creates a second listener (emulates a second instance) that is slower than the listener 1 to respond
    let listener_core_node2 = "listener_core_node2";
    let listener_instance_id2 = "listener_instance2";
    let service_task2 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node2,
            listener_instance_id2,
            test_node_target(listener_node_name),
            &[],
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
                    // This listener also receive the request, it just won't repond in time
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    // This second service instance is a bit slow for processing, so the first listener service will respond first
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
            None, // Here we don't specify any node
            None, // We don't specify any instance_id target either
            request_payload.clone(),
            Duration::from_secs(2),
        )
        .await
        .expect("caller should receive response");

        // Listener instance 1 is supposed to have responded more quickly here
        assert_eq!(response.instance_id(), listener_instance_id1);
        assert_eq!(response.core_node(), listener_core_node1);
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

    // The two services received the request, but only the fastest one has reponded to the sender
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "service callback should have been called exactly once"
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
            &[],
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
            &[],
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
            &[],
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
                None,               // to_core_node
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
            &[],
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
            Some("wrong_core_node"), // to_core_node - wrong one!
            None,                    // no specific to_instance_id
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
            &[],
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
        None,
        "should report unreachable target instance (unknown when no target instance was specified)"
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
            &[],
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
            &[],
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
                &[],
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
                &[],
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
                &[],
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
                &[],
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
                &[],
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
const LINK_RIGHT: &str = "wrist_right";
const LINK_TORSO: &str = "torso";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_listen_dispatches_under_each_bound_link_id() {
    // Producer binds two link_ids on one listen call. Two consumers pin to
    // different link_ids; both must reach the same handler and receive the
    // response addressed to their pinned link_id. Plain happy path.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];
    let server_handle = router.messenger().await;
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "start_recording",
    )
    .await
    .expect("listen should succeed");

    let server_task = tokio::spawn(async move {
        for _ in 0..2 {
            endpoint
                .handle_next_request(|ctx| {
                    let link_id = ctx.link_id().to_string();
                    async move { Ok(Payload::from(link_id.into_bytes())) }
                })
                .await
                .expect("handle_next_request should succeed");
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = router.messenger().await;
    let response_left = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "caller_inst_left",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        Some(LINK_LEFT),
        "start_recording",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"go"),
        Duration::from_secs(2),
    )
    .await
    .expect("left poll should succeed");
    assert_eq!(response_left.payload().as_ref(), LINK_LEFT.as_bytes());

    let response_right = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "caller_inst_right",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        Some(LINK_RIGHT),
        "start_recording",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"go"),
        Duration::from_secs(2),
    )
    .await
    .expect("right poll should succeed");
    assert_eq!(response_right.payload().as_ref(), LINK_RIGHT.as_bytes());

    server_task.await.expect("server task should not panic");
    router.shutdown().await;
}

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
    let bound = vec![LINK_LEFT.to_string()];
    let server_handle = router.messenger().await;
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "start_recording",
    )
    .await
    .expect("listen should succeed");

    let handler_fired = Arc::new(AtomicUsize::new(0));
    let handler_fired_clone = Arc::clone(&handler_fired);
    let server_task = tokio::spawn(async move {
        // Race a request loop against a shutdown signal: Zenoh's keyexpr
        // matcher should refuse to route the unbound-link_id selector to any
        // of this producer's queryables, so handle_next_request would block
        // forever. We bail out after a wall-clock budget; if the handler
        // ever runs the counter trips.
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
async fn service_from_any_consumer_reaches_concrete_link_id_producer() {
    // A `from_any: true` consumer (`to_link_id: None`) must reach a producer
    // bound to a specific link_id. `session.get` accepts Zenoh wildcards, so
    // `to_link_id: None` emits `*` at the link_id slot and the matcher routes
    // the query to the producer's concrete-link_id queryable.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string()];
    let server_handle = router.messenger().await;
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "start_recording",
    )
    .await
    .expect("listen should succeed");

    let server_task = tokio::spawn(async move {
        endpoint
            .handle_next_request(|ctx| {
                let link_id = ctx.link_id().to_string();
                async move { Ok(Payload::from(link_id.into_bytes())) }
            })
            .await
            .expect("handle_next_request should succeed");
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = router.messenger().await;
    let response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "from_any_caller",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        None, // ← `from_any: true` semantics — the exact case that was broken
        "start_recording",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"go"),
        Duration::from_secs(2),
    )
    .await
    .expect(
        "from_any consumer must reach the concrete-link_id producer (queryable selector wildcards \
         the link_id slot, so Zenoh matches it against the producer's `wrist_left` literal)",
    );
    assert_eq!(
        response.payload().as_ref(),
        LINK_LEFT.as_bytes(),
        "producer should respond stamped with its own bound link_id"
    );

    server_task.await.expect("server task should not panic");
    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_feedback_routes_per_goal_link_id() {
    // One producer binds two link_ids. Two consumers send goals targeting
    // different link_ids. The producer's per-goal feedback must address
    // each consumer's pinned link_id. A regression of this routing would
    // either deliver feedback to the wrong consumer or fail to deliver at
    // all (an invalid keyexpr containing `*`).
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];
    let server_handle = router.messenger().await;
    let action = ActionMessenger::expose(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "record",
    )
    .await
    .expect("expose should succeed");

    let mut goal_service = action.goal_service;
    let mut cancel_service = action.cancel_service;
    let mut result_service = action.result_service;
    let factory = action.feedback_publisher_factory;

    // Handle two goals: each emits one feedback message echoing the
    // observed link_id, then accepts the result request and closes.
    let server_task = tokio::spawn(async move {
        for _ in 0..2 {
            // goal
            let (ctx, responder) = goal_service
                .recv_next_request()
                .await
                .expect("goal recv")
                .expect("goal closed");
            let link_id = ctx.link_id().to_string();
            let wire = ctx.message().payload().into_inner();
            let declared = factory
                .declare_from_wire(&link_id, wire)
                .await
                .expect("declare_from_wire");
            responder
                .respond(Payload::from(format!("accepted={link_id}").into_bytes()))
                .await
                .expect("goal respond");
            declared
                .publisher
                .publish(
                    NonEmptyPayload::try_new(Payload::from(
                        format!("feedback={link_id}").into_bytes(),
                    ))
                    .expect("non-empty feedback"),
                )
                .await
                .expect("feedback publish");

            // Result request closes the feedback stream and answers.
            let (_result_ctx, result_responder) = result_service
                .recv_next_request()
                .await
                .expect("result recv")
                .expect("result closed");
            // Send end-of-stream so the consumer's feedback loop terminates.
            declared.publisher.publish_end().await.expect("publish_end");
            result_responder
                .respond(Payload::from(format!("result={link_id}").into_bytes()))
                .await
                .expect("result respond");
        }
        // Drain any spurious cancel deliveries (none expected).
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            cancel_service.recv_next_request(),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let exercise_link = |link_id: &'static str| {
        let caller_handle = router.messenger();
        async move {
            let caller_handle = caller_handle.await;
            let mut goal_handle = ActionMessenger::send_goal(
                &caller_handle,
                "caller_core",
                &format!("caller_inst_{link_id}"),
                SenderTarget::interface("depth_camera", "v1").expect("iface target"),
                Some(link_id),
                "record",
                Some("server_core"),
                Some("server_inst"),
                Payload::from_static(b"start"),
                QoSProfile::Reliable,
                Duration::from_secs(2),
            )
            .await
            .expect("send_goal should succeed");

            let goal_response = goal_handle.goal_response().payload().as_ref().to_vec();
            assert_eq!(
                goal_response,
                format!("accepted={link_id}").into_bytes(),
                "goal response should echo the targeted link_id"
            );

            let feedback = goal_handle
                .on_next_feedback()
                .await
                .expect("feedback should be delivered");
            assert_eq!(
                feedback.payload().as_ref(),
                format!("feedback={link_id}").as_bytes(),
                "feedback must be scoped to the consumer's link_id, not crossed"
            );

            let result = ActionMessenger::request_result(
                &caller_handle,
                &goal_handle,
                Duration::from_secs(2),
            )
            .await
            .expect("request_result should succeed");
            assert_eq!(
                result.payload().as_ref(),
                format!("result={link_id}").as_bytes(),
                "result must come from the same link_id-scoped goal cycle"
            );
        }
    };

    // Sequence the two consumers so the producer task's `for _ in 0..2` is
    // deterministic; a parallel issue would let the producer pick up goals
    // in either order.
    exercise_link(LINK_LEFT).await;
    exercise_link(LINK_RIGHT).await;

    server_task.await.expect("server task panicked");
    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_multi_link_producer_from_any_consumer_fires_handler_exactly_once() {
    // Regression test for the duplicate-dispatch bug: a producer binding two
    // link_ids on one `listen_service` and a `from_any` consumer (no
    // `to_link_id` pin) used to fire the user handler twice — once per bound
    // link_id — because each link_id declared its own queryable and Zenoh's
    // `QueryTarget::All` delivered the consumer's `*` selector to every
    // matching queryable in the same process. After the fix a single
    // queryable absorbs both and the dispatcher claims `bound_link_ids[0]`,
    // so the handler fires exactly once.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];
    let server_handle = router.messenger().await;
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "start_recording",
    )
    .await
    .expect("listen should succeed");

    let handler_fired = Arc::new(AtomicUsize::new(0));
    let handler_fired_clone = Arc::clone(&handler_fired);
    let server_task = tokio::spawn(async move {
        // Serve the one expected request, then race the second
        // `handle_next_request` against a short deadline so a duplicate
        // dispatch trips the counter without hanging the test forever.
        endpoint
            .handle_next_request(move |ctx| {
                let fired = Arc::clone(&handler_fired_clone);
                let link_id = ctx.link_id().to_string();
                async move {
                    fired.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from(link_id.into_bytes()))
                }
            })
            .await
            .expect("first handle_next_request should succeed");

        let _ = tokio::time::timeout(
            Duration::from_millis(250),
            endpoint.handle_next_request(|_ctx| async move {
                Ok(Payload::from_static(b"unexpected_duplicate"))
            }),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let caller_handle = router.messenger().await;
    let response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "from_any_caller",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        None, // ← `from_any: true` — the case that double-dispatched before.
        "start_recording",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"go"),
        Duration::from_secs(2),
    )
    .await
    .expect("from_any poll against multi-link producer should succeed");
    assert_eq!(
        response.payload().as_ref(),
        LINK_LEFT.as_bytes(),
        "first-bound dispatch policy: producer should claim bound_link_ids[0]"
    );

    server_task.await.expect("server task panicked");

    assert_eq!(
        handler_fired.load(Ordering::SeqCst),
        1,
        "user handler must fire exactly once for one from_any consumer call",
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn action_multi_link_producer_from_any_consumer_fires_goal_handler_exactly_once() {
    // Action-flavored regression: each action sub-service (goal, cancel,
    // result) runs the dispatcher independently. First-bound policy keeps
    // the link_id consistent across the lifecycle, so the user observes a
    // single goal/cancel/result invocation per consumer call. Without the
    // fix the producer would race two goal handlers on the same
    // consumer-generated `goal_id` and declare two feedback publishers.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];
    let server_handle = router.messenger().await;
    let action = ActionMessenger::expose(
        &server_handle,
        "server_core",
        "server_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "record",
    )
    .await
    .expect("expose should succeed");

    let mut goal_service = action.goal_service;
    let mut cancel_service = action.cancel_service;
    let mut result_service = action.result_service;
    let factory = action.feedback_publisher_factory;

    let goal_handler_count = Arc::new(AtomicUsize::new(0));
    let result_handler_count = Arc::new(AtomicUsize::new(0));
    let goal_count_clone = Arc::clone(&goal_handler_count);
    let result_count_clone = Arc::clone(&result_handler_count);

    let server_task = tokio::spawn(async move {
        // Goal
        let (ctx, responder) = goal_service
            .recv_next_request()
            .await
            .expect("goal recv")
            .expect("goal closed");
        let link_id = ctx.link_id().to_string();
        let wire = ctx.message().payload().into_inner();
        let declared = factory
            .declare_from_wire(&link_id, wire)
            .await
            .expect("declare_from_wire");
        goal_count_clone.fetch_add(1, Ordering::SeqCst);
        responder
            .respond(Payload::from(format!("accepted={link_id}").into_bytes()))
            .await
            .expect("goal respond");
        declared
            .publisher
            .publish(
                NonEmptyPayload::try_new(Payload::from(format!("feedback={link_id}").into_bytes()))
                    .expect("non-empty feedback"),
            )
            .await
            .expect("feedback publish");

        // Result
        let (_result_ctx, result_responder) = result_service
            .recv_next_request()
            .await
            .expect("result recv")
            .expect("result closed");
        result_count_clone.fetch_add(1, Ordering::SeqCst);
        declared.publisher.publish_end().await.expect("publish_end");
        result_responder
            .respond(Payload::from(format!("result={link_id}").into_bytes()))
            .await
            .expect("result respond");

        // Drain a potential duplicate goal that would only arrive if the
        // bug were still present; bounded so the test ends in finite time.
        let dup_goal =
            tokio::time::timeout(Duration::from_millis(250), goal_service.recv_next_request())
                .await;
        if let Ok(Ok(Some((dup_ctx, dup_responder)))) = dup_goal {
            goal_count_clone.fetch_add(1, Ordering::SeqCst);
            // Best-effort respond so the consumer side doesn't hang on
            // unexpected extra deliveries while we fail the assertion.
            let dup_link_id = dup_ctx.link_id().to_string();
            let _ = dup_responder
                .respond(Payload::from(format!("dup={dup_link_id}").into_bytes()))
                .await;
        }

        // Cancel must not be invoked at all.
        let _ = tokio::time::timeout(
            Duration::from_millis(50),
            cancel_service.recv_next_request(),
        )
        .await;
    });

    tokio::time::sleep(Duration::from_millis(150)).await;

    let caller_handle = router.messenger().await;
    let mut goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "from_any_caller",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        None, // ← `from_any: true` — the case that double-dispatched before.
        "record",
        Some("server_core"),
        Some("server_inst"),
        Payload::from_static(b"start"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    assert_eq!(
        goal_handle.goal_response().payload().as_ref(),
        format!("accepted={LINK_LEFT}").into_bytes(),
        "first-bound dispatch policy: goal handler should observe bound_link_ids[0]",
    );

    let feedback = goal_handle
        .on_next_feedback()
        .await
        .expect("feedback should be delivered");
    assert_eq!(
        feedback.payload().as_ref(),
        format!("feedback={LINK_LEFT}").as_bytes(),
        "only one feedback publisher should exist — under the first-bound link_id",
    );

    let result =
        ActionMessenger::request_result(&caller_handle, &goal_handle, Duration::from_secs(2))
            .await
            .expect("request_result should succeed");
    assert_eq!(
        result.payload().as_ref(),
        format!("result={LINK_LEFT}").as_bytes(),
        "result handler must observe the same link_id the goal handler did",
    );

    server_task.await.expect("server task panicked");

    assert_eq!(
        goal_handler_count.load(Ordering::SeqCst),
        1,
        "goal handler must fire exactly once for one from_any send_goal call",
    );
    assert_eq!(
        result_handler_count.load(Ordering::SeqCst),
        1,
        "result handler must fire exactly once for one from_any request_result call",
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_emit_fans_out_to_every_bound_link_id() {
    // One emit on the producer side becomes N wire publishes. Two
    // consumers pin to different link_ids; each must receive its own
    // copy with the correct link_id in the wire path.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];

    let subscriber_handle = router.messenger().await;
    let mut sub_left = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_left",
        Some(SenderTarget::interface("depth_camera", "v1").expect("iface target")),
        Some(LINK_LEFT),
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("left subscribe should succeed");
    let mut sub_right = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_right",
        Some(SenderTarget::interface("depth_camera", "v1").expect("iface target")),
        Some(LINK_RIGHT),
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("right subscribe should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let emitter_handle = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle,
        "pub_core",
        "pub_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "frames",
        QoSProfile::Reliable,
        Payload::from_static(b"frame-0"),
    )
    .await
    .expect("emit should succeed");

    let recv_left = tokio::time::timeout(Duration::from_secs(2), sub_left.on_next_message())
        .await
        .expect("left subscriber should not time out")
        .expect("left subscriber should receive a message");
    assert_eq!(recv_left.payload().as_ref(), b"frame-0");

    let recv_right = tokio::time::timeout(Duration::from_secs(2), sub_right.on_next_message())
        .await
        .expect("right subscriber should not time out")
        .expect("right subscriber should receive a message");
    assert_eq!(recv_right.payload().as_ref(), b"frame-0");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_emit_delivers_once_to_wildcard_subscriber() {
    // Producer bound to two link_ids. A `from_link_id: None` subscriber
    // wildcards the link_id slot and intersects every per-link_id publish
    // the emit loop produces. The "primary marker" attachment must collapse
    // those N publishes back to one delivery on the wildcard axis. Pinned
    // subscribers on each bound link_id must continue to receive one
    // message each — specifically the regression case is the publish for
    // `effective[1..]` (marked secondary) that pinned subscribers still
    // need to receive on their pinned keyexpr.
    let router = TestRouterContext::start().await;
    let bound = vec![LINK_LEFT.to_string(), LINK_RIGHT.to_string()];

    let subscriber_handle = router.messenger().await;
    let mut sub_any = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_any",
        Some(SenderTarget::interface("depth_camera", "v1").expect("iface target")),
        None,
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("wildcard subscribe should succeed");
    let mut sub_left = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_left",
        Some(SenderTarget::interface("depth_camera", "v1").expect("iface target")),
        Some(LINK_LEFT),
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("left subscribe should succeed");
    let mut sub_right = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_right",
        Some(SenderTarget::interface("depth_camera", "v1").expect("iface target")),
        Some(LINK_RIGHT),
        "frames",
        None,
        None,
        QoSProfile::Reliable,
    )
    .await
    .expect("right subscribe should succeed");

    tokio::time::sleep(Duration::from_millis(100)).await;

    let emitter_handle = router.messenger().await;
    TopicMessenger::emit(
        &emitter_handle,
        "pub_core",
        "pub_inst",
        SenderTarget::interface("depth_camera", "v1").expect("iface target"),
        &bound,
        "frames",
        QoSProfile::Reliable,
        Payload::from_static(b"frame-0"),
    )
    .await
    .expect("emit should succeed");

    // Wildcard subscriber: exactly one delivery.
    let first = tokio::time::timeout(Duration::from_secs(2), sub_any.on_next_message())
        .await
        .expect("wildcard subscriber should not time out")
        .expect("wildcard subscriber should receive a message");
    assert_eq!(first.payload().as_ref(), b"frame-0");

    // Grace window: no duplicate arrives. 300ms is well past the loopback
    // round-trip the previous publish completed in.
    let second = tokio::time::timeout(Duration::from_millis(300), sub_any.on_next_message()).await;
    assert!(
        second.is_err(),
        "wildcard subscriber must not receive a duplicate (got {:?})",
        second
            .ok()
            .flatten()
            .map(|m| m.payload().as_ref().to_vec())
    );

    // Pinned subscribers each still receive their one copy. This proves the
    // secondary publish for the non-first-bound link_id (LINK_RIGHT here)
    // still reaches its pinned subscriber — pinned subscribers ignore the
    // primary/secondary marker because their keyexpr already filters to
    // exactly one publish per emit.
    let left = tokio::time::timeout(Duration::from_secs(1), sub_left.on_next_message())
        .await
        .expect("left subscriber should not time out")
        .expect("left subscriber should receive a message");
    assert_eq!(left.payload().as_ref(), b"frame-0");
    let right = tokio::time::timeout(Duration::from_secs(1), sub_right.on_next_message())
        .await
        .expect("right subscriber should not time out")
        .expect("right subscriber should receive a message");
    assert_eq!(right.payload().as_ref(), b"frame-0");

    router.shutdown().await;
}
