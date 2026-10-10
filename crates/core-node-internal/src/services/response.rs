//! Shared mapping from a synchronous core-node service handler's `Result<T>`
//! into the wire-level `PeppyResult<T>`.

use crate::Result;
use peppylib::messaging::ServiceRequestContext;
use peppylib::{PeppyError, PeppyResult};

/// Wraps a handler's `Result<T>` into the wire-level `PeppyResult<T>`, tagging
/// any failure as an `InvalidServiceRequest` attributed to the *sending*
/// instance (`context.message().instance_id()`).
///
/// Plain request/response service handlers funnel their result through this,
/// so the sender's `instance_id` is recorded consistently and the `Err` mapping
/// lives in exactly one place.
///
/// Generic over the success type so handlers that thread extra state alongside
/// the payload (repo remove/exclude return `(response, needs_refresh)`) can
/// reuse it. A handler whose errors need another shape builds its own: node
/// info keeps internal faults apart from caller faults, and the goal-action
/// encoders attribute failures to the action name rather than the sender.
pub(crate) fn into_service_response<T>(
    context: &ServiceRequestContext,
    result: Result<T>,
) -> PeppyResult<T> {
    result.map_err(|e| PeppyError::InvalidServiceRequest {
        identifier: context.message().instance_id().to_string(),
        reason: e.to_string(),
    })
}
