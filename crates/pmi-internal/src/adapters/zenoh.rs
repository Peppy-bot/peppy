use crate::error::{Error, Result};
use crate::types::{PublisherQoS, SubscriberQoS};
use crate::{Message, MessengerBackend, Subscription};
use crate::{ZenohNetProtocol, zenohd};
use askama::Template;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::{collections::HashMap, env, sync::Arc};
use tracing::{debug, info};
use zenoh::qos::{CongestionControl, Priority};

#[derive(Template)]
#[template(path = "zenoh/default_client_config.json5.j2")]
pub struct ZenohClientConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: zenohd::ZenohNetProtocol,
}

pub struct ZenohClientConfig {
    zenoh_config: zenoh::config::Config,
    host: String,
    port: u16,
    protocol: ZenohNetProtocol,
}

pub struct ZenohAdapter {
    zenohd: Option<zenohd::ZenohdFacade>,

    // TODO: client_config must turn into `session` once `start_session` is called, find the right design pattern
    client_config: ZenohClientConfig,
    session: Option<Arc<zenoh::Session>>,

    publishers: HashMap<String, Arc<zenoh::pubsub::Publisher<'static>>>,
}

impl ZenohAdapter {
    /// If `zenohd_config_path` is `None`, a default configuration file will be used or `ZENOHD_CONFIG` env var if set
    pub fn from_zenohd_config(zenohd_config_path: Option<impl AsRef<Path>>) -> Result<Self> {
        let facade = zenohd::ZenohdFacade::new(zenohd_config_path)?;
        // Create a client config that connects to the router
        // Extract the endpoint from the router's listen config
        let client_config = ZenohAdapter::derive_client_config_from_zenohd(&facade);

        Ok(Self {
            zenohd: Some(facade),
            client_config: client_config,
            session: None,
            publishers: HashMap::new(),
        })
    }

    pub fn from_client_config(client_config: impl AsRef<Path>) -> Self {
        let client_config = ZenohAdapter::derive_client_config_from_client_file(client_config);

        Self {
            zenohd: None,
            client_config,
            session: None,
            publishers: HashMap::new(),
        }
    }

    pub fn from_host_port(protocol: ZenohNetProtocol, host: &str, port: u16) -> Self {
        let host = host.to_string();
        let client_template = ZenohClientConfigTemplate {
            host: host.clone(),
            port,
            protocol,
        };

        let client_config_str = client_template
            .render()
            .expect("Failed to render client config template");

        let zenoh_config = zenoh::config::Config::from_json5(&client_config_str)
            .expect("Failed to create client config");

        let client_config = ZenohClientConfig {
            zenoh_config,
            host,
            port,
            protocol,
        };

        Self {
            zenohd: None,
            client_config,
            session: None,
            publishers: HashMap::new(),
        }
    }

    fn derive_client_config_from_client_file(client_config: impl AsRef<Path>) -> ZenohClientConfig {
        fn map_protocol(proto: &str) -> Option<ZenohNetProtocol> {
            match proto {
                "tcp" => Some(ZenohNetProtocol::Tcp),
                "udp" => Some(ZenohNetProtocol::Udp),
                "quic" => Some(ZenohNetProtocol::Quic),
                "ws" => Some(ZenohNetProtocol::Ws),
                _ => None,
            }
        }

        let config_path = client_config.as_ref();
        let (host, port, protocol) = std::fs::read_to_string(config_path)
            .ok()
            .and_then(|contents| serde_json5::from_str::<Value>(&contents).ok())
            .and_then(|value| {
                let endpoints = value.get("connect")?.get("endpoints")?;
                let endpoint_str = match endpoints {
                    Value::Array(items) => items.iter().find_map(|item| item.as_str()),
                    Value::String(s) => Some(s.as_str()),
                    _ => None,
                }?;
                let mut parts = endpoint_str.splitn(2, '/');
                let protocol = map_protocol(parts.next()?)?;
                let address = parts.next()?;
                let (host_part, port_part) = address.rsplit_once(':')?;
                let port = port_part.parse::<u16>().ok()?;
                Some((
                    host_part.trim_matches(['[', ']']).to_string(),
                    port,
                    protocol,
                ))
            })
            .unwrap_or_else(|| (String::new(), 0, ZenohNetProtocol::default()));
        let zenoh_config =
            zenoh::config::Config::from_file(config_path).expect("Failed to create client config");

        ZenohClientConfig {
            zenoh_config,
            host,
            port,
            protocol,
        }
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
}

impl Default for ZenohAdapter {
    /// Uses the default pubsub config or the one defined in `ZENOH_CONFIG`
    fn default() -> Self {
        if let Ok(config_path) = env::var("ZENOH_CONFIG") {
            let config_path = PathBuf::from(config_path);
            if config_path.exists() {
                return ZenohAdapter::from_client_config(config_path);
            }
        }

        ZenohAdapter::from_host_port(ZenohNetProtocol::default(), "127.0.0.1", 7447)
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
        let zenohd = self
            .zenohd
            .as_mut()
            .ok_or(Error::RouterConfigurationNotFound)?;
        zenohd.start_router()?;
        Ok(())
    }

    async fn stop_router(&mut self) -> Result<()> {
        let zenohd = self
            .zenohd
            .as_mut()
            .ok_or(Error::RouterConfigurationNotFound)?;
        zenohd.stop_router()?;
        Ok(())
    }
}
