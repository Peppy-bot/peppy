//! High-level wrapper around the `STACK_LIST` service.
//!
//! Unlike [`crate::core_node::transport::poll_stack_list`], which returns the
//! raw wire response and requires the caller to thread routing parameters
//! through by hand, this layer takes a [`NodeRunner`] directly and parses
//! `graph_json` into a [`SerializedNodeGraph`], so callers don't have to think
//! about the JSON-on-capnp shape.

use std::time::Duration;

use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;

use crate::core_node::transport::poll_stack_list;
use crate::error::{Error, Result};
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

/// Deserialized form of `StackListResponse`: `graph_json` parsed into a
/// `SerializedNodeGraph`, with the optional DOT rendering preserved.
#[derive(Debug, Clone)]
pub struct StackList {
    pub graph: SerializedNodeGraph,
    pub dot_graph: Option<String>,
}

pub async fn list(
    node_runner: &NodeRunner,
    with_dot_graph: bool,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<StackList> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    let response = poll_stack_list(
        &StackListRequest::new(with_dot_graph),
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
        dot_graph: response.dot_graph,
    })
}
