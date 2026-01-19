use std::{fs, net::TcpListener, path::PathBuf};

use tempfile::TempDir;

use crate::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
    ZenohNetProtocol,
};

/// Reserves an ephemeral port by binding to port 0 and returning the assigned port.
/// The returned `TcpListener` holds the port until dropped.
pub fn reserve_ephemeral_port() -> std::io::Result<(u16, TcpListener)> {
    let listener = TcpListener::bind(("127.0.0.1", 0))?;
    let port = listener.local_addr()?.port();
    Ok((port, listener))
}

/// Result of starting a zenohd router process.
///
/// The router is automatically stopped when this instance is dropped.
pub struct ZenohdInstance {
    messenger: Option<Messenger>,
    pub host: String,
    pub port: u16,
}

impl ZenohdInstance {
    /// Returns a mutable reference to the messenger.
    pub fn messenger(&mut self) -> &mut Messenger {
        self.messenger
            .as_mut()
            .expect("messenger was already taken")
    }

    /// Takes ownership of the messenger, preventing automatic cleanup on drop.
    pub fn take_messenger(&mut self) -> Messenger {
        self.messenger.take().expect("messenger was already taken")
    }
}

impl Drop for ZenohdInstance {
    fn drop(&mut self) {
        let Some(mut messenger) = self.messenger.take() else {
            return;
        };
        let _ = std::thread::spawn(move || {
            if let Ok(rt) = tokio::runtime::Runtime::new() {
                let _ = rt.block_on(async move { messenger.stop_router().await });
            }
        })
        .join();
    }
}

/// Writes a zenohd configuration file. When `port` is `None`, reserves an ephemeral port
/// and holds it until the config is written, eliminating TOCTOU race conditions.
pub fn write_zenohd_config(
    host: &str,
    port: Option<u16>,
) -> Result<(TempDir, PathBuf, u16), PeppyMessagingInterfaceError> {
    // Reserve port if not specified - the listener holds it until we finish writing
    let (port, _reservation) = match port {
        Some(p) => (p, None),
        None => {
            let listener =
                TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind ephemeral TCP port");
            let port = listener
                .local_addr()
                .expect("listener has local addr")
                .port();
            (port, Some(listener))
        }
    };

    let temp_dir =
        TempDir::new().map_err(|e| PeppyMessagingInterfaceError::BackendError(e.to_string()))?;
    let config_path = temp_dir.path().join("test_zenoh_config.json5");
    let config_content = format!(
        r#"{{
              "listen": {{
                "endpoints": {{
                  "router": ["tcp/{host}:{port}"]
                }}
              }}
            }}"#
    );
    fs::write(&config_path, config_content)
        .map_err(|e| PeppyMessagingInterfaceError::BackendError(e.to_string()))?;

    // _reservation dropped here, releasing the port for zenohd to bind
    Ok((temp_dir, config_path, port))
}

/// Starts a `zenohd` router. When `port` is `None`, retries with new ports on bind failures.
pub async fn start_zenohd_process(
    host: &str,
    port: Option<u16>,
) -> Result<ZenohdInstance, PeppyMessagingInterfaceError> {
    let max_attempts = if port.is_some() { 1 } else { 32 };

    for attempt in 0..max_attempts {
        let (port, _reservation) = match port {
            Some(p) => (p, None),
            None => {
                let (p, listener) = reserve_ephemeral_port()
                    .map_err(|e| PeppyMessagingInterfaceError::BackendError(e.to_string()))?;
                (p, Some(listener))
            }
        };

        let adapter = ZenohAdapter::with_endpoint(ZenohNetProtocol::Tcp, host, port)?;
        let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));

        // Drop the port reservation before starting the router so zenohd can bind to it
        drop(_reservation);

        match messenger.start_router().await {
            Ok(()) => {
                return Ok(ZenohdInstance {
                    messenger: Some(messenger),
                    host: host.to_string(),
                    port,
                });
            }
            Err(PeppyMessagingInterfaceError::BackendError(_)) if attempt + 1 < max_attempts => {
                continue;
            }
            Err(err) => return Err(err),
        }
    }

    Err(PeppyMessagingInterfaceError::BackendError(format!(
        "Failed to start zenoh router after {max_attempts} attempts"
    )))
}
