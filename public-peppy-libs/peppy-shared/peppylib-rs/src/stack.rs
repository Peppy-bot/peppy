//! High-level wrapper around the `STACK_LIST` service.
//!
//! Unlike a raw [`crate::core_node::transport::poll`], which returns the
//! wire response and requires the caller to thread routing parameters
//! through by hand, this layer takes a [`NodeRunner`] directly and parses
//! `graph_json` into a [`SerializedNodeGraph`], so callers don't have to think
//! about the JSON-on-capnp shape.

use std::time::Duration;

use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::{CopyInfo, StackListRequest};

use crate::core_node::transport::poll;
use crate::error::{Error, Result};
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deserialized form of `StackListResponse`: `graph_json` parsed into a
/// `SerializedNodeGraph`, with the serving daemon's identity preserved.
#[derive(Debug, Clone)]
pub struct StackList {
    pub graph: SerializedNodeGraph,
    /// The copies the serving coordinator runs.
    pub copies: Vec<CopyInfo>,
    /// The cooperative shutdown grace the serving daemon is configured with,
    /// which a caller sizes a destructive request's budget from.
    pub shutdown_grace_secs: u64,
    /// Presence identity of the serving daemon: its core-node name and
    /// daemon-generation instance id, matching its core-node presence token.
    pub core_node: String,
    pub instance_id: String,
    /// Hostname of the machine the serving daemon runs on.
    pub host_name: String,
}

pub async fn list(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<StackList> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll(
        &StackListRequest::new(),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::Deserialization(format!("failed to parse stack graph JSON: {e}")))?;

    Ok(StackList {
        graph,
        copies: response.copies,
        shutdown_grace_secs: response.shutdown_grace_secs,
        core_node: response.core_node,
        instance_id: response.instance_id,
        host_name: response.host_name,
    })
}
