//! Shared mapping from a synchronous core-node service handler's `Result<T>`
//! into the wire-level `PeppyResult<T>`.

use crate::Result;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{PeppyError, PeppyResult};

/// Wraps a handler's `Result<T>` into the wire-level `PeppyResult<T>`, tagging
/// any failure as an `InvalidServiceRequest` attributed to the *sending*
/// instance (`context.message().instance_id()`).
///
/// Every plain request/response service handler (ping, info, datastore, stack
/// list/reset, repo list/add/remove/exclude, node init/sync/remove/stop)
/// funnels its result through this, so the sender's `instance_id` is recorded
/// consistently and the `Err` mapping lives in exactly one place.
///
/// Generic over the success type so handlers that thread extra state alongside
/// the payload (e.g. repo remove/exclude returning `(Payload, needs_refresh)`)
/// can reuse it. Handlers with bespoke error shaping — the clock service's
/// per-stamp timing errors and the goal-action encoders that attribute failures
/// to the action name rather than the sender — intentionally do not use this.
pub(crate) fn into_service_response<T>(
    context: &ServiceRequestContext,
    result: Result<T>,
) -> PeppyResult<T> {
    result.map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: context.message().instance_id().to_string(),
        reason: e.to_string(),
    })
}
