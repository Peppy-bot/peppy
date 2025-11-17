use pmi::start_zenohd_process;
use tokio::signal;

pub const DEFAULT_ZENOH_PORT: u16 = 7448;

#[tokio::main]
async fn main() {
    let host = "0.0.0.0";
    let port = DEFAULT_ZENOH_PORT;

    println!("Starting zenohd router on tcp/{host}:{port}…");
    let (mut messenger, temp_dir, router_host, router_port) =
        match start_zenohd_process(host, Some(port)).await {
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
