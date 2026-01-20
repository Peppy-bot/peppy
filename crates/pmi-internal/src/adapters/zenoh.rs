use crate::error::{Error, Result};
use crate::types::{PublisherQoS, SubscriberQoS, TopicMessage};
use crate::zenohd::{self, ZenohNetProtocol};
use crate::{Message, MessengerBackend, Subscription};
use askama::Template;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::{collections::HashMap, sync::Arc};
use tracing::info;

use zenoh::qos::{CongestionControl, Priority};
use zenoh::sample::SampleFields;

#[derive(Template)]
#[template(path = "zenoh/default_client_config.json5.j2")]
pub struct ZenohClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: zenohd::ZenohNetProtocol,
}

#[derive(Template)]
#[template(path = "zenoh/default_router_config.json5.j2")]
pub struct ZenohRouterConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: ZenohNetProtocol,
}

pub struct ZenohClientConfig {
    zenoh_config: zenoh::config::Config,
    host: String,
    port: u16,
    protocol: ZenohNetProtocol,
}

pub struct ZenohAdapter {
    zenohd: Option<zenohd::ZenohdFacade>,
    client_config: ZenohClientConfig,
    session: Option<Arc<zenoh::Session>>,
    publishers: HashMap<String, Arc<zenoh::pubsub::Publisher<'static>>>,
}

impl ZenohAdapter {
    pub fn with_endpoint(protocol: ZenohNetProtocol, host: &str, port: u16) -> Result<Self> {
        let zenohd_config_path = Self::get_zenohd_config_path(protocol, host, port)?;
        let facade = zenohd::ZenohdFacade::new(zenohd_config_path)?;
        // Create a client config that connects to the router
        // Extract the endpoint from the router's listen config
        let client_config = ZenohAdapter::derive_client_config_from_zenohd(&facade);

        Ok(Self {
            zenohd: Some(facade),
            client_config,
            session: None,
            publishers: HashMap::new(),
        })
    }

    pub fn client_endpoint(&self) -> (&str, u16) {
        (self.client_config.host.as_str(), self.client_config.port)
    }

    fn derive_client_config_from_zenohd(zenohd: &zenohd::ZenohdFacade) -> ZenohClientConfig {
        // Use the same config from the router to infer the client connection
        let connect_host = if zenohd.zenoh_endpoint.host == "0.0.0.0" {
            "127.0.0.1".to_string()
        } else {
            zenohd.zenoh_endpoint.host.clone()
        };

        let client_template = ZenohClientConfigTemplate {
            host: connect_host,
            port: zenohd.zenoh_endpoint.port,
            protocol: zenohd.zenoh_endpoint.protocol,
        };

        let client_config_str = client_template
            .render()
            .expect("Failed to render client config template");

        let client_config = zenoh::config::Config::from_json5(&client_config_str)
            .expect("Failed to create client config");

        ZenohClientConfig {
            zenoh_config: client_config,
            host: client_template.host,
            port: client_template.port,
            protocol: client_template.protocol,
        }
    }

    fn get_zenohd_config_path(
        protocol: ZenohNetProtocol,
        host: &str,
        messaging_port: u16,
    ) -> Result<PathBuf> {
        if let Ok(config_path) = std::env::var("ZENOH_CONFIG") {
            return Ok(PathBuf::from(config_path));
        }

        let config_path =
            std::env::temp_dir().join(format!("zenohd_config_{}.json5", messaging_port));

        let template = ZenohRouterConfigTemplate {
            host: host.to_string(),
            port: messaging_port,
            protocol: protocol,
        };

        let config_content = template.render().map_err(|e| {
            Error::ConfigurationError(format!("Failed to render zenohd config template: {}", e))
        })?;

        std::fs::write(&config_path, config_content).map_err(|e| {
            Error::ConfigurationError(format!("Failed to write zenohd config: {}", e))
        })?;

        Ok(config_path)
    }
}

