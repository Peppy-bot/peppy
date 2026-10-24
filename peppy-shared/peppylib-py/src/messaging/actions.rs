use bytes::Bytes;
use peppylib::messaging::{
    ActionFeedbackPublisher, ActionFeedbackPublisherFactory, ActionGoalHandle, ActionMessenger,
    ActionResultReply, ActionWireSender, CancelCause, ConcurrentAction, DEFAULT_FEEDBACK_BOUND,
    Failure, FeedbackBound, GoalAnswer, GoalContext, GoalOutcome, NonEmptyPayload, PendingGoal,
    SenderGrace, ServiceEndpoint, decode_cancel_ack,
};
use peppylib::types::Payload;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::num::NonZeroUsize;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::services::PyServiceEndpoint;
use super::target::{PyProducerRef, PySenderTarget};
use super::{
    PyMessengerHandle, PyTopicMessage, duration_from_secs_f64, failure_from_py,
    future_into_py_unit, to_py_err,
};

// ---------------------------------------------------------------------------
// ActionFeedbackPublisher
// ---------------------------------------------------------------------------

/// Python wrapper for a per-goal feedback publisher used by action servers.
/// Vended by [`PyActionFeedbackPublisherFactory::declare`] once a goal is
/// accepted.
#[pyclass(name = "ActionFeedbackPublisher")]
pub struct PyActionFeedbackPublisher {
    inner: ActionFeedbackPublisher,
}

#[pymethods]
impl PyActionFeedbackPublisher {
    /// Publish a feedback payload. Must be non-empty: empty is reserved for
    /// the end-of-stream sentinel emitted by [`Self::publish_end`]. An empty
    /// `payload` raises `ValueError` at this FFI boundary so a Python caller
    /// cannot inadvertently close the feedback stream by publishing zero
    /// bytes.
    fn publish<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let publisher = self.inner.clone();
        let payload = NonEmptyPayload::try_new(Payload::from(payload)).map_err(|_| {
            PyValueError::new_err(
                "feedback payload must be non-empty; empty is reserved for publish_end()",
            )
        })?;
        future_into_py_unit(py, async move {
            publisher.publish(payload).await.map_err(to_py_err)?;
            Ok(())
        })
    }

    /// Publish the end-of-stream sentinel. Subscribers' next
    /// `on_next_feedback` call resolves with `ActionFeedbackChannelClosed`
    /// (a `RuntimeError` in Python). A producer that dies without sending
    /// the sentinel is detected via its liveliness token instead, surfacing
    /// as `ActionFeedbackProducerGone` (a `ConnectionError` in Python).
    fn publish_end<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let publisher = self.inner.clone();
        future_into_py_unit(py, async move {
            publisher.publish_end().await.map_err(to_py_err)?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// ActionFeedbackPublisherFactory
// ---------------------------------------------------------------------------

/// Python wrapper for the per-action feedback publisher factory. Returned as
/// a field of [`PyActionCreation`]; the codegen calls
/// [`Self::declare`] from inside `handle_goal_next_request` once a goal is
/// accepted, scoping the feedback topic to that single goal cycle.
#[pyclass(name = "ActionFeedbackPublisherFactory")]
pub struct PyActionFeedbackPublisherFactory {
    inner: ActionFeedbackPublisherFactory,
}

#[pymethods]
impl PyActionFeedbackPublisherFactory {
    /// Standard server-side entry point used by the Python codegen. Unwraps
    /// the wire envelope, declares a per-goal publisher scoped to the
    /// link_id the consumer targeted, and returns
    /// `(publisher, goal_id, user_payload)`.
    fn declare_from_wire<'py>(
        &self,
        py: Python<'py>,
        link_id: String,
        wire: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let factory = self.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let declared = factory
                .declare_from_wire(&link_id, Bytes::from(wire))
                .await
                .map_err(to_py_err)?;
            Ok((
                PyActionFeedbackPublisher {
                    inner: declared.publisher,
                },
                declared.goal_id,
                declared.user_payload.to_vec(),
            ))
        })
    }
}

// ---------------------------------------------------------------------------
// ActionResultReply
// ---------------------------------------------------------------------------

