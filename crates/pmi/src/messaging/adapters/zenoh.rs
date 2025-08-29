use super::super::super::error::{Error, Result};
use super::super::super::zenohd;
use super::super::{Message, MessengerBackend, Subscription, ThroughputMode};
use askama::Template;
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tracing::{debug, info};

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
    pub fn new(router_config: Option<PathBuf>) -> Result<Self> {
        let facade = zenohd::ZenohdFacade::new(router_config)?;
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
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    async fn init(&mut self) -> Result<()> {
        info!("Starting zenohd router...");
        self.zenohd.start_router()?;

        // Create a client config that connects to the router
        // Extract the endpoint from the router's listen config
        let client_config = self.create_client_config();
        info!(
            "Connecting to router at: {}",
            &self.zenohd.zenoh_endpoint.host
        );

        let session = zenoh::open(client_config)
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        self.session = Some(Arc::new(session));
        Ok(())
    }

    async fn publish(&mut self, message: Message) -> Result<()> {
        let publisher = if let Some(pub_ref) = self.publishers.get(&message.topic) {
            Arc::clone(pub_ref)
        } else {
            let session = self.session.as_ref().ok_or_else(|| {
                Error::MessagingSessionError("Session not initialized".to_string())
            })?;

            let new_publisher = Arc::new(
                session
                    .declare_publisher(message.topic.clone())
                    .await
                    .map_err(|e| {
                        Error::PublisherCreationError(format!(
                            "Failed to create publisher for topic '{}': {}",
                            message.topic, e
                        ))
                    })?,
            );

            // Register matching listener only once when creating the publisher
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

    async fn subscribe(
        &self,
        topic: &str,
        throughput_mode: ThroughputMode,
    ) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (tx, rx) = tokio::sync::mpsc::channel(throughput_mode.channel_size());

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

    async fn shutdown(&mut self) -> Result<()> {
        self.zenohd.stop_router()?;
        // Close the Zenoh session if it exists
        if let Some(session) = self.session.take() {
            drop(session);
        }
        Ok(())
    }
}
