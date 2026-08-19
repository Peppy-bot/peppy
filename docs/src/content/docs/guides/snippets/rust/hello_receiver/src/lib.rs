use std::sync::Arc;

use peppygen::consumed_topics::hello_world_param::message_stream;
use peppygen::{NodeRunner, Parameters, Result};

/// The node's entry point. It lives in the library crate so tests can import
/// it: the generated test harness (`peppygen::fixtures::harness::Harness`)
/// boots it in-process, and `main.rs` delegates here for production runs.
pub async fn setup(_params: Parameters, node_runner: Arc<NodeRunner>) -> Result<()> {
    tokio::spawn(receive_messages(node_runner));
    Ok(())
}

async fn receive_messages(node_runner: Arc<NodeRunner>) {
    // Subscribe once; the held subscription buffers every message in order, so
    // looping on `next` never drops a message published between iterations.
    let mut subscription = match message_stream::subscribe(&node_runner).await {
        Ok(subscription) => subscription,
        Err(e) => {
            eprintln!("Failed to subscribe: {e}");
            return;
        }
    };

    loop {
        match subscription.next().await {
            Ok(Some((producer, message))) => {
                println!("Received from {}: {}", producer.instance_id, message.message)
            }
            Ok(None) => break,
            Err(e) => {
                eprintln!("Error receiving message: {e}");
                break;
            }
        }
    }
}
