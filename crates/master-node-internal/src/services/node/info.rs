use crate::Result;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use std::sync::Arc;
use tokio::task::JoinHandle;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
) -> Result<JoinHandle<Result<()>>> {
    todo!("Finish")
}
