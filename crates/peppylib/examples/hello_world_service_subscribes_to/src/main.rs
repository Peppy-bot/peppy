use config::consts::DEFAULT_MESSAGING_PORT;
use names_generator2::get_random;
use peppylib::{MessengerHandle, Payload, ServiceMessenger};
use rand::rng;
use std::time::Duration;
use peppylib::messaging::SenderTarget;

const POLL_SERVICE_NAME: &str = "hello_service";
const POLL_NODE_NAME: &str = "hello_node";

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
    // Create a messenger for the sending node.
    let sender_handle = connect_messenger("127.0.0.1", DEFAULT_MESSAGING_PORT).await;
    let core_node = format!("{}_core", get_random(rng()));
    let as_instance_id = format!("{}_caller", get_random(rng()));

    let request_payload = Payload::from_static(b"Hello service");

    println!(
        "Sending service request as instance_id {as_instance_id} and core node {core_node}..."
    );
    let response = ServiceMessenger::poll(
        &sender_handle,
        &core_node,
        &as_instance_id,
        SenderTarget::node(POLL_NODE_NAME, "v1").expect("test target"),
        None,
        POLL_SERVICE_NAME,
        None, // to_core_node - not needed
        None, // to_instance_id - we don't need to point to a particular instance, any would work
        request_payload,
        Duration::from_secs(3),
    )

    .await
    .expect("Service call should succeed");

    let response_payload = response.payload();
    let response_text = String::from_utf8_lossy(response_payload.as_ref());
    let from_service_instance_id = response.instance_id();
    println!("Received response from {from_service_instance_id} instance_id: `{response_text}`");
}
