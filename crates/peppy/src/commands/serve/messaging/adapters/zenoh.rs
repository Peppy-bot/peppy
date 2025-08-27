use super::super::{Message, MessengerBackend, Subscription};
use crate::{Error, Result};
use askama::Template;
use std::fmt;
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use zenoh::Session;
use zenoh::config::Config;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum Protocol {
    Tcp,
    Udp,
    Quic,
    Ws,
}

impl fmt::Display for Protocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Protocol::Tcp => write!(f, "tcp"),
            Protocol::Udp => write!(f, "udp"),
            Protocol::Quic => write!(f, "quic"),
            Protocol::Ws => write!(f, "ws"),
        }
    }
}

impl Default for Protocol {
    fn default() -> Self {
        Protocol::Tcp
    }
}

#[derive(Template)]
#[template(path = "zenoh/default_config.json5.j2")]
pub struct ZenohConfigTemplate {
    pub host: String,
    pub port: u16,
    pub protocol: Protocol,
}

pub struct ZenohAdapter {
    config: zenoh::config::Config,
    session: Option<Session>,
    router_process: Option<Child>,
}

impl ZenohAdapter {
    pub fn new(config: Option<PathBuf>) -> Result<Self> {
        let config = match config {
            Some(config_path) => Config::from_file(config_path).map_err(|e| {
                Error::ConfigurationError(format!("Failed to load config file: {}", e))
            })?,
            None => Config::from_json5(&Self::render_default_config()).map_err(|e| {
                Error::ConfigurationError(format!("Failed to parse default config: {}", e))
            })?,
        };
        Ok(Self {
            config,
            session: None,
            router_process: None,
        })
    }

    /// Renders the Zenoh configuration from the template
    pub fn render_default_config() -> String {
        let template = ZenohConfigTemplate {
            host: "0.0.0.0".to_string(),
            port: 7447,
            protocol: Protocol::default(),
        };

        template
            .render()
            .unwrap_or_else(|e| panic!("Failed to render Zenoh config: {}", e))
    }
}

impl Default for ZenohAdapter {
    fn default() -> Self {
        Self {
            config: Config::from_json5(&ZenohAdapter::render_default_config())
                .expect("Default config should always be valid"),
            session: None,
            router_process: None,
        }
    }
}

impl MessengerBackend for ZenohAdapter {
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    fn start_router(&mut self) -> Result<()> {
        // Write config to a temporary file
        let temp_dir = std::env::temp_dir();
        let config_path = temp_dir.join("zenohd_config.json5");

        // Convert the config to JSON5 string
        let config_str = json5::to_string(&self.config)
            .map_err(|e| Error::BackendError(format!("Failed to serialize config: {}", e)))?;

        // Write config to file
        let mut file = fs::File::create(&config_path)
            .map_err(|e| Error::BackendError(format!("Failed to create config file: {}", e)))?;
        file.write_all(config_str.as_bytes())
            .map_err(|e| Error::BackendError(format!("Failed to write config file: {}", e)))?;

        // Extract listen endpoint from config for logging
        let config_value: serde_json::Value = json5::from_str(&config_str)
            .map_err(|e| Error::BackendError(format!("Failed to parse config: {}", e)))?;
        let listen_endpoint = config_value
            .get("listen")
            .and_then(|v| v.get("endpoints"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .unwrap();

        // Start zenohd as a separate process with the config file
        let mut child = Command::new("zenohd")
            .arg("-c")
            .arg(&config_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up
        std::thread::sleep(std::time::Duration::from_millis(500));

        // Check if the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                // Clean up config file on failure
                let _ = fs::remove_file(&config_path);
                return Err(Error::BackendError(format!(
                    "zenohd exited unexpectedly with status: {}",
                    status
                )));
            }
            Ok(None) => {
                // Process is still running, which is what we want
                tracing::info!("Zenoh router started, listening on {}", listen_endpoint);
            }
            Err(e) => {
                // Clean up config file on failure
                let _ = fs::remove_file(&config_path);
                return Err(Error::BackendError(format!(
                    "Failed to check zenohd status: {}",
                    e
                )));
            }
        }

        // Store the child process handle
        self.router_process = Some(child);

        Ok(())
    }

    fn connect(&mut self) -> Result<()> {
        // Extract the connect endpoint from the config
        // Default to the standard endpoint if not specified
        let config_str = json5::to_string(&self.config)
            .map_err(|e| Error::BackendError(format!("Failed to serialize config: {}", e)))?;
        let config_value: serde_json::Value = json5::from_str(&config_str)
            .map_err(|e| Error::BackendError(format!("Failed to parse config: {}", e)))?;
        let connect_endpoint = config_value
            .get("listen")
            .and_then(|v| v.get("endpoints"))
            .and_then(|v| v.as_array())
            .and_then(|arr| arr.first())
            .and_then(|v| v.as_str())
            .unwrap();

        let mut config = Config::default();

        // Set mode to client using insert_json5
        config
            .insert_json5("mode", "\"client\"")
            .map_err(|e| Error::BackendError(format!("Failed to set client mode: {}", e)))?;

        // Set connect endpoints
        config
            .insert_json5("connect/endpoints", &format!("[\"{}\"]", connect_endpoint))
            .map_err(|e| Error::BackendError(format!("Failed to set connect endpoint: {}", e)))?;

        // Use blocking zenoh API since trait requires sync
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|e| Error::BackendError(format!("Failed to create runtime: {}", e)))?;

        let session = runtime
            .block_on(async { zenoh::open(config).await })
            .map_err(|e| {
                Error::BackendError(format!("Failed to connect to Zenoh router: {}", e))
            })?;

        self.session = Some(session);
        tracing::info!("Connected to Zenoh router at {}", connect_endpoint);
        Ok(())
    }

    async fn publish(&self, _message: Message) -> Result<()> {
        // zenoh publish
        Ok(())
    }

    async fn subscribe(&self, _topic: &str) -> Result<Subscription> {
        // create zenoh subscriber, forward events into rx
        let (_tx, rx) = tokio::sync::mpsc::channel(128);
        // spawn task to pump zenoh samples into tx
        Ok(Subscription { rx })
    }

    fn shutdown(&mut self) -> Result<()> {
        // Close the Zenoh session if it exists
        if let Some(session) = self.session.take() {
            drop(session);
        }

        // Terminate the zenohd router process if it's running
        if self.router_process.is_some() {
            if let Some(mut child) = self.router_process.take() {
                // Try to kill the process gracefully
                match child.kill() {
                    Ok(()) => {
                        tracing::info!("Zenohd router process terminated");
                    }
                    Err(e) => {
                        tracing::warn!("Failed to terminate zenohd router process: {}", e);
                    }
                }

                // Wait for the process to actually exit
                let _ = child.wait();
            }
        }

        Ok(())
    }
}