/// Python wrapper for the typed result reply returned by
/// `ActionMessenger.request_result`. The engine's result-outcome envelope is
/// decoded Rust-side, so Python reads the typed `status`, the raw result
/// `body`, the failure of a failed goal and the reason of a cancelled one
/// directly (no re-parsing of the framing).
#[pyclass(name = "ActionResultReply")]
pub struct PyActionResultReply {
    status: u8,
    body: Vec<u8>,
    failure: Option<Failure>,
    cancel_reason: Option<String>,
    instance_id: String,
    core_node: String,
}

impl PyActionResultReply {
    fn from_reply(reply: ActionResultReply) -> Self {
        let status = reply.outcome.status() as u8;
        let (body, failure, cancel_reason) = match reply.outcome {
            GoalOutcome::Completed(result) => (result.to_vec(), None, None),
            GoalOutcome::Cancelled { reason, result } => (result.to_vec(), None, Some(reason)),
            GoalOutcome::Failed(failure) => (Vec::new(), Some(failure), None),
            GoalOutcome::Abandoned | GoalOutcome::Expired => (Vec::new(), None, None),
        };
        Self {
            status,
            body,
            failure,
            cancel_reason,
            instance_id: reply.instance_id,
            core_node: reply.core_node,
        }
    }
}

#[pymethods]
impl PyActionResultReply {
    /// The terminal [`peppylib::messaging::ResultStatus`] as its `u8` tag
    /// (0=Completed, 1=Cancelled, 2=Abandoned, 3=Expired, 4=Failed).
    #[getter]
    fn status(&self) -> u8 {
        self.status
    }

    /// The raw user result payload of a completed or cancelled goal. Empty
    /// for the other statuses.
    #[getter]
    fn body<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.body)
    }

    /// The failure kind of a failed goal; `None` for the other statuses.
    #[getter]
    fn failure_kind(&self) -> Option<&str> {
        self.failure.as_ref().map(|failure| failure.kind().as_str())
    }

    /// The failure message of a failed goal; `None` for the other statuses.
    #[getter]
    fn failure_message(&self) -> Option<&str> {
        self.failure.as_ref().map(Failure::message)
    }

    /// Why a cancelled goal was cancelled; `None` for the other statuses.
    #[getter]
    fn cancel_reason(&self) -> Option<&str> {
        self.cancel_reason.as_deref()
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }
}

// ---------------------------------------------------------------------------
// ActionCancelReply
// ---------------------------------------------------------------------------

/// Python wrapper for the typed cancel reply returned by
/// `ActionMessenger.cancel_goal`. The framework cancel-ack is decoded Rust-side
/// (mirroring `request_result`), so Python reads the typed `state` tag directly.
#[pyclass(name = "ActionCancelReply")]
pub struct PyActionCancelReply {
    state: u8,
    instance_id: String,
    core_node: String,
}

#[pymethods]
impl PyActionCancelReply {
    /// The [`peppylib::messaging::CancelState`] as its `u8` tag
    /// (0=Signalled, 1=AlreadyTerminal, 2=Unknown).
    #[getter]
    fn state(&self) -> u8 {
        self.state
    }

    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }
}

// ---------------------------------------------------------------------------
// ActionGoalHandle
// ---------------------------------------------------------------------------

/// Python wrapper for a client-side goal handle returned by `send_goal`.
///
/// A clone of the underlying [`ActionWireSender`] is cached at construction so
/// that `cancel_goal` and `request_result` can proceed without locking the
/// mutex, which is only needed by `on_next_feedback` (mutates the subscription).
#[pyclass(name = "ActionGoalHandle")]
pub struct PyActionGoalHandle {
    pub(crate) inner: Arc<Mutex<ActionGoalHandle>>,
    refusal: Option<Failure>,
    goal_reply_body: Vec<u8>,
    instance_id: String,
    core_node: String,
    sender: ActionWireSender,
    goal_id: String,
}

#[pymethods]
impl PyActionGoalHandle {
    /// Whether the server admitted the goal. Decoded from the framework
    /// admission ack that wraps every goal reply.
    #[getter]
    fn accepted(&self) -> bool {
        self.refusal.is_none()
    }

    /// The failure kind the server refused the goal with; `None` for an
    /// accepted goal.
    #[getter]
    fn refusal_kind(&self) -> Option<&str> {
        self.refusal.as_ref().map(|failure| failure.kind().as_str())
    }

