use super::super::{Message, MessengerBackend, Subscription};
use crate::{Error, Result};
use std::process::{Child, Command, Stdio};
use zenoh::Session;
use zenoh::config::Config;

pub struct ZenohAdapter {
    // hold zenoh session, pubs, subs, etc.
    host: String,
    port: u16,
    session: Option<Session>,
    router_process: Option<Child>,
}

impl ZenohAdapter {
    pub fn new(host: String, port: u16) -> Self {
        Self {
            host,
            port,
            session: None,
            router_process: None,
        }
    }
}

impl Default for ZenohAdapter {
    fn default() -> Self {
        Self {
            host: "0.0.0.0".into(),
            port: 7447,
            session: None,
            router_process: None,
        }
    }
}

impl MessengerBackend for ZenohAdapter {
    /// Starts a zenohd process, using std::process::Command is the recommended way as using the
    /// rust crate directly prevents the user from using plugins/adminspace
    async fn start_router(&mut self) -> Result<()> {
        let listen_endpoint_str = format!("tcp/{}:{}", self.host, self.port);

        // Start zenohd as a separate process
        let mut child = Command::new("zenohd")
            .arg("--listen")
            .arg(&listen_endpoint_str)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::BackendError(format!("Failed to start zenohd: {}", e)))?;

        // Give zenohd a moment to start up
        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        // Check if the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                return Err(Error::BackendError(format!(
                    "zenohd exited unexpectedly with status: {}",
                    status
                )));
            }
            Ok(None) => {
                // Process is still running, which is what we want
                tracing::info!("Zenoh router started, listening on {}", listen_endpoint_str);
            }
            Err(e) => {
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

    async fn connect(&mut self) -> Result<()> {
        let connect_endpoint = format!("tcp/{}:{}", self.host, self.port);

        let mut config = Config::default();

        // Set mode to client using insert_json5
        config
            .insert_json5("mode", "\"client\"")
            .map_err(|e| Error::BackendError(format!("Failed to set client mode: {}", e)))?;

        // Set connect endpoints
        config
            .insert_json5("connect/endpoints", &format!("[\"{}\"]", connect_endpoint))
            .map_err(|e| Error::BackendError(format!("Failed to set connect endpoint: {}", e)))?;

        let session = zenoh::open(config).await.map_err(|e| {
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

    async fn shutdown(&mut self) -> Result<()> {
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
