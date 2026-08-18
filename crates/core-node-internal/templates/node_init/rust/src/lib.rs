use std::sync::Arc;

use peppygen::{NodeRunner, Parameters, Result};

/// The node's entry point. It lives in the library crate so tests can import
/// it: the generated test harness (`peppygen::fixtures::harness::Harness`)
/// boots it in-process, and `main.rs` delegates here for production runs.
pub async fn setup(parameters: Parameters, node_runner: Arc<NodeRunner>) -> Result<()> {
    let _ = parameters;
    let _ = node_runner;
    Ok(())
}