    /// The failure message the server refused the goal with; `None` for an
    /// accepted goal.
    #[getter]
    fn refusal_message(&self) -> Option<&str> {
        self.refusal.as_ref().map(Failure::message)
    }

    /// The producer's declared goal response payload, stripped of the
    /// admission ack. Empty when the action declares no goal response or the
    /// goal was refused.
    #[getter]
    fn goal_reply_body(&self) -> &[u8] {
        &self.goal_reply_body
    }

    /// The instance id of the producer that answered the goal. Identifies
    /// the discovered producer when `send_goal` ran with `target=None`.
    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The core node of the producer that answered the goal.
    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }

    /// Correlation ID generated by `send_goal` for this goal cycle.
    #[getter]
    fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// Wait for the next feedback message from the action server.
    ///
    /// Raises `RuntimeError` when the stream ends cleanly (the server
    /// published the end-of-stream sentinel) and `ConnectionError` when the
    /// pinned producer instance disappeared without closing the stream
    /// (`ActionFeedbackProducerGone`); `request_result` then resolves with
    /// status 2 (Abandoned). Raises `peppylib.FeedbackOverflowError` once
    /// the caller fell behind by more unread messages than its bound: the
    /// handle cancelled the goal.
    fn on_next_feedback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut handle = inner.lock().await;
            let msg = handle.on_next_feedback().await.map_err(to_py_err)?;
            Ok(PyTopicMessage::from(msg))
        })
    }
}

// ---------------------------------------------------------------------------
// ActionCreation
// ---------------------------------------------------------------------------

/// Python wrapper for the server-side action components returned by `expose`.
#[pyclass(name = "ActionCreation")]
pub struct PyActionCreation {
    goal_service: Arc<Mutex<ServiceEndpoint>>,
    cancel_service: Arc<Mutex<ServiceEndpoint>>,
    feedback_publisher_factory: ActionFeedbackPublisherFactory,
    result_service: Arc<Mutex<ServiceEndpoint>>,
    /// Producer-instance liveliness advertisement. Held (not Python-visible)
    /// so the producer stays observable as alive for exactly as long as this
    /// creation (and with it the action endpoint) exists.
    _liveliness_token: peppylib::messaging::LivelinessToken,
}

#[pymethods]
impl PyActionCreation {
    #[getter]
    fn goal_service(&self) -> PyServiceEndpoint {
        PyServiceEndpoint {
            inner: Arc::clone(&self.goal_service),
        }
    }

    #[getter]
    fn cancel_service(&self) -> PyServiceEndpoint {
        PyServiceEndpoint {
            inner: Arc::clone(&self.cancel_service),
        }
    }

    #[getter]
    fn feedback_publisher_factory(&self) -> PyActionFeedbackPublisherFactory {
        PyActionFeedbackPublisherFactory {
            inner: self.feedback_publisher_factory.clone(),
        }
    }

    #[getter]
    fn result_service(&self) -> PyServiceEndpoint {
        PyServiceEndpoint {
            inner: Arc::clone(&self.result_service),
        }
    }
}

// ---------------------------------------------------------------------------
// ActionMessenger
// ---------------------------------------------------------------------------

/// Python wrapper for ActionMessenger (goal / feedback / result / cancel pattern).
#[pyclass(name = "ActionMessenger")]
pub struct PyActionMessenger;

