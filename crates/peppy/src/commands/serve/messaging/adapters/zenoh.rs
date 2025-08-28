use super::super::{Message, MessengerBackend, Subscription};
use crate::{Error, Result, zenohd};
use std::{collections::HashMap, path::PathBuf, sync::Arc};
use tracing::{debug, info};

pub struct ZenohAdapter {
    zenohd: zenohd::ZenohdFacade,
    session: Option<Arc<zenoh::Session>>,
    publishers: HashMap<String, Arc<zenoh::pubsub::Publisher<'static>>>,
}

impl ZenohAdapter {
    pub fn new(config: Option<PathBuf>) -> Result<Self> {
        let facade = zenohd::ZenohdFacade::new(config)?;
        let publishers = HashMap::new();

        Ok(Self {
            zenohd: facade,
            session: None,
            publishers,
        })
    }
}

impl MessengerBackend for ZenohAdapter {
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    async fn init(&mut self) -> Result<()> {
        self.zenohd.start_router()?;
        let session = zenoh::open(self.zenohd.config.clone())
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

    async fn subscribe(&self, _topic: &str) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump zenoh samples into tx
        Ok(Subscription { rx })
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
