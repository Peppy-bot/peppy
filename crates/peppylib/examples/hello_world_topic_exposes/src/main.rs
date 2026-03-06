use bytes::Bytes;
use config::consts::DEFAULT_MESSAGING_PORT;
use config::node::QoSProfile;
use names_generator2::get_random;
use peppylib::{MessengerHandle, TopicMessenger};
use rand::rng;

#[tokio::main]
async fn main() {
    let topic_name = "hello_msg";
    let qos = QoSProfile::Reliable;

    // Those properties are found in the peppy_launcher.json5 `deployments` array
    let node_name = "hello_node";
    let core_node = format!("{}_core", get_random(rng()));
    let instance_id = format!("{}_emitter", get_random(rng()));

    // Create a messenger for the sending node.
    let host = "127.0.0.1";
    let port = DEFAULT_MESSAGING_PORT;
    let sender_handle = MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!(
                "failed to create messenger on {host}:{port}: {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?"
            )
        });

    let payload = Bytes::from_static(b"Hello world");

    println!("Sending payload as {instance_id} with core node {core_node}...");
    TopicMessenger::emit(
        &sender_handle,
        &core_node,
        &instance_id,
        node_name,
        topic_name,
        qos,
        payload,
    )
    .await
    .expect("Should send the payload");
    println!("Payload sent");
}