#[pymethods]
impl PyActionMessenger {
    /// Expose an action server, returning the goal, cancel, result services and feedback publisher.
    ///
    /// Pass `SenderTarget.node(name, tag)` for nodes or
    /// `SenderTarget.contract(name, tag)` for contract-implemented actions.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_identity, as_action_name))]
    fn expose<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_identity: PySenderTarget,
        as_action_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_identity = as_identity.into_inner();
        crate::py_future::future_into_py(py, async move {
            let creation = ActionMessenger::expose(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_identity,
                &as_action_name,
            )
            .await
            .map_err(to_py_err)?;

            Ok(PyActionCreation {
                goal_service: Arc::new(Mutex::new(creation.goal_service)),
                cancel_service: Arc::new(Mutex::new(creation.cancel_service)),
                feedback_publisher_factory: creation.feedback_publisher_factory,
                result_service: Arc::new(Mutex::new(creation.result_service)),
                _liveliness_token: creation.liveliness_token,
            })
        })
    }

    /// Send a goal to an action server. The framework generates a fresh
    /// `goal_id`, wraps `user_payload`, and exposes the id on the returned
    /// handle via `goal_id`.
    ///
    /// Pass `SenderTarget.node(name, tag)` for nodes or
    /// `SenderTarget.contract(name, tag)` for contract-implemented actions.
    /// `target` is the producer's full `(core_node, instance_id)` pair:
    /// `Some` pins it (no discovery), `None` is a genuine wildcard
    /// (discover-then-pin). Generated `fire_goal` wrappers pass their
    /// explicit `target` parameter, a membership-checked member of the
    /// slot's bound set.
    #[staticmethod]
    ///
    /// The handle keeps up to `feedback_bound` unread feedback messages
    /// (`None` takes peppylib's default); past it, it cancels the goal and
    /// `on_next_feedback` raises `peppylib.FeedbackOverflowError`.
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, to_target, to_action_name, target=None, user_payload=vec![], feedback_bound=None, goal_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn send_goal<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_action_name: String,
        target: Option<PyProducerRef>,
        user_payload: Vec<u8>,
        feedback_bound: Option<usize>,
        goal_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let goal_timeout = duration_from_secs_f64("goal_timeout_secs", goal_timeout_secs)?;
        let feedback_bound = match feedback_bound {
            None => DEFAULT_FEEDBACK_BOUND,
            Some(bound) => FeedbackBound::new(NonZeroUsize::new(bound).ok_or_else(|| {
                PyValueError::new_err("feedback_bound must be one message or more")
            })?),
        };
        let to_target = to_target.into_inner();
        let handle = messenger.inner.clone();
        crate::py_future::future_into_py(py, async move {
            let target = target.map(PyProducerRef::into_inner);
            let goal_handle = ActionMessenger::send_goal(
                &handle,
                &as_core_node,
                &as_instance_id,
                to_target,
                &to_action_name,
                target.as_ref(),
                Payload::from(user_payload),
                feedback_bound,
                goal_timeout,
            )
            .await
            .map_err(to_py_err)?;

            // Cache the decoded goal reply and the wire sender so cancel_goal /
            // request_result never need to lock the mutex behind ActionGoalHandle.
            let reply = goal_handle.goal_reply();
            let (refusal, goal_reply_body) = match &reply.answer {
                GoalAnswer::Accepted(response) => (None, response.as_ref().to_vec()),
                GoalAnswer::Refused(failure) => (Some(failure.clone()), Vec::new()),
            };
            let instance_id = reply.instance_id.clone();
            let core_node = reply.core_node.clone();
            let goal_id = goal_handle.goal_id().to_string();
            let sender = goal_handle.sender().clone();

            Ok(PyActionGoalHandle {
                inner: Arc::new(Mutex::new(goal_handle)),
                refusal,
                goal_reply_body,
                instance_id,
                core_node,
                sender,
                goal_id,
            })
        })
    }

    /// Cancel an active goal.
    ///
    /// Does not acquire the goal handle mutex, so this can run concurrently
    /// with `on_next_feedback` without blocking.
    #[staticmethod]
    fn cancel_goal<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        goal_handle: &PyActionGoalHandle,
        cancel_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let cancel_timeout = duration_from_secs_f64("cancel_timeout_secs", cancel_timeout_secs)?;
        let handle = messenger.inner.clone();
        let sender = goal_handle.sender.clone();
        let goal_id = goal_handle.goal_id.clone();
        crate::py_future::future_into_py(py, async move {
            let response =
                ActionMessenger::cancel_with_sender(&handle, &sender, &goal_id, cancel_timeout)
                    .await
                    .map_err(to_py_err)?;
            let instance_id = response.instance_id().to_string();
            let core_node = response.core_node().to_string();
            // Decode the framework cancel-ack Rust-side (mirroring request_result),
            // so Python reads a typed `state` tag without re-parsing the capnp.
            let state = decode_cancel_ack(response.payload_bytes().as_ref()).map_err(to_py_err)?;
            Ok(PyActionCancelReply {
                state: state as u8,
                instance_id,
                core_node,
            })
        })
    }

    /// Request the final result of a completed goal.
    ///
    /// Does not acquire the goal handle mutex, so this can run concurrently
    /// with `on_next_feedback` without blocking.
    #[staticmethod]
    fn request_result<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        goal_handle: &PyActionGoalHandle,
        result_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let result_timeout = duration_from_secs_f64("result_timeout_secs", result_timeout_secs)?;
        let handle = messenger.inner.clone();
        let sender = goal_handle.sender.clone();
        let goal_id = goal_handle.goal_id.clone();
        crate::py_future::future_into_py(py, async move {
            let reply = ActionMessenger::request_result_with_sender(
                &handle,
                &sender,
                &goal_id,
                result_timeout,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyActionResultReply::from_reply(reply))
        })
    }

    /// Check whether an action server is reachable. `target` is the
    /// producer's full `(core_node, instance_id)` pair (`None` probes any
    /// matching producer).
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_core_node, as_instance_id, to_target, to_action_name, target=None))]
    fn is_reachable<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_action_name: String,
        target: Option<PyProducerRef>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let to_target = to_target.into_inner();
        crate::py_future::future_into_py(py, async move {
            let target = target.map(PyProducerRef::into_inner);
            let reachable = ActionMessenger::is_reachable(
                &handle,
                &bound_core_node,
                &as_instance_id,
                to_target,
                &to_action_name,
                target.as_ref(),
            )
            .await
            .map_err(to_py_err)?;
            Ok(reachable)
        })
    }
}

