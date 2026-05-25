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
    loop {
        let result = hello_world_param_message_stream::on_next_message_received(
            &node_runner,
            None,
        )
        .await;

        match result {
            Ok((instance_id, message)) => println!("Received from {instance_id}: {}", message.message),
            Err(e) => {
                eprintln!("Error receiving message: {e}");
                break;
            }
        }
    }
}
