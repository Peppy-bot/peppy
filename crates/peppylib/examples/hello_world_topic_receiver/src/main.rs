use config::consts::DEFAULT_ZENOH_PORT;
use config::node::QoSProfile;
use peppylib::{MessengerHandle, TopicMessenger};
use tokio::signal;

#[tokio::main]
async fn main() {
    let topic_name = "hello_msg";
    let qos = QoSProfile::Reliable;

    // Those properties are found in the peppy_config.json5 `deployments` array
    let ns = "/hello_ns";

    // Create a messenger for the receiving node.
    let host = "127.0.0.1";
    let port = DEFAULT_ZENOH_PORT;
    let receiver_handle = MessengerHandle::from_host_port(host, port)
        .await
        .unwrap_or_else(|error| {
            panic!("failed to create messenger on {host}:{port}: {error:?}.\n Did you start a zenohd server with the `zenohd_simple` example?")
        });

    let mut subscription = TopicMessenger::subscribe(&receiver_handle, ns, topic_name, qos)
        .await
        .expect("Should subscribe to the topic");

    println!("Waiting for payload... Press CTRL+C to stop.");

    loop {
        tokio::select! {
            _ = signal::ctrl_c() => {
                println!("Received CTRL+C, exiting.");
                break;
            }
            maybe_msg = subscription.on_next_message() => {
                match maybe_msg {
                    Some(received) => {
                        let payload = String::from_utf8_lossy(&received.payload);
                        let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S");
                        println!("[{timestamp}] Received `{payload}` from topic `{}`", received.topic);
                    }
                    None => {
                        println!("Subscription closed by sender.");
                        break;
                    }
                }
            }
        }
    }
}
