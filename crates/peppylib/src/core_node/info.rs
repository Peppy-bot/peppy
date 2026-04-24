//! High-level wrapper around the `INFO` service.
//!
//! Unlike [`crate::core_node::transport::poll_info`], which returns the raw
//! wire response and requires the caller to thread routing parameters through
//! by hand, this layer takes a [`NodeRunner`] directly. The response is
//! already fully typed, so it is returned as-is.

use std::time::Duration;

use core_node_api::encoding::{InfoRequest, InfoResponse};

use crate::core_node::transport::poll_info;
use crate::error::Result;
use crate::runtime::NodeRunner;

const DEFAULT_RESPONSE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn info(
    node_runner: &NodeRunner,
    response_timeout: impl Into<Option<Duration>> + Send,
) -> Result<InfoResponse> {
    let timeout = response_timeout.into().unwrap_or(DEFAULT_RESPONSE_TIMEOUT);
    let processor = node_runner.processor();
    let core_node = processor.bound_core_node();

    poll_info(
        &InfoRequest::new(),
        node_runner.messenger(),
        core_node,
        processor.bound_instance_id(),
        core_node,
        timeout,
    )
    .await
}
