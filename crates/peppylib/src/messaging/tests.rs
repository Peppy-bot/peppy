use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};
use rand::{seq::SliceRandom, thread_rng};
use std::collections::HashMap;
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;
use tokio::{sync::oneshot, time::sleep};

use crate::error::Error;
use crate::messaging::PeppyMessenger;

const PORT_START: u16 = 30_000;
const PORT_END: u16 = 60_000;
static NEXT_PORT: AtomicU32 = AtomicU32::new(PORT_START as u32);

fn allocate_candidate_port() -> u16 {
    loop {
        let current = NEXT_PORT.load(Ordering::Relaxed);
        let candidate = if current >= PORT_END as u32 {
            PORT_START as u32
        } else {
            current
        };
        let next = candidate + 1;
        if NEXT_PORT
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return candidate as u16;
        }
    }
}

fn pick_free_tcp_port() -> Option<u16> {
    Some(allocate_candidate_port())
}

async fn try_start_zenohd_instance(
    host: &str,
    port: u16,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

    let config_content = format!(
        r#"{{
              "listen": {{
                "endpoints": {{
                  "router": ["tcp/{host}:{port}"]
                }}
              }}
            }}"#
    );

    fs::write(&zenohd_config_path, config_content).expect("Failed to write zenoh router config");
    let adapter = ZenohAdapter::from_zenohd_config(Some(&zenohd_config_path))
        .expect("Failed to create zenoh adapter from config");
    let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
    messenger.start_router().await?;
    Ok((messenger, temp_dir, String::from(host), port))
}

