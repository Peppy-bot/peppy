use crate::error::{Error, Result};
use crate::messaging_types::{PublisherQoS, SubscriberQoS};
use crate::zenohd;
use crate::{Message, MessengerBackend, Subscription};
use askama::Template;
use std::path::PathBuf;
use std::{collections::HashMap, sync::Arc};
use tracing::{debug, info};
use zenoh::qos::{CongestionControl, Priority};

#[derive(Template)]
#[template(path = "zenoh/default_client_config.json5.j2")]
pub struct ZenohClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: zenohd::ZenohNetProtocol,
}

pub struct ZenohAdapter {
    zenohd: zenohd::ZenohdFacade,
    session: Option<Arc<zenoh::Session>>,
    publishers: HashMap<String, Arc<zenoh::pubsub::Publisher<'static>>>,
}

impl ZenohAdapter {
    pub fn new(zenohd_config_path: Option<PathBuf>) -> Result<Self> {
        let facade = zenohd::ZenohdFacade::new(zenohd_config_path)?;
        let publishers = HashMap::new();

        Ok(Self {
            zenohd: facade,
            session: None,
            publishers,
        })
    }

    fn create_client_config(&self) -> zenoh::config::Config {
        // Use the same config from the router but for client connection
        let connect_host = if self.zenohd.zenoh_endpoint.host == "0.0.0.0" {
            "127.0.0.1".to_string()
        } else {
            self.zenohd.zenoh_endpoint.host.clone()
        };

        let client_template = ZenohClientConfigTemplate {
            host: connect_host,
            port: self.zenohd.zenoh_endpoint.port,
            protocol: self.zenohd.zenoh_endpoint.protocol,
        };

        let client_config_str = client_template
            .render()
            .expect("Failed to render client config template");

        zenoh::config::Config::from_json5(&client_config_str)
            .expect("Failed to create client config")
    }
}

impl MessengerBackend for ZenohAdapter {
    async fn start_session(&mut self) -> Result<()> {
        // Create a client config that connects to the router
        // Extract the endpoint from the router's listen config
        let client_config = self.create_client_config();
        let session = zenoh::open(client_config)
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        info!(
            "Zenoh session started on: {}:{}",
            &self.zenohd.zenoh_endpoint.host, self.zenohd.zenoh_endpoint.port
        );
        self.session = Some(Arc::new(session));
        Ok(())
    }

    async fn stop_session(mut self) -> Result<()> {
        // Close the Zenoh session if it exists
        if let Some(session) = self.session.take() {
            drop(session);
        }
        Ok(())
    }

    async fn publish(&mut self, message: Message, qos: PublisherQoS) -> Result<()> {
        let publisher = if let Some(pub_ref) = self.publishers.get(&message.topic) {
            Arc::clone(pub_ref)
        } else {
            let session = self.session.as_ref().ok_or_else(|| {
                Error::MessagingSessionError("Session not initialized".to_string())
            })?;

            // Map QoS to Zenoh settings
            let (priority, congestion_control, express) = match qos {
                PublisherQoS::BestEffort => (Priority::DataLow, CongestionControl::Drop, true),
                PublisherQoS::Standard => (Priority::Data, CongestionControl::Drop, false),
                PublisherQoS::Important => (Priority::DataHigh, CongestionControl::Block, false),
                PublisherQoS::Critical => (Priority::RealTime, CongestionControl::Block, true),
            };

            let new_publisher = Arc::new(
                session
                    .declare_publisher(message.topic.clone())
                    .congestion_control(congestion_control)
                    .priority(priority)
                    .express(express)
                    .await
                    .map_err(|e| {
                        Error::PublisherCreationError(format!(
                            "Failed to create publisher for topic '{}': {}",
                            message.topic, e
                        ))
                    })?,
            );

            new_publisher
                .matching_listener()
                .callback(|matching_status| {
                    if matching_status.matching() {
                        info!("Publisher has matching subscribers");
                    } else {
                        debug!("Publisher has no more matching subscribers");
                    }
                })
                .background()
                .await
                .map_err(|e| {
                    Error::MatchingListenerError(format!(
                        "Failed to register matching listener: {}",
                        e
                    ))
                })?;

            self.publishers
                .insert(message.topic.clone(), Arc::clone(&new_publisher));
            new_publisher
        };

        // Publish the message payload
        publisher
            .put(message.payload)
            .await
            .map_err(|e| Error::PublishError {
                topic: e.to_string(),
            })?;

        Ok(())
    }

    async fn subscribe(&self, topic: &str, qos: SubscriberQoS) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (tx, rx) = tokio::sync::mpsc::channel(qos.channel_size());

        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;

        let subscriber = session
            .declare_subscriber(topic)
            .await
            .map_err(|e| Error::MessagingSessionError(e.to_string()))?;

        // Spawn background task to forward messages with abort handle
        let join_handle = tokio::spawn(async move {
            loop {
                match subscriber.recv_async().await {
                    Ok(sample) => {
                        // Get the raw bytes from the sample
                        let payload_bytes = sample.payload().to_bytes();

                        // Create a Message object with topic and payload as bytes::Bytes
                        let message = Message {
                            topic: sample.key_expr().as_str().to_string(),
                            payload: bytes::Bytes::from(payload_bytes.into_owned()),
                        };

                        // Send the Message on the tx channel
                        if let Err(e) = tx.send(message).await {
                            tracing::error!("Failed to send message: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("Subscriber stopped receiving messages: {}", e);
                        break;
                    }
                }
            }
        });

        // Get the abort handle from the join handle
        let abort_handle = join_handle.abort_handle();

        Ok(Subscription::new(rx, abort_handle))
    }

    async fn start_router(&mut self) -> Result<()> {
        self.zenohd.start_router()?;
        Ok(())
    }

    async fn stop_router(&mut self) -> Result<()> {
        self.zenohd.stop_router()?;
        Ok(())
    }
}
