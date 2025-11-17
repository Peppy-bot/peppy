use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU32, Ordering},
};

use tempfile::TempDir;

use crate::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};

const PORT_START: u16 = 40_000;
const PORT_END: u16 = 65_000;
static NEXT_PORT: AtomicU32 = AtomicU32::new(PORT_START as u32);

fn map_io_error(error: std::io::Error) -> PeppyMessagingInterfaceError {
    PeppyMessagingInterfaceError::BackendError(error.to_string())
}

/// Returns a TCP port from the test range [40000, 65000).
pub fn pick_free_tcp_port() -> u16 {
    loop {
        let current = NEXT_PORT.load(Ordering::Relaxed);
        let candidate = if current >= PORT_END as u32 {
            PORT_START as u32
        } else {
            current
        };
        let next = candidate + 1;
        if NEXT_PORT
            .compare_exchange(current, next, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return candidate as u16;
        }
    }
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
    let adapter = ZenohAdapter::from_zenohd_config(Some(config_path))?;
    Ok(Messenger::new(MessengerAdapter::Zenoh(adapter)))
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

    Err(PeppyMessagingInterfaceError::BackendError(
        format!("Failed to start zenoh router after {max_start_attempts} attempts").into(),
    ))
}