/// Helper function start a zenoh router before each test (done by peppyd in the real world)
/// We'd rather start a real local zenoh router than use a mocked instance that can blow up in the real world
async fn start_zenohd_process() -> (Messenger, TempDir, String, u16) {
    const MAX_START_ATTEMPTS: usize = 32;
    let host = "127.0.0.1";

    for attempt in 0..MAX_START_ATTEMPTS {
        let port = pick_free_tcp_port().expect("Failed to allocate TCP port");
        match try_start_zenohd_instance(host, port).await {
            Ok(result) => return result,
            Err(err) if attempt + 1 < MAX_START_ATTEMPTS => {
                if !matches!(err, PeppyMessagingInterfaceError::BackendError(_)) {
                    panic!("Failed to start zenoh router: {:?}", err);
                }
                // Retry with a new port when the backend signals a binding failure.
            }
            Err(err) => panic!(
                "Failed to start zenoh router after {MAX_START_ATTEMPTS} attempts: {:?}",
                err
            ),
        }
    }

    unreachable!("zenoh router start retry loop exhausted unexpectedly")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    // Those attributes are found in the message definition `exposes`
    let sender_node_name = "uvc_camera";
    let receiver_node = "vision_pipeline";
    let topic_name = "video_frame";
    let qos = QoSProfile::SensorData;

    // Those properties are found in the `deployments` array
    let ns = "/camera/rear";

    let payload = Bytes::from_static(b"A message");

    let sender_node = PeppyMessenger::from_host_port(&sender_node_name, &host, port)
        .await
        .expect("failed to create sender messenger");
    let receiver_node = PeppyMessenger::from_host_port(&receiver_node, &host, port)
        .await
        .expect("failed to create receiver messenger");

    let mut subscription = receiver_node
        .receive_topic_msg(&sender_node_name, ns, topic_name, qos.clone())
        .await
        .expect("Should subscribe to the topic");

    sender_node
        .emit_topic_message(ns, topic_name, qos, payload.clone())
        .await
        .expect("Should send the payload");

    let received = subscription
        .rx
        .recv()
        .await
        .expect("Should receive the published message");

    let expected_topic = PeppyMessenger::build_full_namespace(&sender_node_name, ns, topic_name);
    assert_eq!(received.topic, expected_topic);
    assert_eq!(received.payload, payload);

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_5000hz_messages() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    // Those attributes are found in the message definition `exposes`
    let sender_node_name = "uvc_camera";
    let receiver_node = "vision_pipeline";
    let topic_name = "video_frame";
    let qos = QoSProfile::SensorData;

    // Those properties are found in the `deployments` array
    let ns = "/camera/rear";

    let sender_node = PeppyMessenger::from_host_port(&sender_node_name, &host, port)
        .await
        .expect("failed to create sender messenger");
    let receiver_node = PeppyMessenger::from_host_port(&receiver_node, &host, port)
        .await
        .expect("failed to create receiver messenger");

    let mut subscription = receiver_node
        .receive_topic_msg(&sender_node_name, ns, topic_name, qos.clone())
        .await
        .expect("Should subscribe to the topic");

    let expected_topic = PeppyMessenger::build_full_namespace(&sender_node_name, ns, topic_name);
    let message_count = 5000;
    let mut message_ids: Vec<u32> = (0..message_count as u32).collect();
    message_ids.shuffle(&mut thread_rng());

    for &message_id in &message_ids {
        let payload = Bytes::from(message_id.to_le_bytes().to_vec());
        sender_node
            .emit_topic_message(ns, topic_name, qos.clone(), payload)
            .await
            .expect("Should send the payload");
        sleep(Duration::from_millis(1)).await;
    }

    for expected_id in &message_ids {
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.rx.recv())
            .await
            .expect("Timed out waiting for a message")
            .expect("Subscription closed before receiving all messages");

        assert_eq!(message.topic, expected_topic);

        let payload = message.payload.as_ref();
        assert_eq!(
            payload.len(),
            std::mem::size_of::<u32>(),
            "Payload should encode the message index"
        );

        let mut id_bytes = [0u8; std::mem::size_of::<u32>()];
        id_bytes.copy_from_slice(payload);
        let received_id = u32::from_le_bytes(id_bytes);

        assert_eq!(
            received_id, *expected_id,
            "Messages should arrive in the same order they were sent"
        );
    }

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_handle_request_processes_multiple_messages() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let service_name = "enable_camera";
    let namespace = "/camera";
    let expected_requests = 5;
    let call_count = Arc::new(AtomicUsize::new(0));
    let (shutdown_tx, shutdown_rx) = oneshot::channel();

    let service_handle = {
        let call_count = Arc::clone(&call_count);
        let host = host.clone();
        tokio::spawn(async move {
            let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port)
                .await
                .expect("failed to create service messenger");

            let mut service = service_expose_node
                .expose_service(namespace, service_name)
                .await
                .expect("service should start");

            tokio::select! {
                result = service.handle_requests(|request| {
                    let call_count = Arc::clone(&call_count);
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message.payload.clone())
                    }
                }) => result,
                _ = shutdown_rx => Ok(()),
            }
        })
    };

    {
        let caller_node = PeppyMessenger::from_host_port("vision_pipeline", &host, port)
            .await
            .expect("failed to create caller messenger");

        for i in 0..expected_requests {
            let request_payload = Bytes::from(format!("enable=true;request={i}").into_bytes());
            let response = caller_node
                .poll_service(
                    service_node,
                    namespace,
                    service_name,
                    request_payload.clone(),
                    Duration::from_secs(1),
                )
                .await
                .expect("caller should receive response");
            assert_eq!(
                response, request_payload,
                "response should match the originating request payload"
            );
        }
    }

    let _ = shutdown_tx.send(());

    let service_result = service_handle.await.expect("service task panicked");
    service_result.expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        expected_requests,
        "service should process all requests"
    );

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[test]
fn build_topic_path_removes_redundant_separators() {
    let path =
        super::PeppyMessenger::build_full_namespace("uvc_camera", "/camera/rear/", "/video_frame");
    assert_eq!(path, "uvc_camera/camera/rear/video_frame");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let caller_node = "vision_pipeline";
    let service_name = "enable_camera";
    let namespace = "/camera";

    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_handle = {
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port)
            .await
            .expect("failed to create service messenger");

        let service_root =
            PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
        let expected_request_topic = format!("{service_root}/request");

        let service = service_expose_node
            .expose_service(namespace, service_name)
            .await
            .expect("service should start");

        let expected_request_topic = expected_request_topic.clone();
        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let mut service = service;

            let handled = service
                .handle_next_request(|request| {
                    let response_payload = response_payload.clone();
                    async move {
                        assert_eq!(request.message.topic, expected_request_topic);
                        assert_eq!(request.message.payload, request_payload);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(response_payload)
                    }
                })
                .await
                .expect("service should receive exactly one request");

            assert!(
                handled,
                "service subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port)
            .await
            .expect("failed to create caller messenger");
        let response = caller_node
            .poll_service(
                service_node,
                namespace,
                service_name,
                request_payload.clone(),
                Duration::from_secs(1),
            )
            .await
            .expect("caller should receive response");

        assert_eq!(response, response_payload);
    }

    // Ensure the service callback was called exactly once
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "service callback should have been called exactly once"
    );

    service_handle
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

