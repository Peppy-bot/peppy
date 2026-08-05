use std::sync::Arc;

use peppygen::consumed_topics::greeter::message_stream;
use peppygen::{NodeBuilder, NodeRunner, Parameters, Result};

fn main() -> Result<()> {
    NodeBuilder::new().run(|_args: Parameters, node_runner| async move {
        // The `greeter` slot declares `cardinality: "zero_or_one"`, so its
        // accessor is an `Option`: `Some` where the deployment linked a
        // producer, `None` where it wrote the slot vacant. There is no third
        // case, and no empty set to interpret.
        match message_stream::bound_producer(&node_runner).cloned() {
            Some(greeter) => {
                println!("greeter bound: {}", greeter.instance_id);
                tokio::spawn(receive_messages(node_runner));
            }
            None => println!("no greeter bound: running without greetings"),
        }
        Ok(())
    })
}

async fn receive_messages(node_runner: Arc<NodeRunner>) {
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
