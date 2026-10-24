//! How a core-node service handler answers: its encoded response, or an
//! error reply with a failure kind and a message.

use crate::Result;
use core_node_api::DaemonFailureKind;
use peppylib::messaging::{Failure, ServiceAnswer};
use peppylib::types::Payload;

/// The failure of a request or a goal: `kind`, a kind of the daemon, and
/// the message of `reason`.
pub(crate) fn failure(kind: impl Into<DaemonFailureKind>, reason: impl ToString) -> Failure {
    Failure::new(kind.into(), reason.to_string())
}

/// `failure` with `message` as its message: a failure of the same kind, which
/// a caller rephrases to say where it happened.
pub(crate) fn rephrased(failure: &Failure, message: impl Into<String>) -> Failure {
    Failure::new(failure.kind().clone(), message)
}

/// The answer of a handler whose work gives `result`: the encoded response,
/// or the error reply of its error, with the failure kind the error gives
/// ([`crate::Error::into_failure`]).
///
/// Plain request/response service handlers funnel their result through
/// this, so the mapping of an error to its failure lives in one place. A
/// handler that refuses a request for a reason of its own builds that
/// failure with [`failure`] and the kind that names the reason.
pub(crate) fn into_service_answer(result: Result<Payload>) -> ServiceAnswer {
    result.map_err(crate::Error::into_failure)
}
