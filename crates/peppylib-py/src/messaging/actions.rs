use peppylib::messaging::{
    ActionGoalHandle, ActionMessenger, ActionServer, ActionWireSender, GoalContext,
    NonEmptyPayload, ServiceRequestContext, ServiceResponder,
};
use peppylib::types::Payload;
use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::iface::PySenderTarget;
use super::{PyMessengerHandle, PyTopicMessage, duration_from_secs_f64, to_py_err};
use crate::config::PyQoSProfile;

// ---------------------------------------------------------------------------
// GoalContext
// ---------------------------------------------------------------------------

/// Python wrapper for a per-goal [`GoalContext`]. Returned by
/// [`PyActionGoalRequest::accept`]; it owns that goal's feedback stream, cancel
/// signal, and result delivery, so a server can drive many goals concurrently
/// by moving one of these into each worker coroutine.
#[pyclass(name = "GoalContext")]
pub struct PyGoalContext {
    inner: Arc<GoalContext>,
}

#[pymethods]
impl PyGoalContext {
    /// The client-generated id of this goal.
    #[getter]
    fn goal_id(&self) -> &str {
        self.inner.goal_id()
    }

    /// Envelope-stripped goal request bytes, ready for the typed decoder.
    #[getter]
    fn request_bytes<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, self.inner.request_bytes())
    }

    /// Whether a cancel for this goal has already been received.
    fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Resolves when a cancel request for this goal arrives. Idempotent and
    /// resolves immediately if the cancel already arrived, so it is safe to
    /// race against the worker's own coroutine (e.g. with `asyncio.wait`).
    fn cancel_signal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.cancel_signal().await;
            Ok(())
        })
    }

    /// Publish one feedback message on this goal's stream. Empty payloads are
    /// rejected because the empty payload is the end-of-stream sentinel that
    /// [`Self::complete`] emits; passing zero bytes raises `ValueError`.
    fn publish_feedback<'py>(
        &self,
        py: Python<'py>,
        payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        let payload = NonEmptyPayload::try_new(Payload::from(payload)).map_err(|_| {
            PyValueError::new_err(
                "feedback payload must be non-empty; empty is reserved for the end-of-stream sentinel",
            )
        })?;
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner.publish_feedback(payload).await.map_err(to_py_err)?;
            Ok(())
        })
    }

    /// Deliver the final result for this goal. Closes the feedback stream
    /// first, then rendezvous with the client's `get_result` by `goal_id`.
    /// Idempotent: a second call is a no-op.
    fn complete<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            inner
                .complete(Payload::from(payload))
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// ActionGoalRequest
// ---------------------------------------------------------------------------

/// A goal awaiting an accept/reject decision, returned by
/// [`PyActionServer::recv_next_goal`]. Holds the request context and its
/// responder until the server decides; [`Self::accept`] registers the routing
/// slot and replies, [`Self::reject`] replies with an error.
#[pyclass(name = "ActionGoalRequest")]
pub struct PyActionGoalRequest {
    server: Arc<Mutex<ActionServer>>,
    state: Arc<Mutex<Option<(ServiceRequestContext, ServiceResponder)>>>,
    payload: Vec<u8>,
    instance_id: String,
    core_node: String,
}

