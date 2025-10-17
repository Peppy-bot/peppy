use crate::error::{Error, Result};
use bytes::Bytes;
use config::node::{MessageFormat, QoSProfile, SchemaType, TypeToken};
use pmi::{Message, Messenger, MessengerAdapter, MessengerBackend, PublisherQoS, ZenohAdapter};
use serde_json::{self, Value as JsonValue};

pub struct MessagePayload {
    message_format: MessageFormat,
    payload: Bytes,
}

impl MessagePayload {
    // TODO: Maybe the payload should be a protobuf format or something?
    // TODO: Seems like a function like `pub fn push_frame(encoding: String, header: PushFrameHeader, height: u32, image: [u8; 3], width: u32,)`
    // should be encoded by the generator to a ProtoBuf format or something and then passed as a payload here
    pub fn new(message_format: &str, payload: Bytes) -> Result<Self> {
        let message_format: MessageFormat = serde_json5::from_str(message_format)?;
        Ok(Self {
            message_format,
            payload: payload,
        })
    }

    /// Validates the payload based on the `message_format`. Do not use on every message as it would
    /// increase latency
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// This struct represent one deployment instance messaging
pub struct PeppyMessenger {
    messenger: Messenger,
}

impl PeppyMessenger {
    pub async fn new() -> Self {
        // I should also be able to pass in the host/port to the zenoh client config (for the moment it's only derived from zenohd in `derive_client_config`)
        let adapter = ZenohAdapter::default();
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_session()
            .await
            .expect("Failed to start session");

        Self { messenger }
    }

    fn map_node_qos_to_publisher_qos(qos: QoSProfile) -> PublisherQoS {
        match qos {
            QoSProfile::Standard => PublisherQoS::Standard,
            QoSProfile::Reliable => PublisherQoS::Important,
            QoSProfile::SensorData => PublisherQoS::BestEffort,
            QoSProfile::Critical => PublisherQoS::Critical,
        }
    }

    pub async fn send_topic_message(
        &mut self,
        node_name: &str,
        namespace: &str,
        topic_name: &str,
        qos: QoSProfile,
        payload: Bytes,
    ) -> Result<()> {
        let full_ns = format!("{}/{}/{}", node_name, namespace, topic_name);
        let msg = Message::new(&full_ns, &payload);

        let publisher_qos = PeppyMessenger::map_node_qos_to_publisher_qos(qos);

        self.messenger
            .publish(msg, publisher_qos)
            .await
            .map_err(Error::PeppyMessagingInterface)
    }
}

#[cfg(test)]
mod tests {
    use config::node::{MessageFormat, QoSProfile};
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, ZenohAdapter};
    use std::{fs, net::TcpListener};

    use crate::messaging::{MessagePayload, PeppyMessenger};

    fn pick_free_tcp_port() -> Option<u16> {
        (0..10).find_map(|_| {
            TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|sock| {
                let port = sock.local_addr().ok()?.port();
                // Drop socket to free port for messaging router
                drop(sock);
                Some(port)
            })
        })
    }

    /// Helper function start a zenoh router before each test (done by peppyd in the real world)
    async fn start_zenohd_process() {
        let port = pick_free_tcp_port().unwrap();

        let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
        let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

        let config_content = format!(
            r#"{{
                  "listen": {{
                  "endpoints": {{
                      "router": ["tcp/127.0.0.1:{port}"]
                  }}
                }}
            }}"#
        );

        fs::write(&zenohd_config_path, config_content).unwrap();
        let adapter = ZenohAdapter::from_zenohd_config(Some(&zenohd_config_path)).unwrap();
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
        messenger
            .start_router()
            .await
            .expect("Failed to start router");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
    async fn test_basic_publish_subscribe() {
        start_zenohd_process().await;

        let ns = "/camera/rear";
        let node_name = "uvc_camera";
        let topic_name = "video_frame";
        let qos = QoSProfile::SensorData;

        let message_format = r#"
            header: {
              type: "object",
              stamp: "time",
              frame_id: "u32",
            },
            encoding: "string", // "rgb8", "bgr8", "yuyv", "mjpeg"
            width: "u32",
            height: "u32",
            image: {
              type: "array",
              items: "u8",
              length: 3
            },
        "#;
        let payload = r#"
            header: {
              type: "object",
              stamp: "1760596713",
              frame_id: 1,
            },
            encoding: "yuyv",
            width: 1920,
            height: 1080,
            image: [231, 5, 23],
        "#;

        // let message_payload = MessagePayload::new(&message_format, &payload).unwrap();
        // message_payload.validate().unwrap();

        let kind_of_caller = r#"
        #[derive(Debug, Clone)]
        pub struct PushFrameHeader {
            pub frame_id: u32,
            pub stamp: std::time::SystemTime,
        }

        pub async fn push_frame_async(
            encoding: String,
            header: PushFrameHeader,
            height: u32,
            image: [u8; 3],
            width: u32,
        ) {
            let _ = (&encoding, &header, &height, &image, &width);
            todo!("publish peppylib topic asynchronously. Encode first with capn proto");
        }
        "#;

        let sender_messenger = PeppyMessenger::new().await;
        let receiver_messenger = PeppyMessenger::new().await;

        //sender_messenger.send_topic_message(node_name, ns, topic_name, qos, message_payload);

        // TODO: Use PeppyMessage
        // messenger
        //     .start_session()
        //     .await
        //     .expect("Failed to start session");

        // // Subscribe to a topic
        // let mut sub = receiver_messenger
        //     .subscribe("test/topic", SubscriberQoS::Standard)
        //     .await
        //     .expect("Failed to subscribe");

        // // Publish a message
        // let msg = Message::new("test/topic", b"Hello World");
        // messenger
        //     .publish(msg.clone(), PublisherQoS::Standard)
        //     .await
        //     .expect("Failed to publish");

        // // Verify subscriber receives the message
        // let received = sub.rx.recv().await.expect("Failed to receive message");
        // assert_eq!(received.topic, "test/topic");
        // assert_eq!(received.payload, msg.payload);

        // messenger.stop_router().await.expect("Failed to shutdown");
    }
}
