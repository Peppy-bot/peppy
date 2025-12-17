use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::peppy_config::BuildSystem;
use master_node::encoding::NodeInitRequest;
use tracing::info;

use super::types::NodeName;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const MASTER_NODE_NAME: &str = "master_node";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

pub struct NodeBuilder {
    ctx: Arc<AppContext>,
    to_dir: PathBuf,
    node_name: NodeName,
    build_system: BuildSystem,
}

impl NodeBuilder {
    pub fn new(ctx: &Arc<AppContext>, node_name: NodeName) -> Self {
        Self {
            ctx: Arc::clone(ctx),
            to_dir: ctx.root_dir.clone(),
            node_name,
            build_system: BuildSystem::Rust,
        }
    }

    pub fn to_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.to_dir = dir.into();
        self
    }

    pub fn build_system(mut self, build_system: BuildSystem) -> Self {
        self.build_system = build_system;
        self
    }

    pub fn build(self) -> Result<()> {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(self.build_async())
    }

    async fn build_async(self) -> Result<()> {
        info!(
            "Creating node '{}' in {}",
            self.node_name,
            self.to_dir.display()
        );

        // Connect to the daemon if not already connected
        self.ctx.connect().await?;

        let messenger_handle = self
            .ctx
            .messenger_handle()
            .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

        let request = NodeInitRequest::new(&self.to_dir, self.node_name.as_str())
            .with_build_system(self.build_system);

        let response = request
            .poll(
                messenger_handle,
                MASTER_NODE_NAME,
                CALLER_INSTANCE_ID,
                MASTER_NODE_NAME,
                None,
                REQUEST_TIMEOUT,
            )
            .await
            .map_err(|e| {
                Error::ExecutionFailed(format!("Failed to call node_init service: {}", e))
            })?;

        if response.success {
            info!(
                "Successfully created node '{}' at {}/{}",
                self.node_name,
                self.to_dir.display(),
                self.node_name
            );
            Ok(())
        } else {
            Err(Error::ExecutionFailed(response.error_message))
        }
    }
}
