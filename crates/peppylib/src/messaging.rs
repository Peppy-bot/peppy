use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError,
    PublisherQoS, SubscriberQoS, Subscription, ZenohAdapter, ZenohNetProtocol,
};
use sha2::{Digest, Sha256};
use std::{
    future::Future,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use tokio::{sync::Mutex, task::JoinHandle};
use tracing::error;

/// This struct represent one deployment instance messaging
pub struct PeppyMessenger {
    node_name: String,
    messenger: Arc<Mutex<Messenger>>,
}

impl PeppyMessenger {
    pub async fn new(node_name: &str) -> Self {
        let adapter = ZenohAdapter::default();
        Self {
            node_name: String::from(node_name),
            messenger: Arc::new(Mutex::new(PeppyMessenger::new_session(adapter).await)),
        }
    }

    pub async fn from_host_port(node_name: &str, host: &str, port: u16) -> Self {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
        Self {
            node_name: String::from(node_name),
            messenger: Arc::new(Mutex::new(PeppyMessenger::new_session(adapter).await)),
        }
    }

    async fn new_session(adapter: ZenohAdapter) -> Messenger {
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        messenger
    }

    fn map_node_qos_to_publisher_qos(qos: QoSProfile) -> PublisherQoS {
        match qos {
            QoSProfile::Standard => PublisherQoS::Standard,
            QoSProfile::Reliable => PublisherQoS::Important,
            QoSProfile::SensorData => PublisherQoS::BestEffort,
            QoSProfile::Critical => PublisherQoS::Critical,
        }
    }

    fn map_node_qos_to_subscriber_qos(qos: QoSProfile) -> SubscriberQoS {
        match qos {
            QoSProfile::SensorData => SubscriberQoS::HighThroughput,
            _ => SubscriberQoS::Standard,
        }
    }

    /// Generates a unique request ID using SHA256 hash of node name + timestamp + thread ID
    /// This ensures each service call has a unique correlation ID
    fn generate_request_id(node_name: &str) -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let thread_id = std::thread::current().id();

        let mut hasher = Sha256::new();
        hasher.update(node_name.as_bytes());
        hasher.update(timestamp.to_le_bytes());
        hasher.update(format!("{:?}", thread_id).as_bytes());

        let result = hasher.finalize();
        format!("{:x}", result)[..16].to_string() // Use first 16 hex chars for compactness
    }

    pub fn build_full_namespace(
        node_name: &str,
        namespace: &str,
        message_type_name: &str,
    ) -> String {
        [node_name, namespace, message_type_name]
            .into_iter()
            .flat_map(|part| part.split('/'))
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub async fn receive_topic_msg(
        &self,
        from_node_name: &str,
        namespace: &str,
        topic_name: &str,
    ) -> Result<Subscription> {
        let qos = QoSProfile::Reliable; // Always the same QoSProfile for services
        let topic_path = Self::build_full_namespace(from_node_name, namespace, topic_name);
        let subscriber_qos = Self::map_node_qos_to_subscriber_qos(qos);

        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger.subscribe(&topic_path, subscriber_qos).await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        Ok(subscription)
    }

    pub async fn emit_topic_message(
        &self,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let full_ns = Self::build_full_namespace(&self.node_name, namespace, topic_name);
        let msg = Message::new(&full_ns, &payload);

        let publisher_qos = PeppyMessenger::map_node_qos_to_publisher_qos(qos);

        let mut messenger = self.messenger.lock().await;
        messenger
            .publish(msg, publisher_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub async fn start_service<F, Fut>(
        &self,
        namespace: &str,
        service_name: &str,
        handler: F,
    ) -> Result<JoinHandle<Result<()>>>
    where
        F: Fn(Message) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Bytes>> + Send + 'static,
    {
        let service_root = Self::build_full_namespace(&self.node_name, namespace, service_name);
        let request_topic_base = format!("{service_root}/request");
        let request_subscription_topic = format!("{request_topic_base}/**");
        let response_topic_base = format!("{service_root}/response");

        let subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .subscribe(&request_subscription_topic, SubscriberQoS::Standard)
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        let messenger = Arc::clone(&self.messenger);
        let handler = Arc::new(handler);

        let task: JoinHandle<Result<()>> = tokio::spawn(async move {
            let mut subscription = subscription;
            let request_topic_prefix = format!("{request_topic_base}/");

            while let Some(request) = subscription.rx.recv().await {
                let Message { topic, payload } = request;

                let request_id = topic
                    .strip_prefix(&request_topic_prefix)
                    .and_then(|rest| rest.split('/').next())
                    .filter(|segment| !segment.is_empty())
                    .map(|segment| segment.to_string());

                let handler_request = Message {
                    topic: request_topic_base.clone(),
                    payload,
                };

                let response_topic = request_id
                    .as_ref()
                    .map(|id| format!("{response_topic_base}/{id}"))
                    .unwrap_or_else(|| response_topic_base.clone());

                match handler(handler_request).await {
                    Ok(payload) => {
                        let message = Message::new(&response_topic, payload.as_ref());
                        let mut messenger = messenger.lock().await;
                        messenger
                            .publish(message, PublisherQoS::Standard)
                            .await
                            .map_err(Error::PeppyMessagingInterface)?;
                    }
                    Err(err) => {
                        error!(?err, "service handler returned error");
                    }
                }
            }
            Ok(())
        });

        Ok(task)
    }

    pub async fn poll_service(
        &self,
        from_node_name: &str,
        namespace: &str,
        service_name: &str,
        request_payload: Bytes,
    ) -> Result<Bytes> {
        let service_root =
            PeppyMessenger::build_full_namespace(from_node_name, namespace, service_name);

        let request_id = Self::generate_request_id(&self.node_name);

        let request_topic = format!("{service_root}/request/{request_id}");
        let response_topic = format!("{service_root}/response/{request_id}");

        let mut response_subscription = {
            let messenger = self.messenger.lock().await;
            messenger
                .subscribe(&response_topic, SubscriberQoS::Standard)
                .await
        }
        .map_err(Error::PeppyMessagingInterface)?;

        {
            let mut messenger = self.messenger.lock().await;
            messenger
                .publish(
                    Message::new(&request_topic, request_payload.as_ref()),
                    PublisherQoS::Standard,
                )
                .await
                .map_err(Error::PeppyMessagingInterface)?;
        }

        let response = response_subscription.rx.recv().await.ok_or_else(|| {
            Error::PeppyMessagingInterface(PeppyMessagingInterfaceError::BackendError(
                "service response channel closed".to_string(),
            ))
        })?;

        Ok(response.payload)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use config::node::QoSProfile;
    use pmi::{Message, Messenger, MessengerAdapter, MessengerBackend, ZenohAdapter};
    use std::{
        fs,
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use tempfile::TempDir;

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

        fs::write(&zenohd_config_path, config_content)
            .expect("Failed to write zenoh router config");
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
        let path = super::PeppyMessenger::build_full_namespace(
            "uvc_camera",
            "/camera/rear/",
            "/video_frame",
        );
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

        let service_root =
            PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
        let expected_request_topic = format!("{service_root}/request");
        let request_payload = Bytes::from_static(b"enable=true");
        let response_payload = Bytes::from_static(b"ack");

        let call_count = Arc::new(AtomicUsize::new(0));
        let service_handle = service_messenger
            .start_service(namespace, service_name, {
                let expected_request_topic = expected_request_topic.clone();
                let request_payload = request_payload.clone();
                let response_payload = response_payload.clone();
                let call_count = call_count.clone();

                move |message: Message| {
                    assert_eq!(message.topic, expected_request_topic);
                    assert_eq!(message.payload, request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    let response_payload = response_payload.clone();

                    async move { Ok(response_payload) }
                }
            })
            .await
            .expect("service should start");

        let response = caller_messenger
            .poll_service(
                service_node,
                namespace,
                service_name,
                request_payload.clone(),
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

        service_handle.abort();

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

        let service_root =
            PeppyMessenger::build_full_namespace(service_node, namespace, service_name);
        let expected_request_topic = format!("{service_root}/request");
        let concurrent_requests = 25;
        let request_payloads: Vec<Bytes> = (0..concurrent_requests)
            .map(|i| Bytes::from(format!("enable=true;request={i}").into_bytes()))
            .collect();
        let call_count = Arc::new(AtomicUsize::new(0));

        let service_handle = service_messenger
            .start_service(namespace, service_name, {
                let expected_request_topic = expected_request_topic.clone();
                let call_count = Arc::clone(&call_count);
                move |message: Message| {
                    assert_eq!(message.topic, expected_request_topic);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    let response_payload = message.payload.clone();

                    async move {
                        tokio::task::yield_now().await;
                        Ok(response_payload)
                    }
                }
            })
            .await
            .expect("service should start");

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

        let expected_count = call_count.load(Ordering::SeqCst);
        assert_eq!(
            expected_count, concurrent_requests,
            "service should have been called {concurrent_requests} times"
        );

        service_handle.abort();

        router_messenger
            .stop_router()
            .await
            .expect("Failed to shutdown router");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn service_communication_fails() {
        // TODO: Fails if the service is not yet started with a ServiceUnreachable error
    }
}