impl MessengerBackend for ZenohAdapter {
    async fn start_session(&mut self) -> Result<()> {
        let session = zenoh::open(self.client_config.zenoh_config.clone())
            .await
            .map_err(|e| Error::BackendError(format!("Failed to create Zenoh session: {}", e)))?;

        info!(
            "Zenoh session started on: {}://{}:{}",
            &self.client_config.protocol, &self.client_config.host, &self.client_config.port
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
        let identifier = message.identifier().to_string();
        let publisher = if let Some(pub_ref) = self.publishers.get(identifier.as_str()) {
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
                    .declare_publisher(identifier.clone())
                    .congestion_control(congestion_control)
                    .priority(priority)
                    .express(express)
                    .await
                    .map_err(|e| {
                        Error::PublisherCreationError(format!(
                            "Failed to create publisher for topic '{}': {}",
                            identifier, e
                        ))
                    })?,
            );

            #[cfg(debug_assertions)]
            let key_expr = identifier.clone();
            new_publisher
                .matching_listener()
                .callback(move |_matching_status| {
                    #[cfg(debug_assertions)]
                    {
                        if _matching_status.matching() {
                            info!("Publisher '{}' has matching subscribers", key_expr);
                        } else {
                            info!("Publisher '{}' has no more matching subscribers", key_expr);
                        }
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
                .insert(identifier.clone(), Arc::clone(&new_publisher));
            new_publisher
        };

        // Publish the message payload
        publisher
            .put(message.payload().as_ref())
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
                        let SampleFields {
                            key_expr, payload, ..
                        } = sample.into();

                        let key_expr = key_expr.as_str();
                        // Create a ResponseMessage object with topic and payload
                        match TopicMessage::from_zbytes(key_expr, payload) {
                            Ok(message) => {
                                if let Err(e) = tx.send(message).await {
                                    tracing::error!("Failed to send message: {}", e);
                                    break;
                                }
                            }
                            Err(err) => {
                                tracing::error!(
                                    %key_expr,
                                    %err,
                                    "Failed to build ResponseMessage from sample"
                                );
                            }
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

    async fn has_matching_subscribers(&self, topic: &str) -> Result<bool> {
        let session = self
            .session
            .as_ref()
            .ok_or_else(|| Error::MessagingSessionError("Session not initialized".to_string()))?;

        if let Some(existing) = self.publishers.get(topic) {
            let matching = existing
                .matching_status()
                .await
                .map_err(|e| {
                    Error::MatchingListenerError(format!(
                        "Failed to retrieve matching status for topic '{}': {}",
                        topic, e
                    ))
                })?
                .matching();
            return Ok(matching);
        }

        let publisher = session.declare_publisher(topic).await.map_err(|e| {
            Error::PublisherCreationError(format!(
                "Failed to create publisher for topic '{}': {}",
                topic, e
            ))
        })?;

        let matching = publisher
            .matching_status()
            .await
            .map_err(|e| {
                Error::MatchingListenerError(format!(
                    "Failed to retrieve matching status for topic '{}': {}",
                    topic, e
                ))
            })?
            .matching();

        publisher.undeclare().await.map_err(|e| {
            Error::MatchingListenerError(format!(
                "Failed to undeclare publisher for topic '{}': {}",
                topic, e
            ))
        })?;

        Ok(matching)
    }

    async fn start_router(&mut self) -> Result<()> {
        let zenohd = self
            .zenohd
            .as_mut()
            .ok_or(Error::ZenohDConfigurationNotFound)?;
        zenohd.start_router()?;
        Ok(())
    }

    async fn stop_router(&mut self) -> Result<()> {
        let zenohd = self
            .zenohd
            .as_mut()
            .ok_or(Error::ZenohDConfigurationNotFound)?;
        zenohd.stop_router()?;
        Ok(())
    }

    fn get_host(&self) -> SocketAddr {
        let host = &self.client_config.host;
        let port = self.client_config.port;
        // Parse host as IP address; use localhost as fallback for empty/invalid hosts
        let ip = host
            .parse()
            .unwrap_or_else(|_| std::net::IpAddr::V4(std::net::Ipv4Addr::LOCALHOST));
        SocketAddr::new(ip, port)
    }
}
