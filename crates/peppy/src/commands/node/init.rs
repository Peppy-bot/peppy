use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::node::Toolchain;
use core_node_api::encoding::NodeInitRequest;
use tracing::info;

use super::types::NodeName;
use crate::commands::CALLER_INSTANCE_ID;
use crate::context::AppContext;
use crate::error::{Error, Result};
use core_node::transport::poll_node_init;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

pub struct NodeInitBuilder {
    ctx: Arc<AppContext>,
    to_dir: PathBuf,
    node_name: NodeName,
    toolchain: Toolchain,
    with_container: bool,
    timeout: Option<Duration>,
}

impl NodeInitBuilder {
    pub fn new(
        ctx: &Arc<AppContext>,
        node_name: NodeName,
        toolchain: Toolchain,
        with_container: bool,
    ) -> Self {
        Self {
            ctx: Arc::clone(ctx),
            to_dir: ctx.root_dir.clone(),
            node_name,
            toolchain,
            with_container,
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
        let conn = self.ctx.connect_to_daemon().await?;

        info!(
            "Creating node '{}' in {} and core node '{}'",
            self.node_name,
            self.to_dir.display(),
            &conn.core_node_name
        );

        let request = NodeInitRequest::new(
            &self.to_dir,
            self.node_name.as_str(),
            &conn.git_hash,
            self.with_container,
            self.toolchain,
        );

        let response = poll_node_init(
            &request,
            conn.messenger,
            &conn.core_node_name,
            CALLER_INSTANCE_ID,
            &conn.core_node_name,
            self.timeout,
        )
        .await
        .map_err(|e| Error::ExecutionFailed(format!("Failed to call node_init service: {}", e)))?;

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
