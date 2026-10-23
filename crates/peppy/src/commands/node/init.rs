use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use config::node::Toolchain;
use core_node_api::encoding::NodeInitRequest;
use tracing::info;

use super::types::NodeName;
use crate::commands::caller_instance_id;
use crate::context::AppContext;
use crate::error::Result;
use peppylib::core_node::transport::poll;

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

    pub fn with_dir(mut self, dir: impl Into<PathBuf>) -> Self {
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

        // The request embeds `to_dir` (defaulting to the caller's cwd), which
        // the daemon joins and creates on ITS OWN filesystem: a remote target
        // would silently scaffold the node on the wrong machine.
        crate::commands::reject_remote_target_for_local_path(&conn, "peppy node init")?;

        info!(
            "Creating node '{}' in {} and core node '{}'",
            self.node_name,
            self.to_dir.display(),
            &conn.target_core_node
        );

        let request = NodeInitRequest::new(
            &self.to_dir,
            self.node_name.as_str(),
            &conn.git_hash,
            self.with_container,
            self.toolchain,
        );

        poll(
            &request,
            conn.messenger,
            &conn.core_node_name,
            caller_instance_id(),
            &conn.target_core_node,
            self.timeout,
        )
        .await
        .map_err(|e| crate::commands::request_error("Failed to initialize the node", e))?;

        info!(
            "Successfully created node '{}' at {}/{}",
            self.node_name,
            self.to_dir.display(),
            self.node_name
        );
        Ok(())
    }
}
