use std::fs;

use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, PeppyMessagingInterfaceError, ZenohAdapter,
};
use tempfile::TempDir;
use tokio::signal;

pub const DEFAULT_ZENOH_PORT: u16 = 7448;

async fn try_start_zenohd_instance(
    host: &str,
    port: u16,
) -> Result<(Messenger, TempDir, String, u16), PeppyMessagingInterfaceError> {
    let temp_dir = TempDir::new()?;
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

    fs::write(&zenohd_config_path, config_content)?;
    let adapter = ZenohAdapter::from_zenohd_config(Some(&zenohd_config_path))?;
    let mut messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
    messenger.start_router().await?;
    Ok((messenger, temp_dir, String::from(host), port))
}

#[tokio::main]
async fn main() {
    let host = "127.0.0.1";
    let port = DEFAULT_ZENOH_PORT;

    println!("Starting zenohd router on tcp/{host}:{port}…");
    let (mut messenger, temp_dir, router_host, router_port) =
        match try_start_zenohd_instance(host, port).await {
            Ok(result) => result,
            Err(error) => {
                panic!("failed to start zenohd router on tcp/{host}:{port}: {error:?}");
            }
        };

    let config_path = temp_dir.path().join("test_zenoh_config.json5");
    println!(
        "zenohd router ready on tcp/{router_host}:{router_port}. Using config at {}. Press Ctrl+C to stop.",
        config_path.display()
    );

    if let Err(error) = signal::ctrl_c().await {
        eprintln!("Failed to listen for Ctrl+C signal: {error}");
    }

    println!("Stopping zenohd router…");
    if let Err(error) = messenger.stop_router().await {
        eprintln!("Failed to stop zenohd router cleanly: {error:?}");
    } else {
        println!("zenohd router stopped.");
    }
}
