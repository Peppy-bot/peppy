use bytes::Bytes;
use config::consts::DEFAULT_ZENOH_PORT;
use peppylib::{MessengerHandle, ServiceMessenger};
use std::time::Duration;

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create service messenger on {host}:{port}: {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?"
            )
        })
}

#[tokio::main]
async fn main() {
    let service_name = "hello_service";

    // Those properties are found in the peppy_config.json5 `deployments` array
    let namespace = "/hello_ns";

    // Create a messenger for the sending node.
    let sender_handle = connect_messenger("127.0.0.1", DEFAULT_ZENOH_PORT).await;

    let request_payload = Bytes::from_static(b"Hello service");

    println!("Sending service request...");
    let response = ServiceMessenger::poll(
        &sender_handle,
        namespace,
        service_name,
        request_payload,
        Duration::from_secs(3),
    )
    .await
    .expect("Service call should succeed");

    let response_text = String::from_utf8_lossy(response.as_ref());
    println!("Received response: `{response_text}`");
}
