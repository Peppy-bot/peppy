use std::fs;

use super::net::pick_free_tcp_port;
use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};
use tempfile::TempDir;

async fn try_start_zenohd_instance(
    host: &str,
    port: u16,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let temp_dir = TempDir::new().expect("Failed to create temp dir");
    let zenohd_config_path = temp_dir.path().join("test_zenoh_config.json5");

    let config_content = format!(
        r#"{{
              "listen": {{
                "endpoints": {{
                  "router": ["tcp/{host}:{port}"]
                }}
              }}
            }}"#
    );

    fs::write(&zenohd_config_path, config_content).expect("Failed to write zenoh router config");
    let adapter = ZenohAdapter::from_zenohd_config(Some(&zenohd_config_path))
        .expect("Failed to create zenoh adapter from config");
    let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
    messenger.start_router().await?;
    Ok((messenger, temp_dir, String::from(host), port))
}

/// If the port given is `None`, the Zenoh process will try to start on consts::DEFAULT_ZENOH_PORT
pub async fn start_zenohd_process(
    port: Option<u16>,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let host = "127.0.0.1";
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
