use std::sync::Arc;

use peppygen::consumed_topics::hello_world_param_message_stream;
use peppygen::{NodeBuilder, NodeRunner, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        tokio::spawn(receive_messages(node_runner));
        Ok(())
    })
}

async fn receive_messages(node_runner: Arc<NodeRunner>) {
    // Subscribe once; the held subscription buffers every message in order, so
    // looping on `next` never drops a message published between iterations.
    let mut subscription = match hello_world_param_message_stream::subscribe(&node_runner).await {
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
