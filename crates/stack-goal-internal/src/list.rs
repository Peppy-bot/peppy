//! Reading the copies of a daemon's stack along the route its goals take.

use std::time::Duration;

use core_node_api::encoding::{CopyInfo, StackListRequest};
use peppylib::PeppyError;
use peppylib::core_node::transport::poll;

use crate::DaemonRoute;

/// The copies on the stack of the daemon `route` reaches, as its `stack list`
/// answers them: each with its name, its option, its host and its instances.
/// A daemon that holds no launch answers none. The daemon writes a copy into
/// its record only when its join ends, so a join in progress is not in the
/// answer. Fails when the daemon does not answer within `timeout`.
pub async fn list_copies(
    route: DaemonRoute<'_>,
    timeout: Duration,
) -> Result<Vec<CopyInfo>, PeppyError> {
    let response = poll(
        &StackListRequest::new(),
        route.messenger,
        route.caller_core_node,
        route.caller_instance_id,
        route.daemon_core_node,
        timeout,
    )
    .await?;
    Ok(response.copies)
}
