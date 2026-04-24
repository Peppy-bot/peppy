//! Higher-level wrapper around the `STACK_LIST` service.
//!
//! Unlike [`crate::core_node::transport::poll_stack_list`], which returns the
//! raw wire response, this layer parses `graph_json` into a
//! [`SerializedNodeGraph`] so callers don't have to think about the
//! JSON-on-capnp shape.

use std::time::Duration;

use core_node_api::SerializedNodeGraph;
use core_node_api::encoding::StackListRequest;

use crate::MessengerHandle;
use crate::core_node::transport::poll_stack_list;
use crate::error::{Error, Result};

/// Deserialized form of `StackListResponse`: `graph_json` parsed into a
/// `SerializedNodeGraph`, with the optional DOT rendering preserved.
#[derive(Debug, Clone)]
pub struct StackList {
    pub graph: SerializedNodeGraph,
    pub dot_graph: Option<String>,
}

pub async fn stack_list(
    request: &StackListRequest,
    messenger: &MessengerHandle,
    bound_core_node: &str,
    as_instance_id: &str,
    target_core_node: &str,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<StackList> {
    let response = poll_stack_list(
        request,
        messenger,
        bound_core_node,
        as_instance_id,
        target_core_node,
        response_timeout,
    )
    .await?;

    let graph: SerializedNodeGraph = serde_json::from_str(&response.graph_json)
        .map_err(|e| Error::Deserialization(format!("failed to parse stack graph JSON: {e}")))?;

    Ok(StackList {
        graph,
        dot_graph: response.dot_graph,
    })
}
