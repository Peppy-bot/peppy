use std::{
    fs,
    net::TcpListener,
    path::{Path, PathBuf},
};

use tempfile::TempDir;

use crate::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};

fn map_io_error(error: std::io::Error) -> PeppyMessagingInterfaceError {
    PeppyMessagingInterfaceError::BackendError(error.to_string())
}

/// A reservation for a free TCP port. The port remains reserved (bound) until this
/// struct is dropped or [`PortReservation::release`] is called.
///
/// Use this to minimize TOCTOU race conditions when multiple tests run in parallel:
/// 1. Call [`reserve_free_tcp_port`] to get a reservation
/// 2. Use [`PortReservation::port`] to get the port number for configuration
/// 3. Drop the reservation (or call [`PortReservation::release`]) right before the
///    service binds to the port
pub struct PortReservation {
    port: u16,
    _listener: TcpListener,
}

impl PortReservation {
    /// Returns the reserved port number.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// Releases the port reservation and returns the port number.
    /// Call this right before the actual service binds to the port.
    pub fn release(self) -> u16 {
        self.port
    }
}

/// Reserves a free TCP port by binding to it. The port remains reserved until
/// the returned [`PortReservation`] is dropped.
///
/// This is useful when you need to minimize the window between port selection
/// and actual binding by another service (e.g., zenoh).
pub fn reserve_free_tcp_port() -> PortReservation {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("failed to bind ephemeral TCP port");
    let port = listener
        .local_addr()
        .expect("listener has local addr")
        .port();
    PortReservation {
        port,
        _listener: listener,
    }
}

/// Returns a free TCP port by asking the OS for an ephemeral port.
///
/// Note: This has a small TOCTOU window since the port is released immediately.
/// For parallel test scenarios, prefer [`reserve_free_tcp_port`] which keeps the
/// port bound until you're ready to use it.
pub fn pick_free_tcp_port() -> u16 {
    reserve_free_tcp_port().release()
}

/// Writes a zenohd configuration file bound to `host:port`, keeping the temporary directory alive.
pub fn write_zenohd_config(
    host: &str,
    port: u16,
) -> Result<(TempDir, PathBuf), PeppyMessagingInterfaceError> {
    let temp_dir = TempDir::new().map_err(map_io_error)?;
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

    fs::write(&config_path, config_content).map_err(map_io_error)?;
    Ok((temp_dir, config_path))
}

fn messenger_from_config(config_path: &Path) -> Result<Messenger, PeppyMessagingInterfaceError> {
    let adapter = ZenohAdapter::from_zenohd_config(config_path)?;
    let (_, messaging_port) = adapter.client_endpoint();
    Ok(Messenger::new(
        MessengerAdapter::Zenoh(adapter),
        messaging_port,
    ))
}

fn create_messenger_with_config(
    host: &str,
    port: u16,
) -> Result<(Messenger, TempDir, String, u16, PathBuf), PeppyMessagingInterfaceError> {
    let (temp_dir, config_path) = write_zenohd_config(host, port)?;
    let messenger = messenger_from_config(&config_path)?;
    Ok((messenger, temp_dir, host.to_string(), port, config_path))
}

/// Creates a messenger configured for a zenohd router bound to `host`.
///
/// The router is **not** started. The returned configuration path can be modified before starting
/// the router via `Messenger::start_router`.
pub fn prepare_zenohd_test_router(
    host: &str,
    port: Option<u16>,
) -> Result<(Messenger, TempDir, PathBuf, String, u16), PeppyMessagingInterfaceError> {
    let port = port.unwrap_or_else(pick_free_tcp_port);
    let (messenger, temp_dir, host, port, config_path) = create_messenger_with_config(host, port)?;
    Ok((messenger, temp_dir, config_path, host, port))
}

async fn try_start_zenohd_instance(
    host: &str,
    port: u16,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let (mut messenger, temp_dir, host, port, _) = create_messenger_with_config(host, port)?;
    messenger.start_router().await?;
    Ok((messenger, temp_dir, host, port))
}

/// Starts a `zenohd` router bound to the provided `host` and `port`.
///
/// When `port` is `None`, a free TCP port will be selected automatically from the test range and
/// the function will retry multiple times to avoid bind conflicts.
pub async fn start_zenohd_process(
    host: &str,
    port: Option<u16>,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let max_start_attempts = match port {
        Some(_) => 1,
        None => 32,
    };

    for attempt in 0..max_start_attempts {
        let port = match port {
            Some(port) => port,
            None => pick_free_tcp_port(),
        };
        match try_start_zenohd_instance(host, port).await {
            Ok(result) => return Ok(result),
            Err(err) if attempt + 1 < max_start_attempts => {
                if !matches!(err, PeppyMessagingInterfaceError::BackendError(_)) {
                    return Err(err);
                }
                // Retry with a new port when the backend signals a binding failure.
            }
            Err(err) => return Err(err),
        }
    }

    Err(PeppyMessagingInterfaceError::BackendError(format!(
        "Failed to start zenoh router after {max_start_attempts} attempts"
    )))
}
