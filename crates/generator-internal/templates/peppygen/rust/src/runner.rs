use peppylib::MessengerHandle;
use peppylib::runtime::Processor;
use std::sync::Arc;

pub struct NodeRunner {
    messenger: MessengerHandle,
    runtime_processor: Processor,
}

impl NodeRunner {
    pub async fn new(runtime_processor: Processor) -> crate::Result<Self> {
        let messenger: MessengerHandle = MessengerHandle::from_host_port(
            runtime_processor.messaging_host(),
            runtime_processor.messaging_port(),
        )
        .await
        .map_err(crate::Error::Messaging)?;

        Ok(Self {
            messenger,
            runtime_processor,
        })
    }

    pub fn runtime(&self) -> &Processor {
        &self.runtime_processor
    }

    pub fn messenger(&self) -> &MessengerHandle {
        &self.messenger
    }
}

#[allow(dead_code)]
pub fn run<F, Fut>(setup_fn: F) -> crate::Result<()>
where
    F: FnOnce(crate::parameters::Parameters, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = crate::Result<()>>,
{
    let rt =
        tokio::runtime::Runtime::new().map_err(|source| crate::Error::RuntimeInitialization {
            context: "node runner".to_string(),
            source,
        })?;
    let peppy_config = std::path::PathBuf::from(peppylib::config::NODE_CONFIG_FILE);
    let runtime_processor = Processor::new_with_peppy_config(peppy_config)?;

    rt.block_on(async move {
        let node_runner = Arc::new(NodeRunner::new(runtime_processor).await?);
        let parameters: crate::parameters::Parameters =
            peppylib::config::deserialize_parameters(node_runner.runtime().input_arguments())?;

        setup_fn(parameters, node_runner).await?;

        // Spin until shutdown signal
        shutdown_signal().await;

        Ok(())
    })
    // Runtime drops here → all spawned tasks are cancelled
}

async fn shutdown_signal() {
    // TODO add a signal that can emanate from a request from the master_node
    let ctrl_c = tokio::signal::ctrl_c();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");

        tokio::select! {
            _ = ctrl_c => {}
            _ = sigterm.recv() => {}
        }
    }

    #[cfg(not(unix))]
    {
        let _ = ctrl_c.await;
    }
}
