use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{Messenger, MessengerBackend};
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::oneshot;

use crate::error::Error;
use crate::messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};

const MASTER_NODE_NAME: &str = "master_node";

#[derive(Clone)]
struct ActionClientCase {
    client_id: String,
    goal: Bytes,
    goal_response: Bytes,
    feedback: Bytes,
    result_request: Bytes,
    result_response: Bytes,
}

impl ActionClientCase {
    fn new(prefix: &str, idx: usize) -> Self {
        let client_id = format!("{prefix}_{idx}");
        let goal = Bytes::from(format!("client={client_id};goal_request={idx}").into_bytes());
        let goal_response =
            Bytes::from(format!("client={client_id};goal_response=accepted").into_bytes());
        let feedback =
            Bytes::from(format!("client={client_id};feedback=progress-{idx}").into_bytes());
        let result_request =
            Bytes::from(format!("client={client_id};result_request={idx}").into_bytes());
        let result_response = Bytes::from(format!("client={client_id};result=done").into_bytes());

        Self {
            client_id,
            goal,
            goal_response,
            feedback,
            result_request,
            result_response,
        }
    }
}

struct TestRouterContext {
    router: Messenger,
    _temp_dir: TempDir,
    host: String,
    port: u16,
}

impl TestRouterContext {
    async fn start() -> Self {
        let (router, temp_dir, host, port) = crate::start_zenohd_process("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for tests");
        Self {
            router,
            _temp_dir: temp_dir,
            host,
            port,
        }
    }

    fn host(&self) -> &str {
        &self.host
    }

    fn port(&self) -> u16 {
        self.port
    }

    fn connection_target(&self) -> (String, u16) {
        (self.host.clone(), self.port)
    }

    async fn topic_messenger(&self) -> MessengerHandle {
        connect_topic_messenger(self.host(), self.port()).await
    }

    async fn service_messenger(&self) -> MessengerHandle {
        connect_service_messenger(self.host(), self.port()).await
    }

    async fn action_messenger(&self) -> MessengerHandle {
        connect_action_messenger(self.host(), self.port()).await
    }

    async fn shutdown(mut self) {
        self.router
            .stop_router()
            .await
            .expect("Failed to shutdown router");
    }
}

async fn connect_topic_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!("failed to create topic messenger on {host}:{port}: {error:?}")
        })
}

async fn connect_service_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!("failed to create service messenger on {host}:{port}: {error:?}")
        })
}

async fn connect_action_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!("failed to create action messenger on {host}:{port}: {error:?}")
        })
}

