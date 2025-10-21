use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{Messenger, MessengerAdapter, MessengerBackend, ZenohAdapter};
use std::{
    fs,
    net::TcpListener,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;

use crate::error::Error;
use crate::messaging::PeppyMessenger;

fn pick_free_tcp_port() -> Option<u16> {
    (0..10).find_map(|_| {
        TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|sock| {
            let port = sock.local_addr().ok()?.port();
            drop(sock);
            Some(port)
        })
    })
}

/// Helper function start a zenoh router before each test (done by peppyd in the real world)
async fn start_zenohd_process() -> (Messenger, TempDir, String, u16) {
    let host = "127.0.0.1";
    let port = pick_free_tcp_port().expect("Failed to pick a free TCP port");

    let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
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
    messenger
        .start_router()
        .await
        .expect("Failed to start router");
    (messenger, temp_dir, String::from(host), port)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe() {
    let (mut router_messenger, _, host, port) = start_zenohd_process().await;

    // Those attributes are found in the message definition `exposes`
    let sender_node = "uvc_camera";
    let receiver_node = "vision_pipeline";
    let topic_name = "video_frame";
    let qos = QoSProfile::SensorData;

    // Those properties are found in the `deployments` array
    let ns = "/camera/rear";

    let payload = Bytes::from_static(b"A message");

    let sender_messenger = PeppyMessenger::from_host_port(&sender_node, &host, port).await;
    let receiver_messenger = PeppyMessenger::from_host_port(&receiver_node, &host, port).await;

    let mut subscription = receiver_messenger
        .receive_topic_msg(&sender_node, ns, topic_name)
        .await
        .expect("Should subscribe to the topic");

    sender_messenger
        .emit_topic_message(ns, topic_name, qos, payload.clone())
        .await
        .expect("Should send the payload");

    let received = subscription
        .rx
        .recv()
        .await
        .expect("Should receive the published message");

    let expected_topic = PeppyMessenger::build_full_namespace(&sender_node, ns, topic_name);
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

    let service_messenger = PeppyMessenger::from_host_port(service_node, &host, port).await;
    let caller_messenger = PeppyMessenger::from_host_port(caller_node, &host, port).await;

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");

    let call_count = Arc::new(AtomicUsize::new(0));
    let service = service_messenger
        .expose_service(namespace, service_name)
        .await
        .expect("service should start");

    let service_handle = {
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

    assert_eq!(response, response_payload);

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

    let service_messenger = PeppyMessenger::from_host_port(service_node, &host, port).await;
    let caller_messenger = PeppyMessenger::from_host_port(caller_node, &host, port).await;

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    let concurrent_requests = 25;
    let request_payloads: Vec<Bytes> = (0..concurrent_requests)
        .map(|i| Bytes::from(format!("enable=true;request={i}").into_bytes()))
        .collect();
    let call_count = Arc::new(AtomicUsize::new(0));

    let service = service_messenger
        .expose_service(namespace, service_name)
        .await
        .expect("service should start");

    let service_handle: tokio::task::JoinHandle<Result<(), Error>> = {
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

    let caller_messenger = Arc::new(caller_messenger);

    let mut handles = Vec::with_capacity(concurrent_requests);
    for request_payload in request_payloads.iter().cloned() {
        let caller_messenger = Arc::clone(&caller_messenger);
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
        let (request_payload, response) = handle.await.expect("poll_service task should not panic");
        assert_eq!(
            response, request_payload,
            "response should match the originating request payload"
        );
    }

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

    let caller_messenger = PeppyMessenger::from_host_port(caller_node, &host, port).await;

    let err = caller_messenger
        .poll_service(
            service_node,
            namespace,
            service_name,
            Bytes::from_static(b"enable=true"),
            Duration::from_secs(1),
        )
        .await
        .expect_err("service call should fail when service is not started");

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

    let service_messenger = PeppyMessenger::from_host_port(service_node, &host, port).await;
    let caller_messenger = PeppyMessenger::from_host_port(caller_node, &host, port).await;

    let service_root = PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
    let expected_request_topic = format!("{service_root}/request");
    let request_payload = Bytes::from_static(b"enable=true");
    let response_payload = Bytes::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let response_delay = Duration::from_millis(200);
    let caller_success_timeout = Duration::from_millis(500);
    let caller_timeout = Duration::from_millis(50);

    let service = service_messenger
        .expose_service(namespace, service_name)
        .await
        .expect("service should start");

    let service_handle = {
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

    let success_response = caller_messenger
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

    let err = caller_messenger
        .poll_service(
            service_node,
            namespace,
            service_name,
            request_payload,
            caller_timeout,
        )
        .await
        .expect_err("service call should fail when response exceeds timeout");

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
async fn action_communication() {}
