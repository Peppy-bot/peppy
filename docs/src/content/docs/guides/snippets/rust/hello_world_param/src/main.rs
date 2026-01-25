use std::sync::Arc;
use std::time::Duration;

use peppygen::{NodeBuilder, NodeRunner, Parameters, Result};
use tokio_util::sync::CancellationToken;

/// Emits a "hello world count X" message every 3 seconds, starting immediately.
/// The loop runs until the cancellation token is triggered.
async fn emit_hello_world_loop(runner: Arc<NodeRunner>, token: CancellationToken, name: String) {
    let mut counter: u64 = 0;
    let mut interval = tokio::time::interval(Duration::from_secs(3));
    loop {
        tokio::select! {
            _ = token.cancelled() => break,
            _ = interval.tick() => {
                counter += 1;
                let message = format!("hello {name} count {counter}");
                println!("{message}");
                if let Err(e) = peppygen::exposed_topics::message_stream::emit(&runner, message).await {
                    eprintln!("Failed to emit hello world: {e}");
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
