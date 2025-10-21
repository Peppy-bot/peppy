use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::time::Duration;
use tempfile::TempDir;

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

    let sender_node = PeppyMessenger::from_host_port(&sender_node_name, &host, port).await;
    let receiver_node = PeppyMessenger::from_host_port(&receiver_node, &host, port).await;

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
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port).await;

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

            let request = service
                .next_request()
                .await
                .expect("service should receive exactly one request");
            assert_eq!(request.message.topic, expected_request_topic);
            assert_eq!(request.message.payload, request_payload);
            call_count.fetch_add(1, Ordering::SeqCst);
            request
                .respond(response_payload)
                .await
                .expect("service should send response");

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port).await;
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

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_service_communication_multiple_polls() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    let service_node = "uvc_camera";
    let caller_node = "vision_pipeline";
    let service_name = "enable_camera";
    let namespace = "/camera";

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    let concurrent_requests = 25;
    let request_payloads: Vec<Bytes> = (0..concurrent_requests)
        .map(|i| Bytes::from(format!("enable=true;request={i}").into_bytes()))
        .collect();
    let call_count = Arc::new(AtomicUsize::new(0));

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_handle: tokio::task::JoinHandle<Result<(), Error>> = {
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port).await;

        let service = service_expose_node
            .expose_service(namespace, service_name)
            .await
            .expect("service should start");

        let expected_request_topic = expected_request_topic.clone();
        let call_count = Arc::clone(&call_count);
        let expected_requests = concurrent_requests;

        tokio::spawn(async move {
            let mut service = service;

            for _ in 0..expected_requests {
                let request = service
                    .next_request()
                    .await
                    .expect("service should receive expected number of requests");
                assert_eq!(request.message.topic, expected_request_topic);
                call_count.fetch_add(1, Ordering::SeqCst);
                let response_payload = request.message.payload.clone();

                request
                    .respond(response_payload)
                    .await
                    .expect("service should send response");
            }

            Ok(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_node = Arc::new(PeppyMessenger::from_host_port(caller_node, &host, port).await);
        let mut handles = Vec::with_capacity(concurrent_requests);
        for request_payload in request_payloads.iter().cloned() {
            let caller_messenger = Arc::clone(&caller_node);
            let poll_service = tokio::spawn(async move {
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
                (request_payload, response)
            });
            handles.push(poll_service);
        }

        for handle in handles {
            let (request_payload, response) =
                handle.await.expect("poll_service task should not panic");
            assert_eq!(
                response, request_payload,
                "response should match the originating request payload"
            );
        }
    };

    service_handle
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    let expected_count = call_count.load(Ordering::SeqCst);
    assert_eq!(
        expected_count, concurrent_requests,
        "service should have been called {concurrent_requests} times"
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
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port).await;

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
        let service_expose_node = PeppyMessenger::from_host_port(service_node, &host, port).await;
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
                let request = service
                    .next_request()
                    .await
                    .expect("service should receive expected number of requests");
                assert_eq!(request.message.topic, expected_request_topic);
                call_count.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(response_delay).await;
                request
                    .respond(response_payload.clone())
                    .await
                    .expect("service should send response");
            }

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_node = PeppyMessenger::from_host_port(caller_node, &host, port).await;

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
    // Here is an example of an action object:
    // {
    // // The brain sends signals to the controller
    //   name: "move_arm",
    //   goal_service: {
    //     // Always using "reliable" qos
    //     message_format: {
    //       arm_id: "u16",
    //       desired_position: {
    //         type: "array",
    //         items: "i32",
    //         length: 3
    //       }
    //     }
    //   },
    //   feedback_topic: {
    //     qos_profile: "standard", // Options: "standard", "reliable", "sensor_data"
    //     message_format: {
    //       current_position: {
    //         type: "array",
    //         items: "i32",
    //         length: 3
    //       }
    //     }
    //   },
    //   result_service: {
    //     // Always using "reliable" qos
    //     message_format: {
    //       final_position: {
    //         type: "array",
    //         items: "i32",
    //         length: 3
    //       }
    //     }
    //   }
    // }
}
