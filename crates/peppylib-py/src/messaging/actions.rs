use peppylib::messaging::{ActionGoalHandle, ActionMessenger, ServiceEndpoint, TopicPublisher};
use peppylib::types::Payload;
use pyo3::prelude::*;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::services::PyServiceEndpoint;
use super::{PyMessengerHandle, PyTopicMessage, duration_from_secs_f64, to_py_err};
use crate::config::PyQoSProfile;

// ---------------------------------------------------------------------------
// TopicPublisher
// ---------------------------------------------------------------------------

/// Python wrapper for a feedback publisher used by action servers.
#[pyclass(name = "TopicPublisher")]
pub struct PyTopicPublisher {
    inner: TopicPublisher,
}

#[pymethods]
impl PyTopicPublisher {
    /// Publish a payload on the feedback topic.
    fn publish<'py>(&self, py: Python<'py>, payload: Vec<u8>) -> PyResult<Bound<'py, PyAny>> {
        let publisher = self.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            publisher
                .publish(Payload::from(payload))
                .await
                .map_err(to_py_err)?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------------
// ActionGoalHandle
// ---------------------------------------------------------------------------

/// Python wrapper for a client-side goal handle returned by `send_goal`.
///
/// Immutable identifying fields are cached at construction so that
/// `cancel_goal` and `request_result` can proceed without locking the mutex,
/// which is only needed by `on_next_feedback` (mutates the subscription).
#[pyclass(name = "ActionGoalHandle")]
pub struct PyActionGoalHandle {
    pub(crate) inner: Arc<Mutex<ActionGoalHandle>>,
    goal_response_cache: PyTopicMessage,
    core_node: String,
    instance_id: String,
    node_name: String,
    action_name: String,
    target_core_node: Option<String>,
    target_instance_id: Option<String>,
}

#[pymethods]
impl PyActionGoalHandle {
    /// The initial response received when the goal was accepted.
    #[getter]
    fn goal_response(&self) -> PyTopicMessage {
        self.goal_response_cache.clone()
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
// ActionCreation
// ---------------------------------------------------------------------------

/// Python wrapper for the server-side action components returned by `expose`.
#[pyclass(name = "ActionCreation")]
pub struct PyActionCreation {
    goal_service: Arc<Mutex<ServiceEndpoint>>,
    cancel_service: Arc<Mutex<ServiceEndpoint>>,
    feedback_publisher: TopicPublisher,
    result_service: Arc<Mutex<ServiceEndpoint>>,
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
    fn feedback_publisher(&self) -> PyTopicPublisher {
        PyTopicPublisher {
            inner: self.feedback_publisher.clone(),
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
    #[staticmethod]
    fn expose<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        as_node_name: String,
        as_action_name: String,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let creation = ActionMessenger::expose(
                &handle,
                &as_core_node,
                &as_instance_id,
                &as_node_name,
                &as_action_name,
            )
            .await
            .map_err(to_py_err)?;

            Ok(PyActionCreation {
                goal_service: Arc::new(Mutex::new(creation.goal_service)),
                cancel_service: Arc::new(Mutex::new(creation.cancel_service)),
                feedback_publisher: creation.feedback_publisher,
                result_service: Arc::new(Mutex::new(creation.result_service)),
            })
        })
    }

    /// Send a goal to an action server.
    #[staticmethod]
    #[pyo3(signature = (messenger, as_core_node, as_instance_id, to_node_name, to_action_name, target_core_node=None, target_instance_id=None, goal_payload=vec![], feedback_qos=PyQoSProfile::Reliable, goal_timeout_secs=2.0))]
    #[allow(clippy::too_many_arguments)]
    fn send_goal<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        as_core_node: String,
        as_instance_id: String,
        to_node_name: String,
        to_action_name: String,
        target_core_node: Option<String>,
        target_instance_id: Option<String>,
        goal_payload: Vec<u8>,
        feedback_qos: PyQoSProfile,
        goal_timeout_secs: f64,
    ) -> PyResult<Bound<'py, PyAny>> {
        let goal_timeout = duration_from_secs_f64("goal_timeout_secs", goal_timeout_secs)?;
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let goal_handle = ActionMessenger::send_goal(
                &handle,
                &as_core_node,
                &as_instance_id,
                &to_node_name,
                &to_action_name,
                target_core_node.as_deref(),
                target_instance_id.as_deref(),
                Payload::from(goal_payload),
                feedback_qos.into(),
                goal_timeout,
            )
            .await
            .map_err(to_py_err)?;

