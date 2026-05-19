use bytes::Bytes;
use peppylib::PeppyResult;
use peppylib::messaging::{ActionCreation, ActionFeedbackPublisher, ServiceRequestContext};
use peppylib::types::Payload;
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::debug;

/// Trait for action result types that can be encoded and sent back to clients.
pub(crate) trait ActionResult: Clone + Send + 'static {
    /// Identifier used in error messages (e.g. "node_add_result").
    fn identifier() -> &'static str;

    /// Encode the result into a payload for transmission.
    fn encode_result(&self) -> crate::Result<Payload>;
}

/// Generic state machine for action-based services.
#[derive(Default)]
pub(crate) enum ActionState<R: ActionResult> {
    #[default]
    Idle,
    /// The goal was rejected (no result polling expected).
    Rejected,
    /// An action is currently running. Carries the admission timestamp and
    /// the caller-specified timeout so [`Self::remaining_secs`] can be
    /// computed without a separate lock.
    Running {
        started_at: Instant,
        timeout_secs: u64,
    },
    /// The action completed and the result is ready to be sent.
    Completed { result: R },
    /// The result has been sent to the requester.
    ResultSent { result: R },
}

impl<R: ActionResult> ActionState<R> {
    /// Returns the remaining seconds until the in-flight task's timeout,
    /// or 0 if the state is not `Running`.
    pub(crate) fn remaining_secs(&self) -> u64 {
        match self {
            ActionState::Running {
                started_at,
                timeout_secs,
            } => Duration::from_secs(*timeout_secs)
                .saturating_sub(started_at.elapsed())
                .as_secs(),
            _ => 0,
        }
    }
}

/// Trait for handling incoming goal requests in an action-based service.
///
/// Each action service implements this to define its goal processing logic.
pub(crate) trait GoalHandler: Clone + Send + 'static {
    type Result: ActionResult;

    fn handle_goal(
        &self,
        context: ServiceRequestContext,
        user_payload: Bytes,
        feedback_publisher: ActionFeedbackPublisher,
        state: Arc<Mutex<ActionState<Self::Result>>>,
    ) -> impl Future<Output = PeppyResult<Payload>> + Send;
}

/// Generic cancel request handler shared by all action services.
pub(crate) async fn handle_cancel_request<R: ActionResult>(
    _context: ServiceRequestContext,
    state: Arc<Mutex<ActionState<R>>>,
) -> PeppyResult<Payload> {
    let state_guard = state.lock().await;
    if matches!(*state_guard, ActionState::Running { .. }) {
        Ok(Payload::from_static(
            b"cancel acknowledged (operation cannot be interrupted)",
        ))
    } else {
        Ok(Payload::from_static(
            b"cancel acknowledged (no operation in progress)",
        ))
    }
}

/// Generic result request handler shared by all action services.
pub(crate) async fn handle_result_request<R: ActionResult>(
    _context: ServiceRequestContext,
    state: Arc<Mutex<ActionState<R>>>,
) -> PeppyResult<Payload> {
    let mut state_guard = state.lock().await;

    match std::mem::replace(&mut *state_guard, ActionState::Idle) {
        running @ ActionState::Running { .. } => {
            *state_guard = running;
            // Prefix must match peppylib::encoding::RESULT_PENDING_PREFIX
            Ok(Payload::from_static(
                b"result pending: operation still in progress",
            ))
        }
        ActionState::Completed { result } => {
            let payload = result.encode_result().map_err(|e| {
                peppylib::PeppyError::InvalidServiceRequest {
                    identifier: R::identifier().to_string(),
                    reason: format!("Failed to encode result: {}", e),
                }
            })?;
            *state_guard = ActionState::ResultSent { result };
            Ok(payload)
        }
        ActionState::ResultSent { result } => {
            let payload = result.encode_result().map_err(|e| {
                peppylib::PeppyError::InvalidServiceRequest {
                    identifier: R::identifier().to_string(),
                    reason: format!("Failed to encode result: {}", e),
                }
            })?;
            *state_guard = ActionState::ResultSent { result };
            Ok(payload)
        }
        ActionState::Idle | ActionState::Rejected => {
            // Prefix must match peppylib::encoding::RESULT_PENDING_PREFIX
            Ok(Payload::from_static(b"result pending: no result available"))
        }
    }
}

/// Generic action loop shared by all action-based services.
///
/// Manages the lifecycle of goal → cancel/result polling with support for
/// abandoned actions (new goals accepted while waiting for result polling).
pub(crate) async fn run_action_loop<H: GoalHandler>(
    mut action: ActionCreation,
    handler: H,
) -> crate::Result<()> {
    let state = Arc::new(Mutex::new(ActionState::<H::Result>::default()));
    let factory = action.feedback_publisher_factory.clone();

    loop {
        let goal_result = process_goal_request(
            &mut action.goal_service,
            &factory,
            &handler,
            Arc::clone(&state),
        )
        .await;

        match goal_result {
            Ok(true) => {
                {
                    let mut state_guard = state.lock().await;
                    if matches!(*state_guard, ActionState::Rejected) {
                        *state_guard = ActionState::Idle;
                        continue;
                    }
                }

                // Goal accepted — wait for result, cancel, or new goal requests.
                loop {
                    tokio::select! {
                        cancel_result = action.cancel_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_cancel_request(context, state).await }
                            }
                        }) => {
                            match cancel_result {
                                Ok(true) => {}
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Cancel service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        result_result = action.result_service.handle_next_request({
                            let state = Arc::clone(&state);
                            move |context| {
                                let state = Arc::clone(&state);
                                async move { handle_result_request(context, state).await }
                            }
                        }) => {
                            match result_result {
                                Ok(true) => {
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, ActionState::ResultSent { .. }) {
                                        *state_guard = ActionState::default();
                                        break;
                                    }
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Result service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                        goal_result = process_goal_request(
                            &mut action.goal_service,
                            &factory,
                            &handler,
                            Arc::clone(&state),
                        ) => {
                            match goal_result {
                                Ok(true) => {
                                    let mut state_guard = state.lock().await;
                                    if matches!(*state_guard, ActionState::Rejected) {
                                        *state_guard = ActionState::Idle;
                                    }
                                }
                                Ok(false) => return Ok(()),
                                Err(e) => {
                                    debug!("Goal service error: {}", e);
                                    return Err(e.into());
                                }
                            }
                        }
                    }
                }
            }
            Ok(false) => {
                debug!("Goal service closed");
                return Ok(());
            }
            Err(e) => {
                debug!("Goal service error: {}", e);
                return Err(e.into());
            }
        }
    }
}

async fn process_goal_request<H: GoalHandler>(
    goal_service: &mut peppylib::messaging::ServiceEndpoint,
    factory: &peppylib::messaging::ActionFeedbackPublisherFactory,
    handler: &H,
    state: Arc<Mutex<ActionState<H::Result>>>,
) -> peppylib::PeppyResult<bool> {
    let factory = factory.clone();
    let handler = handler.clone();
    goal_service
        .handle_next_request(|context| async move {
            let link_id = context.link_id().to_string();
            let wire = context.message().payload().into_inner();
            let declared = factory.declare_from_wire(&link_id, wire).await?;
            handler
                .handle_goal(context, declared.user_payload, declared.publisher, state)
                .await
        })
        .await
}