// ---------------------------------------------------------------------------
// ConcurrentAction / PendingGoal / GoalContext (concurrent-goal engine)
// ---------------------------------------------------------------------------
//
// These are thin 1:1 wrappers over the peppylib engine. All routing,
// cancel/result correlation, the result rendezvous, the background loops, and
// the cancel-ack encoding live in Rust (peppylib), so Python and Rust servers
// behave identically. Only the per-action Cap'n Proto encode/decode lives in
// the generated Python, and it crosses this boundary as plain `bytes`.

/// Python wrapper for [`peppylib::messaging::ConcurrentAction`].
#[pyclass(name = "ConcurrentAction")]
pub struct PyConcurrentAction {
    inner: Arc<Mutex<ConcurrentAction>>,
    /// The engine's sender grace, set without the engine's lock, which
    /// `recv_next_goal` holds while it waits for a goal.
    sender_grace: SenderGrace,
}

#[pymethods]
impl PyConcurrentAction {
    /// Expose an action server and start its concurrent engine.
    ///
    /// `has_feedback` must reflect whether the action declares a feedback
    /// topic. Pass `SenderTarget.node(name, tag)` for nodes or
    /// `SenderTarget.contract(name, tag)` for contract-implemented actions.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, as_identity, as_action_name, has_feedback))]
    fn expose<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_identity: PySenderTarget,
        as_action_name: String,
        has_feedback: bool,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let as_identity = as_identity.into_inner();
        crate::py_future::future_into_py(py, async move {
            let action = ConcurrentAction::expose(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_identity,
                &as_action_name,
                has_feedback,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyConcurrentAction {
                sender_grace: action.sender_grace(),
                inner: Arc::new(Mutex::new(action)),
            })
        })
    }

    /// Set the sender grace of the action, in seconds: how long the engine
    /// keeps the goals of a sender that is gone before it sends them their
    /// cancel signal. Zero by default.
    fn set_sender_grace<'py>(&self, py: Python<'py>, seconds: f64) -> PyResult<Bound<'py, PyAny>> {
        let grace = duration_from_secs_f64("seconds", seconds)?;
        self.sender_grace.set(grace);
        future_into_py_unit(py, async move { Ok(()) })
    }

    /// Wait for the next goal request, returning a [`PyPendingGoal`] or `None`
    /// when the goal service stream has closed.
    fn recv_next_goal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let mut action = inner.lock().await;
            let pending = action.recv_next_goal().await.map_err(to_py_err)?;
            Ok(pending.map(|pending| {
                let goal_id = pending.goal_id().to_string();
                let core_node = pending.core_node().to_string();
                let instance_id = pending.instance_id().to_string();
                let request_bytes = pending.request_bytes().to_vec();
                PyPendingGoal {
                    inner: Arc::new(Mutex::new(Some(pending))),
                    goal_id,
                    core_node,
                    instance_id,
                    request_bytes,
                }
            }))
        })
    }
}

