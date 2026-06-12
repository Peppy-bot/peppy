pub mod clock_offset;
pub mod daemon_watchdog;
pub mod health;
pub mod ready;
pub mod shutdown;

use crate::messaging::{SenderTarget, ServiceRequestContext};
use crate::runtime::TaskHandle;
use crate::types::Payload;
use crate::{MessengerHandle, PeppyResult, ServiceMessenger};
use tracing::debug;

/// Starts a service that echoes each request's payload back as the response.
///
/// Used by health and ready services which share identical handling logic.
pub(crate) async fn listen_for_echo_service(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
    service_name: &str,
    log_label: &'static str,
) -> PeppyResult<TaskHandle<PeppyResult<()>>> {
    let mut endpoint =
        ServiceMessenger::listen(messenger, core_node, instance_id, as_identity, service_name)
            .await?;

    let handle = crate::runtime::spawn(async move {
        endpoint
            .handle_requests(|context| handle_echo_request(context, log_label))
            .await
    });
    Ok(handle)
}

async fn handle_echo_request(
    context: ServiceRequestContext,
    log_label: &str,
) -> PeppyResult<Payload> {
    let sender_instance_id = context.message().instance_id();
    debug!("Received `{log_label}` request from {sender_instance_id}");

    // Echo service validates connectivity, not message structure.
    // Health and ready services share this handler for simplicity.
    let payload = context.message().payload();
    Ok(payload)
}
