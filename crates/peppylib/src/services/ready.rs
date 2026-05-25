use crate::messaging::{NODE_READY_SERVICE, SenderTarget};
use crate::runtime::TaskHandle;
use crate::{MessengerHandle, PeppyResult};

pub async fn listen_for_node_ready(
    messenger: &MessengerHandle,
    core_node_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    super::listen_for_echo_service(
        messenger,
        core_node_node,
        instance_id,
        as_identity,
        NODE_READY_SERVICE,
        NODE_READY_SERVICE,
    )
    .await
}