/// Python wrapper for [`peppylib::messaging::PendingGoal`]. `accept`/`refuse`
/// consume the underlying goal, so it is held behind an `Option` and taken on
/// first use.
#[pyclass(name = "PendingGoal")]
pub struct PyPendingGoal {
    inner: Arc<Mutex<Option<PendingGoal>>>,
    goal_id: String,
    core_node: String,
    instance_id: String,
    request_bytes: Vec<u8>,
}

#[pymethods]
impl PyPendingGoal {
    /// The client-generated correlation id for this goal.
    #[getter]
    fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// The core node of the client that sent this goal.
    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }

    /// The instance id of the client that sent this goal.
    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// The envelope-stripped goal request payload, ready to decode.
    #[getter]
    fn request_bytes(&self) -> &[u8] {
        &self.request_bytes
    }

    /// Accept the goal, replying with the encoded `GoalResponse` bytes, and
    /// return the [`PyGoalContext`] that drives it.
    fn accept<'py>(&self, py: Python<'py>, response: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        crate::py_future::future_into_py(py, async move {
            let pending =
                inner.lock().await.take().ok_or_else(|| {
                    PyValueError::new_err("PendingGoal already accepted or refused")
                })?;
            let ctx = pending
                .accept(Payload::from(response))
                .await
                .map_err(to_py_err)?;
            Ok(PyGoalContext {
                inner: Arc::new(ctx),
            })
        })
    }

    /// Refuse the goal with the failure kind `kind` and `message`. No
    /// context is produced. Raises `ValueError` for a text that is not a
    /// failure kind name.
    fn refuse<'py>(
        &self,
        py: Python<'py>,
        kind: String,
        message: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let failure = failure_from_py(&kind, message)?;
        let inner = Arc::clone(&self.inner);
        future_into_py_unit(py, async move {
            let pending =
                inner.lock().await.take().ok_or_else(|| {
                    PyValueError::new_err("PendingGoal already accepted or refused")
                })?;
            pending.refuse(failure).await.map_err(to_py_err)?;
            Ok(())
        })
    }
}

/// Python wrapper for [`peppylib::messaging::GoalContext`]. All methods take
/// `&self`, so it can be shared across asyncio tasks; cancellation, completion,
/// and feedback are all handled by the Rust engine.
///
/// `weakref` so `peppylib.testing`'s mock action server can track live goal
/// contexts without keeping them alive: a strong registry would defer the
/// abandon-on-drop transition a test triggers by releasing its goal handle.
#[pyclass(name = "GoalContext", weakref)]
pub struct PyGoalContext {
    inner: Arc<GoalContext>,
}

#[pymethods]
impl PyGoalContext {
    /// The client-generated correlation id for this goal.
    #[getter]
    fn goal_id(&self) -> &str {
        self.inner.goal_id()
    }

    /// The envelope-stripped goal request payload.
    #[getter]
    fn request_bytes(&self) -> &[u8] {
        self.inner.request_bytes()
    }