/// Ensures a unique request returns its unique response
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_service_communication_multiple_polls_and_callers() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let service_name = "enable_camera";
    let namespace = "/camera";

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    // TODO: That many callers saturate Zenohd
    let caller_count = 500;
    let requests_per_caller = 5;
    let total_requests = caller_count * requests_per_caller;
    let call_count = Arc::new(AtomicUsize::new(0));

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_handle: tokio::task::JoinHandle<Result<(), Error>> = {
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port)
            .await
            .expect("failed to create service messenger");

        let service = service_expose_node
            .expose_service(namespace, service_name)
            .await
            .expect("service should start");

        let expected_request_topic = expected_request_topic.clone();
        let call_count = Arc::clone(&call_count);
        let expected_requests = total_requests;

        tokio::spawn(async move {
            let mut service = service;

            for _ in 0..expected_requests {
                let expected_request_topic = expected_request_topic.clone();
                let handled = service
                    .handle_next_request(|request| {
                        let call_count = Arc::clone(&call_count);
                        async move {
                            assert_eq!(request.message.topic, expected_request_topic);
                            call_count.fetch_add(1, Ordering::SeqCst);
                            Ok(request.message.payload.clone())
                        }
                    })
                    .await
                    .expect("service should receive expected number of requests");

                assert!(
                    handled,
                    "service subscription closed before handling request"
                );
            }

            Ok(())
        })
    };

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

        let mut rng = thread_rng();
        caller_requests.shuffle(&mut rng);

        let mut handles = Vec::with_capacity(caller_count);
        for (caller_node_name, mut requests) in caller_requests {
            requests.shuffle(&mut rng);
            let host = host.clone();
            let poll_service = tokio::spawn(async move {
                let caller_messenger =
                    PeppyMessenger::from_host_port(&caller_node_name, &host, port)
                        .await
                        .expect("failed to create caller messenger");

                let mut caller_results = Vec::with_capacity(requests.len());
                for (request_idx, request_payload) in requests {
                    let response = caller_messenger
                        .poll_service(
                            service_node,
                            namespace,
                            service_name,
                            request_payload.clone(),
                            Duration::from_secs(1),
                        )
                        .await
                        .expect("caller should receive response");

                    caller_results.push((
                        caller_node_name.clone(),
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
        let mut rng = thread_rng();
        verification_indices.shuffle(&mut rng);
        if verification_indices == original_indices {
            verification_indices.rotate_left(1);
        }

        for index in verification_indices {
            let (caller_node_name, request_idx, request_payload, response) = &results[index];
            let expected_payload = expected_payloads
                .remove(&(caller_node_name.clone(), *request_idx))
                .expect("expected payload should exist for caller/request pair");

            assert_eq!(
                request_payload, &expected_payload,
                "stored request payload should match expected value for `{caller_node_name}` request {request_idx}"
            );
            assert_eq!(
                response, &expected_payload,
                "response for `{caller_node_name}` request {request_idx} should match the originating request payload"
            );
        }

        assert!(
            expected_payloads.is_empty(),
            "all expected caller/request pairs should have been validated"
        );
    };

    service_handle
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    let expected_count = call_count.load(Ordering::SeqCst);
    assert_eq!(
        expected_count, total_requests,
        "service should have been called {total_requests} times"
    );

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_not_started() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let caller_node = "vision_pipeline";
    let service_name = "enable_camera";
    let namespace = "/camera";

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port)
            .await
            .expect("failed to create caller messenger");

        caller_node
            .poll_service(
                service_node,
                namespace,
                service_name,
                Bytes::from_static(b"enable=true"),
                Duration::from_secs(1),
            )
            .await
            .expect_err("service call should fail when service is not started")
    };

    match err {
        Error::ServiceTimeout {
            service_node: err_service_node,
            namespace: err_namespace,
            service_name: err_service_name,
        } => {
            assert_eq!(err_service_node, service_node);
            assert_eq!(err_namespace, namespace);
            assert_eq!(err_service_name, service_name);
        }
        other => panic!(
            "expected ServiceTimeout error, received unexpected error: {:?}",
            other
        ),
    }

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_timeout() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let caller_node = "vision_pipeline";
    let service_name = "enable_camera";
    let namespace = "/camera";

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let response_delay = Duration::from_millis(200);
    let caller_success_timeout = Duration::from_millis(500);
    let caller_timeout = Duration::from_millis(50);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_handle = {
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port)
            .await
            .expect("failed to create service messenger");
        let service = service_expose_node
            .expose_service(namespace, service_name)
            .await
            .expect("service should start");

        let expected_request_topic = expected_request_topic.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);
        let response_delay = response_delay;
        let expected_requests = 2;

        tokio::spawn(async move {
            let mut service = service;

            for _ in 0..expected_requests {
                let expected_request_topic = expected_request_topic.clone();
                let response_payload = response_payload.clone();
                let call_count = Arc::clone(&call_count);
                let response_delay = response_delay;

                let handled = service
                    .handle_next_request(|request| async move {
                        assert_eq!(request.message.topic, expected_request_topic);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        tokio::time::sleep(response_delay).await;
                        Ok(response_payload)
                    })
                    .await
                    .expect("service should receive expected number of requests");

                assert!(
                    handled,
                    "service subscription closed before handling request"
                );
            }

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port)
            .await
            .expect("failed to create caller messenger");

        let success_response = caller_node
            .poll_service(
                service_node,
                namespace,
                service_name,
                request_payload.clone(),
                caller_success_timeout,
            )
            .await
            .expect("caller should receive response before timeout");
        assert_eq!(success_response, response_payload);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "service should have processed the successful request exactly once"
        );

        caller_node
            .poll_service(
                service_node,
                namespace,
                service_name,
                request_payload,
                caller_timeout,
            )
            .await
            .expect_err("service call should fail when response exceeds timeout")
    };

    match err {
        Error::ServiceTimeout {
            service_node: err_service_node,
            namespace: err_namespace,
            service_name: err_service_name,
        } => {
            assert_eq!(err_service_node, service_node);
            assert_eq!(err_namespace, namespace);
            assert_eq!(err_service_name, service_name);
        }
        other => panic!(
            "expected ServiceTimeout error for timeout, received: {:?}",
            other
        ),
    }

    service_handle
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "service should have processed both requests"
    );

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let action_node = "controller_node";
    let action_name = "move_right_arm";
    let namespace = "/control";
    let caller_node = "the_brain";

    let goal_payload = Bytes::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Bytes::from_static(b"accepted");
    let feedback_payload = Bytes::from_static(b"progress=50");
    let result_payload = Bytes::from_static(b"done");
    let result_request_payload = Bytes::from_static(b"goal=right_arm");

    let goal_payload_server = goal_payload.clone();
    let goal_response_payload_server = goal_response_payload.clone();
    let feedback_payload_server = feedback_payload.clone();
    let result_payload_server = result_payload.clone();
    let result_request_payload_server = result_request_payload.clone();

    // Launch a background task that plays the role of the action server.
    let server_task = {
        let action_messenger = PeppyMessenger::from_host_port(action_node, &host, port)
            .await
            .expect("failed to create action messenger");

        tokio::spawn(async move {
            let mut action = action_messenger
                .expose_action(namespace, action_name)
                .await
                .expect("action should start");

            let action_root =
                PeppyMessenger::build_full_namespace(action_node, namespace, action_name);
            let expected_goal_topic = format!("{action_root}/goal/request");
            let expected_result_topic = format!("{action_root}/result/request");

            // Wait for the client to send a goal request
            let handled_goal = action
                .goal_service
                .handle_next_request(move |request| {
                    let expected_goal_topic = expected_goal_topic.clone();
                    let expected_goal_payload = goal_payload_server.clone();
                    let expected_goal_response_payload = goal_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message.topic, expected_goal_topic);
                        assert_eq!(request.message.payload, expected_goal_payload);
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
                .publish(feedback_payload_server.clone())
                .await
                .expect("action should publish feedback");

            // Give the client time to attach to the result service before answering.
            sleep(Duration::from_millis(20)).await;

            let handled_result = action
                .result_service
                .handle_next_request(move |request| {
                    let expected_topic = expected_result_topic.clone();
                    let expected_payload = result_request_payload_server.clone();
                    let response_payload = result_payload_server.clone();
                    async move {
                        assert_eq!(request.message.topic, expected_topic);
                        assert_eq!(request.message.payload, expected_payload);
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

    let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port)
        .await
        .expect("failed to create caller messenger");

    // Client sends the goal and obtains the handle carrying goal response + feedback sub.
    let mut goal_handle = caller_node
        .send_action_goal(
            action_node,
            namespace,
            action_name,
            goal_payload,
            QoSProfile::Standard,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should send goal");

    assert_eq!(goal_handle.goal_response(), &goal_response_payload);

    let expected_feedback_topic = PeppyMessenger::build_full_namespace(
        action_node,
        namespace,
        &format!("{action_name}/feedback"),
    );

    // Consume one feedback update from the action server.
    let feedback_message = goal_handle
        .feedback_mut()
        .rx
        .recv()
        .await
        .expect("caller should receive feedback");

    assert_eq!(feedback_message.topic, expected_feedback_topic);
    assert_eq!(feedback_message.payload, feedback_payload);

    // Finally, request the result using the same handle and ensure the server replies.
    let result_response = caller_node
        .poll_action_result(
            &goal_handle,
            result_request_payload,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

    assert_eq!(result_response, result_payload);

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_action_communication_multiple_polls() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let action_node = "controller_node";
    let action_name = "move_right_arm";
    let namespace = "/control";
    let caller_node = "the_brain";

    let goal_payload = Bytes::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Bytes::from_static(b"accepted");
    let feedback_payload = Bytes::from_static(b"progress=50");
    let result_payload = Bytes::from_static(b"done");
    let result_request_payload = Bytes::from_static(b"goal=right_arm");

    let goal_payload_server = goal_payload.clone();
    let goal_response_payload_server = goal_response_payload.clone();
    let feedback_payload_server = feedback_payload.clone();
    let result_payload_server = result_payload.clone();
    let result_request_payload_server = result_request_payload.clone();

    // Launch a background task that plays the role of the action server.
    let server_task = {
        let action_messenger = PeppyMessenger::from_host_port(action_node, &host, port)
            .await
            .expect("failed to create action messenger");

        tokio::spawn(async move {
            let mut action = action_messenger
                .expose_action(namespace, action_name)
                .await
                .expect("action should start");

            let action_root =
                PeppyMessenger::build_full_namespace(action_node, namespace, action_name);
            let expected_goal_topic = format!("{action_root}/goal/request");
            let expected_result_topic = format!("{action_root}/result/request");

            // Wait for the client to send a goal request
            let handled_goal = action
                .goal_service
                .handle_next_request(move |request| {
                    let expected_goal_topic = expected_goal_topic.clone();
                    let expected_goal_payload = goal_payload_server.clone();
                    let expected_goal_response_payload = goal_response_payload_server.clone();
                    async move {
                        assert_eq!(request.message.topic, expected_goal_topic);
                        assert_eq!(request.message.payload, expected_goal_payload);
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
                .publish(feedback_payload_server.clone())
                .await
                .expect("action should publish feedback");

            // Give the client time to attach to the result service before answering.
            sleep(Duration::from_millis(20)).await;

            let handled_result = action
                .result_service
                .handle_next_request(move |request| {
                    let expected_topic = expected_result_topic.clone();
                    let expected_payload = result_request_payload_server.clone();
                    let response_payload = result_payload_server.clone();
                    async move {
                        assert_eq!(request.message.topic, expected_topic);
                        assert_eq!(request.message.payload, expected_payload);
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

    // TODO: Implement multiple clients that connect to the same action and check that the goal/feedback/result that each client get back maps to the original caller in such a way that the messages are not intertwined across the multiple callers

    router_messenger
        .stop_router()
        .await
        .expect("Failed to shutdown router");
}
