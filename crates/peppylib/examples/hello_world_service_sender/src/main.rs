use bytes::Bytes;
use config::consts::DEFAULT_ZENOH_PORT;
use peppylib::ServiceMessenger;
use std::time::Duration;

async fn connect_messenger(node_name: &str, host: &str, port: u16) -> ServiceMessenger {
    ServiceMessenger::from_host_port(node_name, host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create service messenger for node `{node_name}` on {host}:{port}: {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?"
            )
        })
}

#[tokio::main]
async fn main() {
    // Those attributes are found in the peppy.json5 `exposes`
    let service_node_name = "hello_receiver";
    let service_name = "hello_service";

    // Those properties are found in the peppy_config.json5 `deployments` array
    let ns = "/hello_ns";

    // Create a messenger for the sending node.
    let sender_node = connect_messenger("hello_emitter", "127.0.0.1", DEFAULT_ZENOH_PORT).await;

    let request_payload = Bytes::from_static(b"Hello service");

    println!("Sending service request...");
    let response = sender_node
        .poll(
            service_node_name,
            ns,
            service_name,
            request_payload,
            Duration::from_secs(3),
        )
        .await
        .expect("Service call should succeed");

    let response_text = String::from_utf8_lossy(response.as_ref());
    println!("Received response: `{response_text}`");
}