    /// Publish a feedback message on this goal's stream. Empty payloads are
    /// rejected (reserved for the framework's end-of-stream sentinel).
    fn publish_feedback<'py>(
        &self,
        py: Python<'py>,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        let payload = NonEmptyPayload::try_new(Payload::from(payload)).map_err(|_| {
            PyValueError::new_err(
                "feedback payload must be non-empty; empty is reserved for end-of-stream",
            )
        })?;
        future_into_py_unit(py, async move {
            inner.publish_feedback(payload).await.map_err(to_py_err)?;
            Ok(())
        })
    }

    /// Resolves when this goal receives its cancel signal: a cancel request
    /// of its caller, or the end of its sender.
    fn cancel_signal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py_unit(py, async move {
            inner.cancel_signal().await;
            Ok(())
        })
    }

    /// Whether this goal received its cancel signal.
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Why this goal received its cancel signal: `"caller"` or
    /// `"sender_gone"`; `None` before it did.
    #[getter]
    fn cancel_cause(&self) -> Option<&'static str> {
        self.inner.cancel_cause().map(|cause| match cause {
            CancelCause::Caller => "caller",
            CancelCause::SenderGone => "sender_gone",
        })
    }

    /// End the goal with the status completed and its result: a success.
    /// Idempotent: the first terminal call wins.
    fn complete<'py>(&self, py: Python<'py>, result: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py_unit(py, async move {
            inner
                .complete(Payload::from(result))
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }

    /// End the goal with the status failed: the failure kind `kind` and
    /// `message`, and no result. Raises `ValueError` for a text that is not
    /// a failure kind name.
    fn fail<'py>(
        &self,
        py: Python<'py>,
        kind: String,
        message: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let failure = failure_from_py(&kind, message)?;
        let inner = Arc::clone(&self.inner);
        future_into_py_unit(py, async move {
            inner.fail(failure).await.map_err(to_py_err)?;
            Ok(())
        })
    }

    /// End the goal with the status cancelled, for `reason`, with its
    /// result.
    fn complete_cancelled<'py>(
        &self,
        py: Python<'py>,
        reason: String,
        result: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        future_into_py_unit(py, async move {
            inner
                .complete_cancelled(reason, Payload::from(result))
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }

    /// Testing-only: disarm the drop-time close, so releasing this context
    /// performs no `Abandoned` transition and emits no feedback end sentinel.
    /// Exists for mock action servers simulating producer loss (see the Rust
    /// `GoalContext::disarm_close_on_drop`); the wheel always ships it, the
    /// same accepted trade-off as the inert generated mock subpackages. Never
    /// call it from production node code: consumers rely on the abandon
    /// transition it suppresses.
    fn disarm_close_on_drop(&self) {
        self.inner.disarm_close_on_drop();
    }
}

// ---------------------------------------------------------------------------
// Wire-format helpers for goal payload envelopes
// ---------------------------------------------------------------------------

/// Wrap a user goal payload with a length-prefixed `goal_id` for transport.
/// Mirrors `peppylib::messaging::wrap_goal_payload` so Python codegen can
/// produce the same wire format as Rust codegen.
#[pyfunction]
fn wrap_goal_payload(goal_id: String, user_payload: Vec<u8>) -> PyResult<Vec<u8>> {
    let payload =
        peppylib::messaging::wrap_goal_payload(&goal_id, &user_payload).map_err(to_py_err)?;
    Ok(payload.as_ref().to_vec())
}

/// Unwrap an action goal envelope; returns `(goal_id, user_payload_bytes)`.
#[pyfunction]
fn unwrap_goal_payload(wire: Vec<u8>) -> PyResult<(String, Vec<u8>)> {
    let (goal_id, body) = peppylib::messaging::unwrap_goal_payload(&wire).map_err(to_py_err)?;
    Ok((goal_id.to_string(), body.to_vec()))
}

/// Generate a unique `goal_id` for use with `ActionMessenger.send_goal` and
/// per-goal feedback scoping. Mirrors `peppylib::messaging::generate_goal_id`.
#[pyfunction]
fn generate_goal_id() -> String {
    peppylib::messaging::generate_goal_id()
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register the actions submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let actions_module = PyModule::new(parent_module.py(), "actions")?;
    actions_module.add_class::<PyActionMessenger>()?;
    actions_module.add_class::<PyActionResultReply>()?;
    actions_module.add_class::<PyActionCancelReply>()?;
    actions_module.add_class::<PyActionGoalHandle>()?;
    actions_module.add_class::<PyActionCreation>()?;
    actions_module.add_class::<PyActionFeedbackPublisher>()?;
    actions_module.add_class::<PyActionFeedbackPublisherFactory>()?;
    actions_module.add_class::<PyConcurrentAction>()?;
    actions_module.add_class::<PyPendingGoal>()?;
    actions_module.add_class::<PyGoalContext>()?;
    actions_module.add_function(wrap_pyfunction!(wrap_goal_payload, &actions_module)?)?;
    actions_module.add_function(wrap_pyfunction!(unwrap_goal_payload, &actions_module)?)?;
    actions_module.add_function(wrap_pyfunction!(generate_goal_id, &actions_module)?)?;
    parent_module.add_submodule(&actions_module)?;
    Ok(())
}
