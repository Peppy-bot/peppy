use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::QoSProfile;
use pmi::{
    Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, SubscriberQoS,
    Subscription, ZenohAdapter, ZenohNetProtocol,
};

/// This struct represent one deployment instance messaging
pub struct PeppyMessenger {
    node_name: String,
    messenger: Messenger,
}

impl PeppyMessenger {
    pub async fn new(node_name: &str) -> Self {
        let adapter = ZenohAdapter::default();
        Self {
            node_name: String::from(node_name),
            messenger: PeppyMessenger::new_session(adapter).await,
        }
    }

    pub async fn from_host_port(node_name: &str, host: &str, port: u16) -> Self {
        let adapter = ZenohAdapter::from_host_port(ZenohNetProtocol::Tcp, host, port);
        Self {
            node_name: String::from(node_name),
            messenger: PeppyMessenger::new_session(adapter).await,
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

    pub fn build_topic_path(node_name: &str, namespace: &str, topic_name: &str) -> String {
        [node_name, namespace, topic_name]
            .into_iter()
            .flat_map(|part| part.split('/'))
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/")
    }

    pub async fn send_topic_message(
        &mut self,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let full_ns = Self::build_topic_path(&self.node_name, namespace, topic_name);
        let msg = Message::new(&full_ns, &payload);

        let publisher_qos = PeppyMessenger::map_node_qos_to_publisher_qos(qos);

        self.messenger
            .publish(msg, publisher_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }

    pub async fn receive_topic_msg(
        &self,
        from_node_name: &str,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
    ) -> Result<Subscription> {
        let topic_path = Self::build_topic_path(from_node_name, namespace, topic_name);
        let subscriber_qos = Self::map_node_qos_to_subscriber_qos(qos);

        self.messenger
            .subscribe(&topic_path, subscriber_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

#[cfg(test)]
mod tests {
    use bytes::Bytes;
    use config::node::QoSProfile;
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, ZenohAdapter};
    use std::{fs, net::TcpListener};
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
    async fn test_basic_publish_subscribe() {
        let (mut router_messenger, _, host, port) = start_zenohd_process().await;

        // Those attributes are found in the message definition `exposes`
        let sender_node = "uvc_camera";
        let receiver_node = "vision_pipeline";
        let topic_name = "video_frame";
        let qos = QoSProfile::SensorData;

        // Those properties are found in the `deployments` array
        let ns = "/camera/rear";

        let payload = Bytes::from_static(b"A message");

        let mut sender_messenger = PeppyMessenger::from_host_port(&sender_node, &host, port).await;
        let receiver_messenger = PeppyMessenger::from_host_port(&receiver_node, &host, port).await;

        let mut subscription = receiver_messenger
            .receive_topic_msg(&sender_node, ns, topic_name, qos.clone())
            .await
            .expect("Should subscribe to the topic");

        sender_messenger
            .send_topic_message(ns, topic_name, qos, payload.clone())
            .await
            .expect("Should send the payload");

        let received = subscription
            .rx
            .recv()
            .await
            .expect("Should receive the published message");

        let expected_topic = PeppyMessenger::build_topic_path(&sender_node, ns, topic_name);
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
            super::PeppyMessenger::build_topic_path("uvc_camera", "/camera/rear/", "/video_frame");
        assert_eq!(path, "uvc_camera/camera/rear/video_frame");
    }
}