#[test]
fn build_master_key_expr_removes_redundant_separators() {
    let path = super::build_master_key_expr("master", "/service", "/camera/rear/", "/video_frame");
    assert_eq!(path, "master/service/camera/rear/video_frame");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_frame";

    let payload = Bytes::from_static(b"A message");

    let sender_handle = router.topic_messenger().await;
    let receiver_handle = router.topic_messenger().await;

    let mut subscription =
        TopicMessenger::subscribe(&receiver_handle, &node_name, &topic, qos.clone())
            .await
            .expect("Should subscribe to the topic");

    let instance_id = "emitter_instance";
    TopicMessenger::emit(
        &sender_handle,
        MASTER_NODE_NAME,
        &node_name,
        &topic,
        &instance_id,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription.rx.recv())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    let expected_topic = format!(
        "{}/topic/{}/{}/<INSTANCE_ID:{}>",
        MASTER_NODE_NAME, node_name, topic, instance_id
    );

    assert_eq!(received.key_expr(), expected_topic);
    assert_eq!(received.instance_id(), instance_id);
    assert_eq!(received.master_node(), MASTER_NODE_NAME);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn topic_publish_reliable_5000hz_messages() {
    let router = TestRouterContext::start().await;

    let node_name = "uvc_camera";
    let topic = "video_frame";
    let qos = QoSProfile::Reliable;

    let sender_handle = router.topic_messenger().await;
    let receiver_handle = router.topic_messenger().await;

    let mut subscription =
        TopicMessenger::subscribe(&receiver_handle, &node_name, &topic, qos.clone())
            .await
            .expect("Should subscribe to the topic");

    let message_count = 5000;
    let instance_id = "emitter_instance";
    let mut message_ids: Vec<u32> = (0..message_count as u32).collect();
    let mut rng = rand::rng();
    message_ids.shuffle(&mut rng);

    for &message_id in &message_ids {
        let payload = Bytes::from(message_id.to_le_bytes().to_vec());
        TopicMessenger::emit(
            &sender_handle,
            MASTER_NODE_NAME,
            &node_name,
            &topic,
            &instance_id,
            qos.clone(),
            payload,
        )
        .await
        .expect("Should send the payload");
    }

    let expected_key_expr = format!(
        "{}/topic/{}/{}/<INSTANCE_ID:{}>",
        MASTER_NODE_NAME, node_name, topic, instance_id
    );

    let mut received_ids: Vec<u32> = Vec::with_capacity(message_count);
    for _ in 0..message_count {
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.rx.recv())
            .await
            .expect("Timed out waiting for a message")
            .expect("Subscription closed before receiving all messages");

        assert_eq!(message.key_expr(), expected_key_expr);

        let payload = message.payload();
        let payload_bytes = payload.as_bytes();
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_specific_instance_id() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.service_messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            MASTER_NODE_NAME,
            listener_node_name,
            listener_service_name,
            listener_instance_id,
        )
        .await
        .expect("service should start");

        let service_root = super::build_master_key_expr(
            MASTER_NODE_NAME,
            "service",
            listener_node_name,
            listener_service_name,
        );
        let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
        let expected_request_topic = format!(
            "{service_root}/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
        );

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().key_expr(), expected_request_topic);
                    assert_eq!(request.message().instance_id(), caller_instance_id);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");
        let caller_handle = router.service_messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
            Some(listener_instance_id),
            request_payload.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("caller should receive response");

        assert_eq!(response.payload().to_bytes(), response_payload);
        assert_eq!(response.instance_id(), listener_instance_id);
    }

    // Ensure the service callback was called exactly once
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "service callback should have been called exactly once"
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
async fn service_communication_poll_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.service_messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            MASTER_NODE_NAME,
            listener_node_name,
            listener_service_name,
            listener_instance_id,
        )
        .await
        .expect("service should start");

        let service_root = super::build_master_key_expr(
            MASTER_NODE_NAME,
            "service",
            listener_node_name,
            listener_service_name,
        );
        let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
        let expected_request_topic = format!(
            "{service_root}/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
        );

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().key_expr(), expected_request_topic);
                    assert_eq!(request.message().instance_id(), caller_instance_id);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");
        let caller_handle = router.service_messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
            None, // Here we don't specify any node
            request_payload.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("caller should receive response");

        assert_eq!(response.payload().to_bytes(), response_payload);
        assert_eq!(response.instance_id(), listener_instance_id);
    }

    // Ensure the service callback was called exactly once
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "service callback should have been called exactly once"
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
async fn service_communication_poll_wrong_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.service_messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            MASTER_NODE_NAME,
            listener_node_name,
            listener_service_name,
            listener_instance_id,
        )
        .await
        .expect("service should start");

        let service_root = super::build_master_key_expr(
            MASTER_NODE_NAME,
            "service",
            listener_node_name,
            listener_service_name,
        );
        let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
        let expected_request_topic = format!(
            "{service_root}/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
        );

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().key_expr(), expected_request_topic);
                    assert_eq!(request.message().instance_id(), caller_instance_id);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler).await;

            if let Ok(handled) = handled {
                let handled = handled.expect("service should receive exactly one request");

                assert!(
                    handled,
                    "service subscription closed before handling request"
                );
            }

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");
        let caller_handle = router.service_messenger().await;
        let err = {
            let result = ServiceMessenger::poll(
                &caller_handle,
                MASTER_NODE_NAME,
                caller_instance_id,
                listener_node_name,
                listener_service_name,
                Some("wrong_node"), // Use a wrong node name here
                request_payload.clone(),
                Duration::from_secs(1),
            )
            .await;

            let Err(err) = result else {
                panic!("service call should fail when targeting the wrong node");
            };

            err
        };

        let Error::ServiceTimeout {
            instance_id: err_instance_id,
            service_name: err_service_name,
        } = &err
        else {
            panic!(
                "expected ServiceTimeout error, received unexpected error: {:?}",
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
async fn service_communication_poll_wrong_master_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let wrong_master_node = "wrong_master";

    let request_payload = Bytes::from_static(b"enable=true");

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.service_messenger().await;

        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &service_expose_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_service_name,
                listener_instance_id,
            )
            .await
            .expect("service should start");

            service_ready_tx.send(()).unwrap();

            let outcome = tokio::time::timeout(
                service_wait_timeout,
                service.handle_next_request(|_request| async {
                    Ok(Bytes::from_static(b"unexpected payload"))
                }),
            )
            .await;

            match outcome {
                Ok(Ok(_)) => {
                    panic!("service should not receive a request on the wrong master node")
                }
                Ok(Err(err)) => Err(err),
                Err(_) => Ok(()),
            }
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");

        let caller_handle = router.service_messenger().await;
        let result = ServiceMessenger::poll(
            &caller_handle,
            wrong_master_node,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
            Some(listener_instance_id),
            request_payload.clone(),
            Duration::from_millis(200),
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when targeting the wrong master node");
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

    assert_eq!(err_instance_id.as_deref(), Some(listener_instance_id));
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

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_request_routed_to_target_instance_only() {
    let router = TestRouterContext::start().await;

    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let target_instance_id = "listener_primary";
    let other_instance_id = "listener_secondary";
    let caller_instance_id = "caller_instance";

    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let service_wait_timeout = Duration::from_millis(500);

    let (service_a_ready_tx, service_a_ready_rx) = oneshot::channel();
    let (service_b_ready_tx, service_b_ready_rx) = oneshot::channel();

    let service_a_task = {
        let service_expose_handle = router.service_messenger().await;
        let response_payload = response_payload.clone();
        let request_payload = request_payload.clone();

        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &service_expose_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_service_name,
                target_instance_id,
            )
            .await
            .expect("service A should start");

            let _ = service_a_ready_tx.send(());

            let handled = service
                .handle_next_request(|request| {
                    let response_payload = response_payload.clone();
                    let request_payload = request_payload.clone();
                    async move {
                        assert_eq!(request.message().instance_id(), caller_instance_id);
                        assert_eq!(request.message().payload(), &request_payload);
                        Ok(response_payload)
                    }
                })
                .await
                .expect("service A should process the targeted request");

            assert!(handled, "service A subscription closed unexpectedly");

            Ok::<(), Error>(())
        })
    };

    let service_b_task = {
        let service_expose_handle = router.service_messenger().await;

        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &service_expose_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_service_name,
                other_instance_id,
            )
            .await
            .expect("service B should start");

            let _ = service_b_ready_tx.send(());

            let outcome = tokio::time::timeout(
                service_wait_timeout,
                service.handle_next_request(|_request| async {
                    Ok(Bytes::from_static(b"unexpected payload"))
                }),
            )
            .await;

            match outcome {
                Ok(Ok(handled)) => panic!(
                    "non-targeted service should not receive the request (handled={handled})"
                ),
                Ok(Err(err)) => Err(err),
                Err(_) => Ok(()),
            }
        })
    };

    service_a_ready_rx
        .await
        .expect("service A should signal readiness");
    service_b_ready_rx
        .await
        .expect("service B should signal readiness");

    let caller_handle = router.service_messenger().await;
    let response = ServiceMessenger::poll(
        &caller_handle,
        MASTER_NODE_NAME,
        caller_instance_id,
        listener_node_name,
        listener_service_name,
        Some(target_instance_id),
        request_payload.clone(),
        Duration::from_millis(500),
    )
    .await
    .expect("caller should receive response from targeted instance");

    assert_eq!(response.payload().to_bytes(), response_payload);
    assert_eq!(response.instance_id(), target_instance_id);

    service_a_task
        .await
        .expect("service A task panicked")
        .expect("service A task returned error");
    service_b_task
        .await
        .expect("service B task panicked")
        .expect("service B task returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_not_started() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    let caller_instance_id = "caller_instance";

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_handle = router.service_messenger().await;

        let result = ServiceMessenger::poll(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
            None,
            Bytes::from_static(b"enable=true"),
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
async fn service_communication_fails_timeout() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout * 2 + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_root = super::build_master_key_expr(
            MASTER_NODE_NAME,
            "service",
            listener_node_name,
            listener_service_name,
        );
        let expected_request_key_expr = format!(
            "{service_root}/<INSTANCE_ID:{listener_instance_id}>/request/<INSTANCE_ID:{caller_instance_id}>"
        );
        let response_delay = Duration::from_millis(200);

        let service_expose_handle = router.service_messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            MASTER_NODE_NAME,
            listener_node_name,
            listener_service_name,
            listener_instance_id,
        )
        .await
        .expect("service should start");

        let expected_request_topic = expected_request_key_expr.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);
        let response_delay = response_delay;
        let expected_requests = 2;

        tokio::spawn(async move {
            service_ready_tx.send(()).unwrap();

            for _ in 0..expected_requests {
                let expected_request_topic = expected_request_topic.clone();
                let response_payload = response_payload.clone();
                let call_count = Arc::clone(&call_count);
                let response_delay = response_delay;

                let handled = tokio::time::timeout(
                    service_wait_timeout,
                    service.handle_next_request(|request| async move {
                        assert_eq!(request.message().key_expr(), expected_request_topic);
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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let request_payload = Bytes::from_static(b"enable=true");
        let caller_success_timeout = Duration::from_millis(500);
        let caller_failure_timeout = Duration::from_millis(50);

        let caller_handle = router.service_messenger().await;

        let success_response = ServiceMessenger::poll(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
            None,
            request_payload.clone(),
            caller_success_timeout,
        )
        .await
        .expect("caller should receive response before timeout");
        assert_eq!(success_response.payload().to_bytes(), response_payload);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "service should have processed the successful request exactly once"
        );

        let result = ServiceMessenger::poll(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_service_name,
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

    // Caller instance
    let caller_instance_id = "caller_instance";

    let expected_requests = 500;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (service_ready_tx, service_ready_rx) = oneshot::channel();

    let service_task = {
        let call_count = Arc::clone(&call_count);
        let host = host.clone();
        let service_ready_tx = Some(service_ready_tx);
        tokio::spawn(async move {
            let service_expose_handle = connect_service_messenger(&host, port).await;

            let mut service = ServiceMessenger::listen(
                &service_expose_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_service_name,
                listener_instance_id,
            )
            .await
            .expect("service should start");

            if let Some(tx) = service_ready_tx {
                let _ = tx.send(());
            }

            tokio::select! {
                result = service.handle_requests(|request| {
                    let call_count = Arc::clone(&call_count);
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message().payload().to_bytes())
                    }
                }) => result,
                _ = shutdown_rx => Ok(()),
            }
        })
    };

    service_ready_rx
        .await
        .expect("service should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.service_messenger().await;

        for i in 0..expected_requests {
            let request_payload = Bytes::from(format!("enable=true;request={i}").into_bytes());
            let response = ServiceMessenger::poll(
                &caller_handle,
                MASTER_NODE_NAME,
                caller_instance_id,
                listener_node_name,
                listener_service_name,
                Some(listener_instance_id),
                request_payload.clone(),
                Duration::from_secs(2),
            )
            .await
            .expect("caller should receive response");
            assert_eq!(
                response.payload().to_bytes(),
                request_payload,
                "response should match the originating request payload"
            );
        }
    }

    let _ = shutdown_tx.send(());

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

    // TODO: 500 callers saturate Zenohd, it shouldn't
    let caller_count = 100;
    let requests_per_caller = 5;
    let total_requests = caller_count * requests_per_caller;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task: tokio::task::JoinHandle<Result<(), Error>> = {
        let service_expose_handle = router.service_messenger().await;

        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            MASTER_NODE_NAME,
            listener_node_name,
            listener_service_name,
            listener_instance_id,
        )
        .await
        .expect("service should start");

        let service_root = super::build_master_key_expr(
            MASTER_NODE_NAME,
            "service",
            listener_node_name,
            listener_service_name,
        );

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let mut in_flight = Vec::with_capacity(total_requests);
            service_ready_tx.send(()).unwrap();

            for _ in 0..total_requests {
                let service_root = service_root.clone();
                let listener_instance_id = listener_instance_id.to_string();
                let call_count = Arc::clone(&call_count);

                let handle = service
                    .spawn_next_request_handler(move |request| async move {
                        let identifier = request.message().key_expr().to_string();
                        let payload = request.message().payload().to_bytes();
                        let caller_id = request.message().instance_id();
                        let expected_identifier = format!(
                            "{service_root}/<INSTANCE_ID:{listener_instance_id}>/request/<INSTANCE_ID:{caller_id}>"
                        );
                        assert_eq!(identifier, expected_identifier);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(payload)
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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let mut expected_payloads = HashMap::with_capacity(total_requests);
        let mut caller_requests = Vec::with_capacity(caller_count);

        for caller_idx in 0..caller_count {
            let caller_name = format!("vision_pipeline_{caller_idx}");
            let mut requests = Vec::with_capacity(requests_per_caller);
            for request_idx in 0..requests_per_caller {
                let payload =
                    Bytes::from(format!("caller={caller_name};request={request_idx}").into_bytes());
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
                let caller_handle = connect_service_messenger(&host, port).await;

                let mut caller_results = Vec::with_capacity(requests.len());
                for (request_idx, request_payload) in requests {
                    let response = ServiceMessenger::poll(
                        &caller_handle,
                        MASTER_NODE_NAME,
                        &caller_id,
                        listener_node_name,
                        listener_service_name,
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

        let mut verification_indices: Vec<usize> = (0..results.len()).collect();
        let original_indices = verification_indices.clone();
        let mut rng = rand::rng();
        verification_indices.shuffle(&mut rng);
        if verification_indices == original_indices {
            verification_indices.rotate_left(1);
        }

        for index in verification_indices {
            let (caller_id, request_idx, request_payload, response) = &results[index];
            let expected_payload = expected_payloads
                .remove(&(caller_id.clone(), *request_idx))
                .expect("expected payload should exist for caller/request pair");

            assert_eq!(
                request_payload, &expected_payload,
                "stored request payload should match expected value for `{caller_id}` request {request_idx}"
            );
            assert_eq!(
                response.payload().to_bytes(),
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

    let expected_count = call_count.load(Ordering::SeqCst);
    assert_eq!(
        expected_count, total_requests,
        "service should have been called {total_requests} times"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let goal_payload = Bytes::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Bytes::from_static(b"accepted");
    let feedback_payload = Bytes::from_static(b"progress=50");
    let result_payload = Bytes::from_static(b"done");
    let result_request_payload = Bytes::from_static(b"goal=right_arm");

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let goal_payload_server = goal_payload.clone();
        let goal_response_payload_server = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();
        let result_request_payload_server = result_request_payload.clone();

        let action_handle = router.action_messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::listen(
                &action_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_action_name,
                listener_instance_id,
            )
            .await
            .expect("action should start");

            let action_root = super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name,
            );
            let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
            let expected_goal_topic = format!(
                "{action_root}/goal/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );
            let expected_result_topic = format!(
                "{action_root}/result/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );

            let _ = action_ready_tx.send(());

            // Wait for the client to send a goal request
            let handled_goal = action
                .goal_service
                .handle_next_request(move |request| {
                    let expected_goal_topic = expected_goal_topic.clone();
                    let expected_goal_payload = goal_payload_server.clone();
                    let expected_goal_response_payload = goal_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_goal_topic);
                        assert_eq!(request.message().payload(), &expected_goal_payload);
                        Ok(expected_goal_response_payload)
                    }
                })
                .await
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            action
                .feedback_publisher
                .publish_with_prefix(MASTER_NODE_NAME, feedback_payload_server.clone())
                .await
                .expect("action should publish feedback");

            let handled_result = action
                .result_service
                .handle_next_request(move |request| {
                    let expected_topic = expected_result_topic.clone();
                    let expected_payload = result_request_payload_server.clone();
                    let response_payload = result_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_topic);
                        assert_eq!(request.message().payload(), &expected_payload);
                        Ok(response_payload)
                    }
                })
                .await
                .expect("action should receive result request");

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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.action_messenger().await;

        // Client sends the goal and obtains the handle carrying goal response + feedback sub.
        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_action_name,
            Some(listener_instance_id),
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(
            goal_handle.goal_response().payload().to_bytes(),
            goal_response_payload
        );

        let expected_feedback_topic = format!(
            "{}/feedback/<INSTANCE_ID:{listener_instance_id}>",
            super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name
            )
        );

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .feedback_mut()
            .rx
            .recv()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.key_expr(), expected_feedback_topic);
        assert_eq!(feedback_message.payload(), &feedback_payload);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::poll_result(
            &caller_handle,
            caller_instance_id,
            &goal_handle,
            result_request_payload,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.payload().to_bytes(), result_payload);
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let goal_payload = Bytes::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Bytes::from_static(b"accepted");
    let feedback_payload = Bytes::from_static(b"progress=50");
    let result_payload = Bytes::from_static(b"done");
    let result_request_payload = Bytes::from_static(b"goal=right_arm");

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let goal_payload_server = goal_payload.clone();
        let goal_response_payload_server = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();
        let result_request_payload_server = result_request_payload.clone();

        let action_handle = router.action_messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::listen(
                &action_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_action_name,
                listener_instance_id,
            )
            .await
            .expect("action should start");

            let action_root = super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name,
            );
            let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
            let expected_goal_topic = format!(
                "{action_root}/goal/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );
            let expected_result_topic = format!(
                "{action_root}/result/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );

            let _ = action_ready_tx.send(());

            // Wait for the client to send a goal request
            let handled_goal = action
                .goal_service
                .handle_next_request(move |request| {
                    let expected_goal_topic = expected_goal_topic.clone();
                    let expected_goal_payload = goal_payload_server.clone();
                    let expected_goal_response_payload = goal_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_goal_topic);
                        assert_eq!(request.message().payload(), &expected_goal_payload);
                        Ok(expected_goal_response_payload)
                    }
                })
                .await
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            action
                .feedback_publisher
                .publish_with_prefix(MASTER_NODE_NAME, feedback_payload_server.clone())
                .await
                .expect("action should publish feedback");

            let handled_result = action
                .result_service
                .handle_next_request(move |request| {
                    let expected_topic = expected_result_topic.clone();
                    let expected_payload = result_request_payload_server.clone();
                    let response_payload = result_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_topic);
                        assert_eq!(request.message().payload(), &expected_payload);
                        Ok(response_payload)
                    }
                })
                .await
                .expect("action should receive result request");

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

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.action_messenger().await;

        // Client sends the goal and obtains the handle carrying goal response + feedback sub.
        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            MASTER_NODE_NAME,
            caller_instance_id,
            listener_node_name,
            listener_action_name,
            None,
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(
            goal_handle.goal_response().payload().to_bytes(),
            goal_response_payload
        );

        let expected_feedback_topic = format!(
            "{}/feedback/<INSTANCE_ID:{listener_instance_id}>",
            super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name
            )
        );

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .feedback_mut()
            .rx
            .recv()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.key_expr(), expected_feedback_topic);
        assert_eq!(feedback_message.payload(), &feedback_payload);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::poll_result(
            &caller_handle,
            caller_instance_id,
            &goal_handle,
            result_request_payload,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.payload().to_bytes(), result_payload);
    }

    server_task
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
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_instance_id = "caller_instance";

    let goal_payload = Bytes::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Bytes::from_static(b"accepted");
    let feedback_payload = Bytes::from_static(b"progress=50");
    let result_request_payload = Bytes::from_static(b"goal=right_arm");
    let cancel_response_payload = Bytes::from_static(b"cancelled");

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let goal_payload_server = goal_payload.clone();
        let goal_response_payload_server = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let cancel_response_payload_server = cancel_response_payload.clone();

        let action_handle = router.action_messenger().await;
        let action_ready_tx = Some(action_ready_tx);

        tokio::spawn(async move {
            let action = ActionMessenger::listen(
                &action_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_action_name,
                listener_instance_id,
            )
            .await
            .expect("action should start");

            let crate::messaging::ActionCreation {
                mut goal_service,
                mut cancel_service,
                feedback_publisher,
                ..
            } = action;

            let action_root = super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name,
            );
            let listener_instance_segment = format!("<INSTANCE_ID:{listener_instance_id}>");
            let expected_goal_topic = format!(
                "{action_root}/goal/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );
            let expected_cancel_topic = format!(
                "{action_root}/cancel/{listener_instance_segment}/request/<INSTANCE_ID:{caller_instance_id}>"
            );

            if let Some(tx) = action_ready_tx {
                let _ = tx.send(());
            }

            let handled_goal = goal_service
                .handle_next_request(move |request| {
                    let expected_goal_topic = expected_goal_topic.clone();
                    let expected_goal_payload = goal_payload_server.clone();
                    let expected_goal_response_payload = goal_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_goal_topic);
                        assert_eq!(request.message().payload(), &expected_goal_payload);
                        Ok(expected_goal_response_payload)
                    }
                })
                .await
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let stop_feedback = Arc::new(tokio::sync::Notify::new());
            let feedback_task = {
                let stop_feedback = Arc::clone(&stop_feedback);
                let feedback_publisher = feedback_publisher;
                let feedback_payload = feedback_payload_server.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_millis(50));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let stop_notified = stop_feedback.notified();
                    tokio::pin!(stop_notified);
                    loop {
                        tokio::select! {
                            _ = stop_notified.as_mut() => break,
                            _ = ticker.tick() => {
                                feedback_publisher
                                    .publish_with_prefix(MASTER_NODE_NAME, feedback_payload.clone())
                                    .await?;
                            }
                        }
                    }
                    Ok::<(), Error>(())
                })
            };

            let handled_cancel = cancel_service
                .handle_next_request(move |request| {
                    let expected_topic = expected_cancel_topic.clone();
                    let response_payload = cancel_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message().key_expr(), expected_topic);
                        assert!(
                            request.message().payload().is_empty(),
                            "cancel service should receive empty payload"
                        );
                        Ok(response_payload)
                    }
                })
                .await
                .expect("action should receive cancel request");

            assert!(
                handled_cancel,
                "cancel subscription closed before handling request"
            );

            stop_feedback.notify_waiters();

            feedback_task.await.expect("feedback loop task panicked")?;

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    let caller_handle = router.action_messenger().await;

    let mut goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        MASTER_NODE_NAME,
        caller_instance_id,
        listener_node_name,
        listener_action_name,
        Some(listener_instance_id),
        goal_payload,
        QoSProfile::Reliable,
        Duration::from_millis(1000),
    )
    .await
    .expect("caller should send goal");

    assert_eq!(
        goal_handle.goal_response().payload().to_bytes(),
        goal_response_payload
    );

    let expected_feedback_topic = format!(
        "{}/feedback/<INSTANCE_ID:{listener_instance_id}>",
        super::build_master_key_expr(
            MASTER_NODE_NAME,
            "action",
            listener_node_name,
            listener_action_name
        )
    );

    let first_feedback = goal_handle
        .feedback_mut()
        .rx
        .recv()
        .await
        .expect("caller should receive initial feedback");

    assert_eq!(first_feedback.key_expr(), expected_feedback_topic);
    assert_eq!(first_feedback.payload(), &feedback_payload);

    let second_feedback = tokio::time::timeout(
        Duration::from_millis(200),
        goal_handle.feedback_mut().rx.recv(),
    )
    .await
    .expect("feedback stream should continue delivering updates before cancellation")
    .expect("feedback stream closed unexpectedly before cancellation");

    assert_eq!(second_feedback.key_expr(), expected_feedback_topic);
    assert_eq!(second_feedback.payload(), &feedback_payload);

    let cancel_response = ActionMessenger::cancel_goal(
        &caller_handle,
        caller_instance_id,
        &goal_handle,
        Duration::from_millis(500),
    )
    .await
    .expect("caller should receive cancel acknowledgement");

    assert_eq!(
        cancel_response.payload().to_bytes(),
        cancel_response_payload
    );

    while let Ok(message) = goal_handle.feedback_mut().rx.try_recv() {
        assert_eq!(
            message.key_expr(),
            expected_feedback_topic,
            "feedback from unexpected topic while draining"
        );
    }

    let post_cancel_feedback = tokio::time::timeout(
        Duration::from_millis(200),
        goal_handle.feedback_mut().rx.recv(),
    )
    .await;

    if let Ok(Some(message)) = post_cancel_feedback {
        panic!(
            "expected no feedback after cancellation, received topic '{}' with payload {:?}",
            message.key_expr(),
            message.payload()
        );
    }

    let err = ActionMessenger::poll_result(
        &caller_handle,
        caller_instance_id,
        &goal_handle,
        result_request_payload,
        Duration::from_millis(200),
    )
    .await;

    let Err(err) = err else {
        panic!("action result should time out after cancellation");
    };

    let Error::ActionResultUnreachable {
        instance_id: err_instance_id,
        action_name: err_action_name,
    } = &err
    else {
        panic!(
            "expected ActionResultTimeout or ActionResultUnreachable error after cancellation, received: {:?}",
            err
        );
    };

    assert_eq!(
        err_instance_id.as_deref(),
        Some(listener_instance_id),
        "should report unreachable targeted action instance"
    );
    assert_eq!(err_action_name, listener_action_name);

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_action_communication_multiple_polls() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_action_name = "enable_camera";
    let listener_instance_id = "listener_instance";

    // Caller instance
    let caller_prefix = "the_brain";

    const CLIENT_COUNT: usize = 8;
    let cases: Vec<_> = (0..CLIENT_COUNT)
        .map(|idx| ActionClientCase::new(caller_prefix, idx))
        .collect();
    let cases = Arc::new(cases);

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    // Launch a background task that plays the role of the action server.
    let server_task = {
        let action_handle = router.action_messenger().await;
        let action_ready_tx = Some(action_ready_tx);
        let cases = Arc::clone(&cases);

        tokio::spawn(async move {
            let action = ActionMessenger::listen(
                &action_handle,
                MASTER_NODE_NAME,
                listener_node_name,
                listener_action_name,
                listener_instance_id,
            )
            .await
            .expect("action should start");

            let action_root = super::build_master_key_expr(
                MASTER_NODE_NAME,
                "action",
                listener_node_name,
                listener_action_name,
            );
            let crate::messaging::ActionCreation {
                mut goal_service,
                cancel_service: _,
                feedback_publisher,
                mut result_service,
            } = action;
            let feedback_publisher = Arc::new(feedback_publisher);

            if let Some(tx) = action_ready_tx {
                let _ = tx.send(());
            }

            let client_total = cases.len();

            let mut goal_handlers = Vec::with_capacity(client_total);
            for _ in 0..client_total {
                let action_root = action_root.clone();
                let cases = Arc::clone(&cases);
                let feedback_publisher = Arc::clone(&feedback_publisher);

                let handler = goal_service
                .spawn_next_request_handler(move |request| {
                        let action_root = action_root.clone();
                        let cases = Arc::clone(&cases);
                        let feedback_publisher = Arc::clone(&feedback_publisher);

                        async move {
                            let caller_id = request.message().instance_id();
                            let expected_goal_topic = format!(
                                "{action_root}/goal/<INSTANCE_ID:{listener_instance_id}>/request/<INSTANCE_ID:{caller_id}>"
                            );
                            assert_eq!(request.message().key_expr(), expected_goal_topic);

                            let payload = request.message().payload();
                            let payload_bytes = payload.as_bytes();
                            let payload_str = std::str::from_utf8(payload_bytes.as_ref())
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
                                payload, &case.goal,
                                "goal payload for `{client_id}` should match expected value"
                            );

                            feedback_publisher
                                .publish_with_prefix(MASTER_NODE_NAME, case.feedback.clone())
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
                let action_root = action_root.clone();
                let cases = Arc::clone(&cases);

                let handler = result_service
                    .spawn_next_request_handler(move |request| {
                        let action_root = action_root.clone();
                        let cases = Arc::clone(&cases);

                        async move {
                            let caller_id = request.message().instance_id();
                            let expected_result_topic = format!(
                                "{action_root}/result/<INSTANCE_ID:{listener_instance_id}>/request/<INSTANCE_ID:{caller_id}>"
                            );
                            assert_eq!(request.message().key_expr(), expected_result_topic);

                            let payload = request.message().payload();
                            let payload_bytes = payload.as_bytes();
                            let payload_str = std::str::from_utf8(payload_bytes.as_ref())
                                .expect("result payload should be valid UTF-8");

                            let client_id = payload_str
                                .split(';')
                                .find_map(|part| part.strip_prefix("client="))
                                .expect("result payload should contain client identifier")
                                .to_string();

                            let case = cases.iter().find(|case| case.client_id == client_id).unwrap_or_else(|| {
                                panic!("result handler received unexpected client id `{client_id}`")
                            });

                            assert_eq!(
                                payload, &case.result_request,
                                "result request payload for `{client_id}` should match expected value"
                            );

                            Ok(case.result_response.clone())
                        }
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

    let expected_feedback_topic = format!(
        "{}/feedback/<INSTANCE_ID:{listener_instance_id}>",
        super::build_master_key_expr(
            MASTER_NODE_NAME,
            "action",
            listener_node_name,
            listener_action_name
        )
    );

    let total_clients = cases.len();
    let mut shuffled_cases = cases.as_ref().clone();
    let mut rng = rand::rng();
    shuffled_cases.shuffle(&mut rng);

    let mut client_handles = Vec::with_capacity(total_clients);
    for case in shuffled_cases {
        let host = host.clone();
        let expected_feedback_topic = expected_feedback_topic.clone();
        let feedback_search_limit = total_clients;

        let handle = tokio::spawn(async move {
            let caller_handle = connect_action_messenger(&host, port).await;

            let mut goal_handle = ActionMessenger::send_goal(
                &caller_handle,
                MASTER_NODE_NAME,
                &case.client_id,
                listener_node_name,
                listener_action_name,
                None,
                case.goal.clone(),
                QoSProfile::Reliable,
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should send goal");

            assert_eq!(
                goal_handle.goal_response().payload().to_bytes(),
                case.goal_response.clone(),
                "goal response should match expected payload for `{}`",
                case.client_id
            );

            let mut feedback_matched = false;
            for _ in 0..feedback_search_limit {
                let feedback_message = goal_handle
                    .feedback_mut()
                    .rx
                    .recv()
                    .await
                    .expect("caller should receive feedback message");

                assert_eq!(
                    feedback_message.key_expr(),
                    expected_feedback_topic,
                    "feedback should be published on the expected topic"
                );

                if feedback_message.payload() == &case.feedback {
                    feedback_matched = true;
                    break;
                }
            }

            assert!(
                feedback_matched,
                "caller `{}` should observe its corresponding feedback payload",
                case.client_id
            );

            let result_response = ActionMessenger::poll_result(
                &caller_handle,
                &case.client_id,
                &goal_handle,
                case.result_request.clone(),
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should receive result response");

            assert_eq!(
                result_response.payload().to_bytes(),
                case.result_response.clone(),
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