#[pymethods]
impl PyActionGoalRequest {
    /// The raw goal envelope (length-prefixed `goal_id` + user payload). Strip
    /// it with `actions.unwrap_goal_payload` to recover the user payload.
    #[getter]
    fn payload<'py>(&self, py: Python<'py>) -> Bound<'py, PyBytes> {
        PyBytes::new(py, &self.payload)
    }

    /// Caller instance id that fired this goal.
    #[getter]
    fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// Caller core node that fired this goal.
    #[getter]
    fn core_node(&self) -> &str {
        &self.core_node
    }

    /// Accept this goal. Registers its routing slot (so a fast follow-up
    /// cancel/result cannot miss it), replies with `response_payload`, and
    /// returns the per-goal [`GoalContext`]. Raises if already decided.
    fn accept<'py>(
        &self,
        py: Python<'py>,
        response_payload: Vec<u8>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let server = Arc::clone(&self.server);
        let state = Arc::clone(&self.state);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (context, responder) = state.lock().await.take().ok_or_else(|| {
                PyValueError::new_err("goal request already accepted or rejected")
            })?;
            // Register before replying accepted: the client only sends
            // cancel/result after it sees acceptance, so the slot must exist
            // first or a fast follow-up could miss it.
            let goal_ctx = {
                let server = server.lock().await;
                server.register_goal(&context).await.map_err(to_py_err)?
            };
            responder
                .respond(Payload::from(response_payload))
                .await
                .map_err(to_py_err)?;
            Ok(PyGoalContext {
                inner: Arc::new(goal_ctx),
            })
        })
    }

    /// Reject this goal with an error message; the client's `fire_goal` fails
    /// with a service error. Raises if already decided.
    fn reject<'py>(&self, py: Python<'py>, reason: String) -> PyResult<Bound<'py, PyAny>> {
        let state = Arc::clone(&self.state);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let (_context, responder) = state.lock().await.take().ok_or_else(|| {
                PyValueError::new_err("goal request already accepted or rejected")
            })?;
            responder.respond_error(reason).await.map_err(to_py_err)?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// ActionServer
// ---------------------------------------------------------------------------

/// Python wrapper for the concurrent [`ActionServer`] returned by
/// [`PyActionMessenger::expose`]. Background pumps route cancel/result requests
/// to the right goal by `goal_id`; the caller drives the accept loop via
/// [`Self::recv_next_goal`].
#[pyclass(name = "ActionServer")]
pub struct PyActionServer {
    inner: Arc<Mutex<ActionServer>>,
}

#[pymethods]
impl PyActionServer {
    /// Wait for the next goal. Returns an [`PyActionGoalRequest`] to accept or
    /// reject, or `None` when the action's goal service has closed.
    fn recv_next_goal<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let recv = {
                let mut server = inner.lock().await;
                server.recv_next_goal().await.map_err(to_py_err)?
            };
            let Some((context, responder)) = recv else {
                return Ok(None);
            };
            let (payload, instance_id, core_node) = {
                let message = context.message();
                (
                    message.payload().to_vec(),
                    message.instance_id().to_string(),
                    message.core_node().to_string(),
                )
            };
            Ok(Some(PyActionGoalRequest {
                server: Arc::clone(&inner),
                state: Arc::new(Mutex::new(Some((context, responder)))),
                payload,
                instance_id,
                core_node,
            }))
        })
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
    goal_response_cache: PyTopicMessage,
    sender: ActionWireSender,
    goal_id: String,
}

#[pymethods]
impl PyActionGoalHandle {
    /// The initial response received when the goal was accepted.
    #[getter]
    fn goal_response(&self) -> PyTopicMessage {
        self.goal_response_cache.clone()
    }

    /// Correlation ID generated by `send_goal` for this goal cycle.
    #[getter]
    fn goal_id(&self) -> &str {
        &self.goal_id
    }

