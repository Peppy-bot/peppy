use crate::Result;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use std::{sync::Arc, time::Duration};
use tokio::task::JoinHandle;

pub async fn listen_for_node_info(
    messenger: &MessengerHandle,
    master_node_name: &str,
    instance_id: &str,
    node_name: &str,
    node_stack: Arc<NodeStack>,
    timeout: Duration,
) -> Result<JoinHandle<Result<()>>> {
    todo!(
        "Given a node path in NodeInfoRequest, pull the `peppy.json` from that node. In the case of git find a way to pull only the `peppy.json5` file in the node instead of cloning the entire repo."
    )
}
