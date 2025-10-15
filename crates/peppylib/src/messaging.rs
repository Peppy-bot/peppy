// use pmi::MessagingEngineContext;
// use pmi::{Messenger, MessengerBackend};

// struct PeppyMessage {
//     messenger: Messenger,
// }

// impl PeppyMessage {
//     pub async fn new() -> Self {
//         // TODO: This is supposed to use a version of MessagingEngineContext that doesn't have a `start_router` function
//         // I should also be able to pass in the host/port to the zenoh client config (for the moment it's nly derived from zenohd in `derive_client_config`)
//         let context = MessagingEngineContext::zenoh(ZenohConfig::Router {
//             zenohd_config_path: None,
//             client_config: None,
//         });
//         let mut messenger = Messenger::new(context).unwrap();
//         messenger
//             .start_session()
//             .await
//             .expect("Failed to start session");
//         Self { messenger }
//     }
// }

// #[cfg(test)]
// mod tests {
//     use pmi::MessagingEngineContext;
//     use pmi::{Message, Messenger, MessengerBackend, PublisherQoS, SubscriberQoS};
//     use std::{fs, net::TcpListener};

//     fn pick_free_tcp_port() -> Option<u16> {
//         (0..10).find_map(|_| {
//             TcpListener::bind(("127.0.0.1", 0)).ok().and_then(|sock| {
//                 let port = sock.local_addr().ok()?.port();
//                 // Drop socket to free port for messaging router
//                 drop(sock);
//                 Some(port)
//             })
//         })
//     }

//     /// Helper function start a zenoh router before each test (done by peppyd in the real world)
//     async fn start_zenohd_process() {
//         let port = pick_free_tcp_port().unwrap();

//         let temp_dir = tempfile::TempDir::new().expect("Failed to create temp dir");
//         let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

//         let config_content = format!(
//             r#"{{
//                     "listen": {{
//                         "endpoints": {{
//                             "router": ["tcp/127.0.0.1:{port}"]
//                         }}
//                     }}
//                 }}"#
//         );

//         fs::write(&zenohd_config_path, config_content).unwrap();
//         let context = MessagingEngineContext::zenoh(ZenohConfig::Router {
//             zenohd_config_path: Some(zenohd_config_path.clone()),
//             client_config: None,
//         });
//         let mut messenger = Messenger::new(context).unwrap();
//         messenger
//             .start_router()
//             .await
//             .expect("Failed to start router");
//         messenger
//     }

//     #[tokio::test(flavor = "multi_thread", worker_threads = 1)]
//     async fn test_basic_publish_subscribe() {
//         let (mut messenger, _temp_dir) = create_test_messenger().await;

//         messenger
//             .start_session()
//             .await
//             .expect("Failed to start session");

//         // Subscribe to a topic
//         let mut sub = messenger
//             .subscribe("test/topic", SubscriberQoS::Standard)
//             .await
//             .expect("Failed to subscribe");

//         // Publish a message
//         let msg = Message::new("test/topic", b"Hello World");
//         messenger
//             .publish(msg.clone(), PublisherQoS::Standard)
//             .await
//             .expect("Failed to publish");

//         // Verify subscriber receives the message
//         let received = sub.rx.recv().await.expect("Failed to receive message");
//         assert_eq!(received.topic, "test/topic");
//         assert_eq!(received.payload, msg.payload);

//         messenger.stop_router().await.expect("Failed to shutdown");
//     }
// }
