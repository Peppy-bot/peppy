use std::sync::Arc;
use std::time::Duration;

use peppygen::emitted_topics::message_stream;
use peppygen::{NodeBuilder, NodeRunner, Parameters, Result};
use peppylib::runtime::CancellationToken;

/// Emits a "hello world count X" message every 3 seconds, starting immediately.
/// The loop runs until the cancellation token is triggered.
async fn emit_hello_world_loop(runner: Arc<NodeRunner>, token: CancellationToken, name: String) {
    // Declare the publisher once; every publish below is then lock-free.
    let publisher = match message_stream::declare_publisher(&runner).await {
        Ok(publisher) => publisher,
        Err(e) => {
            eprintln!("Failed to declare message_stream publisher: {e}");
            return;
        }
    };
    let mut counter: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = interval.tick() => {
                counter += 1;
                let message = format!("hello {name} count {counter}");
                println!("{message}");
                match message_stream::build_message(message) {
                    Ok(payload) => {
                        if let Err(e) = publisher.publish(payload).await {
                            eprintln!("Failed to publish hello world: {e}");
                        }
                    }
                    Err(e) => eprintln!("Failed to build hello world message: {e}"),
                }
            }
        }
    }
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|args: Parameters, node_runner| async move {
        let runner = node_runner.clone();
        let token = node_runner.cancellation_token().clone();

        // We use tokio::spawn to avoid blocking the closure
        tokio::spawn(emit_hello_world_loop(runner, token, args.name.clone()));

        Ok(())
    })
}
