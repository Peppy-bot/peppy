use crate::runtime::TaskHandle;
use std::sync::Arc;
use tokio::sync::{Mutex, oneshot};
use tracing::debug;

use crate::messaging::{SHUTDOWN_SERVICE, SenderTarget, ServiceAnswer, ServiceRequestContext};
use crate::{MessengerHandle, PeppyResult, ServiceMessenger};

/// Receiver for shutdown signals. When a shutdown request is received by the service,
/// this receiver will complete.
pub type ShutdownReceiver = oneshot::Receiver<()>;

type ShutdownSender = Arc<Mutex<Option<oneshot::Sender<()>>>>;

pub async fn listen_for_shutdown(
    messenger: &MessengerHandle,
    core_node: &str,
    instance_id: &str,
    as_identity: SenderTarget,
) -> PeppyResult<(TaskHandle<PeppyResult<()>>, ShutdownReceiver)> {
    let mut endpoint = ServiceMessenger::listen(
        messenger,
        core_node,
        instance_id,
        as_identity,
        SHUTDOWN_SERVICE,
    )
    .await?;

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let shutdown_tx = Arc::new(Mutex::new(Some(shutdown_tx)));

    let handle = crate::runtime::spawn(async move {
        endpoint
            .handle_requests(|context| {
                let shutdown_tx = Arc::clone(&shutdown_tx);
                async move { handle_shutdown_request(context, shutdown_tx).await }
            })
            .await
    });

    Ok((handle, shutdown_rx))
}

async fn handle_shutdown_request(
    context: ServiceRequestContext,
    shutdown_tx: ShutdownSender,
) -> ServiceAnswer {
    let sender_instance_id = context.message().instance_id();
    let payload = context.message().payload();

    debug!("Received `shutdown` request from {sender_instance_id}");

    // Fire the receiver that the node runtime awaits, during setup and in
    // `run_post_setup_services` (runtime/builder.rs). Only the first request
    // finds the sender; later ones just echo.
    if let Some(tx) = shutdown_tx.lock().await.take() {
        let _ = tx.send(());
    }

    Ok(payload)
}
