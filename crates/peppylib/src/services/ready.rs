use crate::messaging::NODE_READY_SERVICE;
use crate::runtime::TaskHandle;
use crate::{MessengerHandle, PeppyResult};

pub async fn listen_for_node_ready(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    node_name: &str,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    super::listen_for_echo_service(
        messenger,
        core_node_node,
        instance_id,
        node_name,
        NODE_READY_SERVICE,
        "node_ready",
    )
    .await
}
