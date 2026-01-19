use pmi::{zenohd_support::start_zenohd_process, MessengerBackend};
use tokio::signal;

#[tokio::main]
async fn main() {
    let host = "0.0.0.0";
    let port = config::consts::DEFAULT_MESSAGING_PORT;

    println!("Starting zenohd router on tcp/{host}:{port}…");
    let mut instance = match start_zenohd_process(host, Some(port)).await {
        Ok(instance) => instance,
        Err(error) => {
            panic!("failed to start zenohd router on tcp/{host}:{port}: {error:?}");
        }
    };

    let config_path = instance.temp_dir.path().join("test_zenoh_config.json5");
    println!(
        "zenohd router ready on tcp/{}/{}. Using config at {}. Press Ctrl+C to stop.",
        instance.host,
        instance.port,
        config_path.display()
    );

    if let Err(error) = signal::ctrl_c().await {
        eprintln!("Failed to listen for Ctrl+C signal: {error}");
    }

    println!("Stopping zenohd router…");
    if let Err(error) = instance.messenger().stop_router().await {
        eprintln!("Failed to stop zenohd router cleanly: {error:?}");
    } else {
        println!("zenohd router stopped.");
    }
}
