use config::consts::DEFAULT_ZENOH_PORT;
use config::node::QoSProfile;
use peppylib::PeppyMessenger;

async fn connect_messenger(node_name: &str, host: &str, port: u16) -> PeppyMessenger {
    PeppyMessenger::from_host_port(node_name, host, port)
        .await
        .unwrap_or_else(|error| {
            panic!("failed to create messenger for node `{node_name}` on {host}:{port}: {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?")
        })
}

#[tokio::main]
async fn main() {
    // Those attributes are found in the peppy.json5 `exposes`
    let emitter_node_name = "hello_emitter";
    let topic_name = "hello_msg";
    let qos = QoSProfile::Reliable;

    // Those properties are found in the peppy_config.json5 `deployments` array
    let ns = "/hello_ns";

    // Create a messenger for the receiving node.
    let receiver_node = connect_messenger("hello_receiver", "127.0.0.1", DEFAULT_ZENOH_PORT).await;

    let mut subscription = receiver_node
        .receive_topic_msg(emitter_node_name, ns, topic_name, qos)
        .await
        .expect("Should subscribe to the topic");

    println!("Waiting for payload...");
    let received = subscription
        .rx
        .recv()
        .await
        .expect("Should receive the published message");

    let payload = String::from_utf8_lossy(&received.payload);

    println!("Received `{payload}` from topic `{}`", received.topic);
}
