use config::consts::DEFAULT_MESSAGING_PORT;
use pmi::{MessengerBackend, ZenohAdapter};
use tokio::signal;

#[tokio::main]
async fn main() {
    let host = "0.0.0.0";
    let port = DEFAULT_MESSAGING_PORT;

    println!("Starting zenohd router on tcp/{host}:{port}…");
    let mut instance = match ZenohAdapter::start_router_ephemeral(host, Some(port)).await {
        Ok(instance) => instance,
        Err(error) => {
            panic!("failed to start zenohd router on tcp/{host}:{port}: {error:?}");
        }
    };

    println!(
        "zenohd router ready on tcp/{}/{}. Press Ctrl+C to stop.",
        instance.host, instance.port
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
