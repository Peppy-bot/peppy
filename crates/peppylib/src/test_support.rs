use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};
use std::{
    fs,
    sync::atomic::{AtomicU32, Ordering},
};
use tempfile::TempDir;

const PORT_START: u16 = 40_000;
const PORT_END: u16 = 65_000;
static NEXT_PORT: AtomicU32 = AtomicU32::new(PORT_START as u32);

fn allocate_candidate_port() -> u16 {
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

fn pick_free_tcp_port() -> Option<u16> {
    Some(allocate_candidate_port())
}

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

/// Starts a zenoh router backed by `zenohd` for integration tests.
///
/// The router listens on a randomly allocated TCP port bound to 127.0.0.1.
/// A temporary directory containing the generated zenoh configuration is kept alive as
/// part of the returned tuple to ensure the file remains available for the router process.
pub async fn start_zenohd_process()
-> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    const MAX_START_ATTEMPTS: usize = 32;
    let host = "127.0.0.1";

    for attempt in 0..MAX_START_ATTEMPTS {
        let port = pick_free_tcp_port().expect("Failed to allocate TCP port");
        match try_start_zenohd_instance(host, port).await {
            Ok(result) => return Ok(result),
            Err(err) if attempt + 1 < MAX_START_ATTEMPTS => {
                if !matches!(err, PeppyMessagingInterfaceError::BackendError(_)) {
                    return Err(err);
                }
                // Retry with a new port when the backend signals a binding failure.
            }
            Err(err) => return Err(err),
        }
    }

    Err(PeppyMessagingInterfaceError::BackendError(
        format!("Failed to start zenoh router after {MAX_START_ATTEMPTS} attempts").into(),
    ))
}
