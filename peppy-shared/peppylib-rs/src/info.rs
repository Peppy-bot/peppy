//! High-level wrapper around the `INFO` service.
//!
//! Unlike a raw [`crate::core_node::transport::poll`], which returns the
//! wire response and requires the caller to thread routing parameters through
//! by hand, this layer takes a [`NodeRunner`] directly. The response is
//! already fully typed, so it is returned as-is.

use std::time::Duration;

use core_node_api::encoding::{InfoRequest, InfoResponse};

use crate::core_node::poll_bound_core_node;
use crate::error::Result;
use crate::runtime::NodeRunner;

pub async fn info(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<InfoResponse> {
    poll_bound_core_node(node_runner, &InfoRequest::new(), response_timeout.into()).await
}