            // Cache goal_response and identifying fields before wrapping in
            // Arc<Mutex<>> so cancel_goal/request_result never need the lock.
            let resp = goal_handle.goal_response();
            let goal_response_cache = PyTopicMessage {
                key_expr: resp.key_expr().to_string(),
                payload: resp.payload().to_vec(),
                instance_id: resp.instance_id().to_string(),
                core_node: resp.core_node().to_string(),
            };

            Ok(PyActionGoalHandle {
                inner: Arc::new(Mutex::new(goal_handle)),
                goal_response_cache,
                core_node: as_core_node,
                instance_id: as_instance_id,
                node_name: to_node_name,
                action_name: to_action_name,
                target_core_node,
                target_instance_id,
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
        let core_node = goal_handle.core_node.clone();
        let instance_id = goal_handle.instance_id.clone();
        let node_name = goal_handle.node_name.clone();
        let action_name = goal_handle.action_name.clone();
        let target_core_node = goal_handle.target_core_node.clone();
        let target_instance_id = goal_handle.target_instance_id.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = ActionMessenger::cancel_goal_with(
                &handle,
                &core_node,
                &instance_id,
                &node_name,
                &action_name,
                target_core_node.as_deref(),
                target_instance_id.as_deref(),
                cancel_timeout,
            )
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
        let core_node = goal_handle.core_node.clone();
        let instance_id = goal_handle.instance_id.clone();
        let node_name = goal_handle.node_name.clone();
        let action_name = goal_handle.action_name.clone();
        let target_core_node = goal_handle.target_core_node.clone();
        let target_instance_id = goal_handle.target_instance_id.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let response = ActionMessenger::request_result_with(
                &handle,
                &core_node,
                &instance_id,
                &node_name,
                &action_name,
                target_core_node.as_deref(),
                target_instance_id.as_deref(),
                result_timeout,
            )
            .await
            .map_err(to_py_err)?;
            Ok(PyTopicMessage::from(response))
        })
    }

    /// Check whether an action server is reachable.
    #[staticmethod]
    #[pyo3(signature = (messenger, bound_core_node, as_instance_id, target_node_name, target_action_name, target_core_node=None, target_instance_id=None))]
    #[allow(clippy::too_many_arguments)]
    fn is_reachable<'py>(
        py: Python<'py>,
        messenger: &PyMessengerHandle,
        bound_core_node: String,
        as_instance_id: String,
        target_node_name: String,
        target_action_name: String,
        target_core_node: Option<String>,
        target_instance_id: Option<String>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let handle = messenger.inner.clone();
        pyo3_async_runtimes::tokio::future_into_py(py, async move {
            let reachable = ActionMessenger::is_reachable(
                &handle,
                &bound_core_node,
                &as_instance_id,
                &target_node_name,
                &target_action_name,
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
// Module registration
// ---------------------------------------------------------------------------

/// Register the actions submodule.
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let actions_module = PyModule::new(parent_module.py(), "actions")?;
    actions_module.add_class::<PyActionMessenger>()?;
    actions_module.add_class::<PyActionGoalHandle>()?;
    actions_module.add_class::<PyActionCreation>()?;
    actions_module.add_class::<PyTopicPublisher>()?;
    parent_module.add_submodule(&actions_module)?;
    Ok(())
}
