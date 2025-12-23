use peppylib::MessengerHandle;
use peppylib::runtime::Processor;

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

pub fn run<F>(setup_fn: F) -> crate::Result<()>
where
    F: FnOnce(&peppylib::config::NodeArguments, &NodeRunner) -> crate::Result<()>,
{
    let rt =
        tokio::runtime::Runtime::new().map_err(|source| crate::Error::RuntimeInitialization {
            context: "node runner".to_string(),
            source,
        })?;
    let peppy_config = {
        let var_name = peppylib::config::RUNTIME_CONFIG_VAR_NAME;
        let peppy_config_path =
            std::env::var(var_name).map_err(|source| crate::Error::MissingInstanceIdEnvVar {
                var: var_name,
                source,
            })?;
        std::path::PathBuf::from(peppy_config_path)
    };
    let runtime_processor = Processor::new_with_peppy_config(peppy_config)?;

    rt.block_on(async move {
        let node_runner = NodeRunner::new(runtime_processor).await?;
        // TODO: Maybe turn those input arguments into a simple struct in the future?
        let args = node_runner.runtime().input_arguments();

        setup_fn(&args, &node_runner)?;

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
