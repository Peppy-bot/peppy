use crate::Result;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

const NODE_GENERATE_TIMEOUT: Duration = Duration::from_secs(30);

struct NodeStartTimeouts {
    startup: Duration,
    health_check: Duration,
}

pub async fn listen_for_stack_launch(
    messenger: &MessengerHandle,
    master_node_node: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    node_startup_timeout: Duration,
    node_start_health_timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    // Step 1: Clear the node stack
    // Step 3: Run `node info` on each node in the deployment

    // Final step, clean up the temp dir
    todo!("Finish")
}
