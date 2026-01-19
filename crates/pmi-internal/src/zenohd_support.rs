use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};

/// Result of starting a zenohd router process.
///
/// The router is automatically stopped when this instance is dropped.
pub struct ZenohdInstance {
    messenger: Option<Messenger>,
    temp_dir: Option<TempDir>,
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

    /// Returns a reference to the temp directory.
    pub fn temp_dir(&self) -> &TempDir {
        self.temp_dir.as_ref().expect("temp_dir was already taken")
    }

    /// Takes ownership of the temp directory.
    pub fn take_temp_dir(&mut self) -> TempDir {
        self.temp_dir.take().expect("temp_dir was already taken")
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

/// Creates a messenger from an existing zenohd config file.
pub fn messenger_from_config(
    config_path: &Path,
) -> Result<Messenger, PeppyMessagingInterfaceError> {
    let adapter = ZenohAdapter::from_zenohd_config(config_path)?;
    let (_, messaging_port) = adapter.client_endpoint();
    Ok(Messenger::new(
        MessengerAdapter::Zenoh(adapter),
        messaging_port,
    ))
}

/// Starts a `zenohd` router. When `port` is `None`, retries with new ports on bind failures.
pub async fn start_zenohd_process(
    host: &str,
    port: Option<u16>,
) -> Result<ZenohdInstance, PeppyMessagingInterfaceError> {
    let max_attempts = if port.is_some() { 1 } else { 32 };

    for attempt in 0..max_attempts {
        let (temp_dir, config_path, port) = write_zenohd_config(host, port)?;
        let mut messenger = messenger_from_config(&config_path)?;

        match messenger.start_router().await {
            Ok(()) => {
                return Ok(ZenohdInstance {
                    messenger: Some(messenger),
                    temp_dir: Some(temp_dir),
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
