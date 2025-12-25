use config::consts::NODE_CONFIG_FILE;
use tracing::info;

use crate::MessengerHandle;
use crate::config::deserialize_parameters;
use crate::error::{Error, Result};
use crate::runtime::Processor;
use crate::services::health::listen_for_node_health;
use std::sync::Arc;

pub struct NodeRunner {
    messenger: MessengerHandle,
    runtime_processor: Processor,
}

impl NodeRunner {
    pub async fn new(runtime_processor: Processor) -> Result<Self> {
        let messenger: MessengerHandle = MessengerHandle::from_host_port(
            runtime_processor.messaging_host(),
            runtime_processor.messaging_port(),
        )
        .await?;

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
pub fn run<F, Fut, Params>(setup_fn: F) -> Result<()>
where
    F: FnOnce(Params, Arc<NodeRunner>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
    Params: serde::de::DeserializeOwned,
{
    let rt = tokio::runtime::Runtime::new().map_err(|source| Error::RuntimeInitialization {
        context: "node runner".to_string(),
        source,
    })?;
    let peppy_config = std::path::PathBuf::from(NODE_CONFIG_FILE);
    let runtime_processor = Processor::new_with_peppy_config(peppy_config)?;

    rt.block_on(async move {
        let node_runner = Arc::new(NodeRunner::new(runtime_processor).await?);
        let parameters: Params = deserialize_parameters(node_runner.runtime().input_arguments())?;

        setup_fn(parameters, Arc::clone(&node_runner)).await?;

        mandatory_services(Arc::clone(&node_runner)).await?;

        Ok(())
    })
    // Runtime drops here → all spawned tasks are cancelled
}

async fn mandatory_services(node_runner: Arc<NodeRunner>) -> Result<()> {
    let runtime = node_runner.runtime();
    info!(
        "Starting the master node with name {} and instance_id {}...",
        runtime.node_name(),
        runtime.bound_instance_id(),
    );
    let handles = vec![
        listen_for_node_health(
            &node_runner.messenger(),
            runtime.bound_master_node(),
            runtime.bound_instance_id(),
            runtime.node_name(),
        )
        .await?,
    ];

    // Wait for all service handlers
    // futures::future::try_join_all(handles)
    //     .await?
    //     .into_iter()
    //     .collect::<Result<Vec<_>>>()?;

    // TODO: Tokio::select on the handles, if shutdown is detected, stop awaiting

    info!("Shutting down master node...");
    Ok(())
}