    /// Wait for the next feedback message from the action server.
    fn on_next_feedback<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let inner = Arc::clone(&self.inner);
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let mut handle = inner.lock().await;
            let msg = handle.on_next_feedback().await.map_err(to_py_err)?;
            Ok(PyTopicMessage::from(msg))
        })
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
    /// Expose an action server. Returns an [`PyActionServer`] whose
    /// `recv_next_goal` drives the accept loop; background pumps route
    /// cancel/result requests to each goal by `goal_id`.
    ///
    /// Pass `SenderTarget.node(name, tag)` for nodes or
    /// `SenderTarget.interface(name, tag)` for `conforms_to` actions.
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
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let server = ActionMessenger::expose(
                &handle,
                &as_core_node,
                &as_instance_id,
                as_identity,
                &as_action_name,
            )
            .await
            .map_err(to_py_err)?;

            Ok(PyActionServer {
                inner: Arc::new(Mutex::new(server)),
            })
        })
    }

    /// Send a goal to an action server. The framework generates a fresh
    /// `goal_id`, wraps `user_payload`, and exposes the id on the returned
    /// handle via `goal_id`.
    ///
    /// Pass `SenderTarget.node(name, tag)` for nodes or
    /// `SenderTarget.interface(name, tag)` for `conforms_to` actions.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, to_target, to_action_name, target_core_node=None, target_instance_id=None, user_payload=vec![], feedback_qos=PyQoSProfile::Reliable, goal_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn send_goal<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_action_name: String,
        target_core_node: Option<String>,
        target_instance_id: Option<String>,
        user_payload: Vec<u8>,
        feedback_qos: PyQoSProfile,
        goal_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let goal_timeout = duration_from_secs_f64("goal_timeout_secs", goal_timeout_secs)?;
        let to_target = to_target.into_inner();
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let goal_handle = ActionMessenger::send_goal(
                &handle,
                &as_core_node,
                &as_instance_id,
                to_target,
                &to_action_name,
                target_core_node.as_deref(),
                target_instance_id.as_deref(),
                Payload::from(user_payload),
                feedback_qos.into(),
                goal_timeout,
            )
            .await
            .map_err(to_py_err)?;

            // Cache goal_response and the wire sender so cancel_goal /
            // request_result never need to lock the mutex behind ActionGoalHandle.
            let resp = goal_handle.goal_response();
            let goal_response_cache = PyTopicMessage {
                payload: resp.payload().to_vec(),
                instance_id: resp.instance_id().to_string(),
                core_node: resp.core_node().to_string(),
            };
            let goal_id = goal_handle.goal_id().to_string();
            let sender = goal_handle.sender().clone();

            Ok(PyActionGoalHandle {
                inner: Arc::new(Mutex::new(goal_handle)),
                goal_response_cache,
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
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response =
                ActionMessenger::cancel_with_sender(&handle, &sender, &goal_id, cancel_timeout)
                    .await
                    .map_err(to_py_err)?;
            Ok(PyTopicMessage::from(response))
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
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = ActionMessenger::request_result_with_sender(
                &handle,
                &sender,
                &goal_id,
                result_timeout,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicMessage::from(response))
        })
    }

    /// Check whether an action server is reachable.
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_core_node, as_instance_id, to_target, to_action_name, target_core_node=None, target_instance_id=None))]
    #[allow(clippy::too_many_arguments)]
    fn is_reachable<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_core_node: String,
        as_instance_id: String,
        to_target: PySenderTarget,
        to_action_name: String,
        target_core_node: Option<String>,
        target_instance_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        let to_target = to_target.into_inner();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let reachable = ActionMessenger::is_reachable(
                &handle,
                &bound_core_node,
                &as_instance_id,
                to_target,
                &to_action_name,
                target_core_node.as_deref(),
                target_instance_id.as_deref(),
            )
            .await
            .map_err(to_py_err)?;
            Ok(reachable)
        })
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

/// Decode the SDK-owned cancel acknowledgement returned by `cancel_goal`.
/// Returns `True` if the goal was in flight when the cancel arrived, `False`
/// otherwise. The server side ack is produced by the framework's cancel pump,
/// not a user handler, so consumers decode it with this rather than a capnp
/// codec.
#[pyfunction]
fn decode_cancel_ack(payload: Vec<u8>) -> PyResult<bool> {
    peppylib::messaging::decode_cancel_ack(&payload).map_err(to_py_err)
}

// ---------------------------------------------------------------------------
// Module registration
// ---------------------------------------------------------------------------

/// Register the actions submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let actions_module = PyModule::new(parent_module.py(), "actions")?;
    actions_module.add_class::<PyActionMessenger>()?;
    actions_module.add_class::<PyActionGoalHandle>()?;
    actions_module.add_class::<PyActionServer>()?;
    actions_module.add_class::<PyActionGoalRequest>()?;
    actions_module.add_class::<PyGoalContext>()?;
    actions_module.add_function(wrap_pyfunction!(wrap_goal_payload, &actions_module)?)?;
    actions_module.add_function(wrap_pyfunction!(unwrap_goal_payload, &actions_module)?)?;
    actions_module.add_function(wrap_pyfunction!(generate_goal_id, &actions_module)?)?;
    actions_module.add_function(wrap_pyfunction!(decode_cancel_ack, &actions_module)?)?;
    parent_module.add_submodule(&actions_module)?;
    Ok(())
}
