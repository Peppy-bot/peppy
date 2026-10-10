//! Reading the copies of a daemon's stack along the route its goals take.

use std::time::Duration;

use core_node_api::encoding::{CopyChange, CopyInfo, StackListRequest};
use peppylib::PeppyError;
use peppylib::core_node::transport::poll;

use crate::DaemonRoute;

/// The copies on a daemon's stack, and the join or the removal of a copy
/// that holds the stack now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackCopies {
    /// Each copy with its name, its option, its host and its instances. The
    /// daemon writes a copy into its record only when its join ends.
    pub copies: Vec<CopyInfo>,
    /// The join or the removal that holds the stack, so a join in progress
    /// is here and not in `copies`.
    pub copy_change: Option<CopyChange>,
}

/// The copies on the stack of the daemon `route` reaches, and the copy
/// change that holds it, as its `stack list` answers them. A daemon that
/// holds no launch answers no copy. Fails when the daemon does not answer
/// within `timeout`.
pub async fn list_copies(
    route: DaemonRoute<'_>,
    timeout: Duration,
) -> Result<StackCopies, PeppyError> {
    let response = poll(
        &StackListRequest::new(),
        route.messenger,
        route.caller_core_node,
        route.caller_instance_id,
        route.daemon_core_node,
        timeout,
    )
    .await?;
    Ok(StackCopies {
        copies: response.copies,
        copy_change: response.copy_change,
    })
}
