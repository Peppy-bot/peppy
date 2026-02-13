use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::node::Toolchain;
use daemon_node::encoding::NodeInitRequest;
use tracing::info;

use super::types::NodeName;
use crate::context::AppContext;
use crate::error::{Error, Result};

const CALLER_INSTANCE_ID: &str = "peppy-cli";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct NodeInitBuilder {
    ctx: Arc<AppContext>,
    to_dir: PathBuf,
    node_name: NodeName,
    toolchain: Toolchain,
    timeout: Option<Duration>,
}

impl NodeInitBuilder {
    pub fn new(ctx: &Arc<AppContext>, node_name: NodeName, toolchain: Toolchain) -> Self {
        Self {
            ctx: Arc::clone(ctx),
            to_dir: ctx.root_dir.clone(),
            node_name,
            toolchain,
            timeout: Some(REQUEST_TIMEOUT),
        }
    }

    #[allow(clippy::wrong_self_convention)]
    pub fn to_dir(mut self, dir: impl Into<PathBuf>) -> Self {
        self.to_dir = dir.into();
        self
    }

    pub fn with_timeout(mut self, timeout: impl Into<Option<Duration>>) -> Self {
        self.timeout = timeout.into();
        self
    }

    pub fn build(self) -> Result<()> {
        crate::commands::block_on(self.build_async())
    }

    async fn build_async(self) -> Result<()> {
        // Read the daemon state to discover the daemon node name
        let daemon_state = self.ctx.read_daemon_state().map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to read daemon state. Is the peppy daemon running? Error: {}",
                e
            ))
        })?;
        let daemon_node_name = &daemon_state.daemon_node_name;
        let git_hash = &daemon_state.git_hash;

        info!(
            "Creating node '{}' in {} and daemon node '{}'",
            self.node_name,
            self.to_dir.display(),
            &daemon_node_name
        );

        // Connect to the daemon if not already connected
        self.ctx.connect().await?;

        let messenger_handle = self
            .ctx
            .messenger_handle()
            .ok_or_else(|| Error::ExecutionFailed("Failed to connect to daemon".to_string()))?;

        let request = NodeInitRequest::new(
            &self.to_dir,
            self.node_name.as_str(),
            git_hash,
            self.toolchain,
        );

        let response = request
            .poll(
                messenger_handle,
                daemon_node_name,
                CALLER_INSTANCE_ID,
                daemon_node_name,
                self.timeout,
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
