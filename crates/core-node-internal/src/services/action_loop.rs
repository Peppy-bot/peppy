//! Shared accept loop for the built-in core-node actions.
//!
//! Each built-in action (`node_add`, `node_run`, `node_build`, `stack_launch`,
//! `repo_refresh`) is single-goal: it admits one in-flight goal at a time,
//! guarded by a [`super::node::gate::ConcurrencyGate`], and rejects concurrent
//! goals. The loop pulls goals from the [`ActionServer`] and hands each to the
//! action's [`GoalHandler`], which owns the full accept/reject decision: on
//! acceptance it registers a `GoalContext`, replies, and spawns the work that
//! drives the goal to [`peppylib::messaging::GoalContext::complete`].

use peppylib::PeppyResult;
use peppylib::messaging::{ActionServer, ServiceRequestContext, ServiceResponder};
use peppylib::types::Payload;
use std::future::Future;

/// Reply to a goal request with an encoded goal-response payload, or surface an
/// encoding failure as a service handler error. Centralizes the
/// `respond` / `respond_error` split at every goal accept/reject site.
pub(crate) async fn reply_goal(responder: ServiceResponder, encoded: PeppyResult<Payload>) {
    match encoded {
        Ok(payload) => {
            let _ = responder.respond(payload).await;
        }
        Err(err) => {
            let _ = responder.respond_error(err.to_string()).await;
        }
    }
}

/// Handles a single goal request for a built-in action.
///
/// The handler decodes the goal, runs admission through the action's gate, and
/// replies on the `responder`. On acceptance it registers a per-goal
/// `GoalContext` via [`ActionServer::register_goal`] and spawns the work task.
/// On rejection it replies without registering, so the result service later
/// answers any stray result request for that goal with a definitive error.
pub(crate) trait GoalHandler: Clone + Send + 'static {
    fn handle_goal(
        &self,
        request: ServiceRequestContext,
        responder: ServiceResponder,
        server: &ActionServer,
    ) -> impl Future<Output = ()> + Send;
}

/// Accept loop shared by all built-in core-node actions. Runs until the goal
/// service closes (the messenger session is torn down).
pub(crate) async fn run_action_loop<H: GoalHandler>(
    mut server: ActionServer,
    handler: H,
) -> crate::Result<()> {
    while let Some((request, responder)) = server.recv_next_goal().await? {
        handler.handle_goal(request, responder, &server).await;
    }
    Ok(())
}
