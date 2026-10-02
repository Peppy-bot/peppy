use config::consts::{
    DEFAULT_MESSAGING_PORT, NODE_CONFIG_FILE, PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME,
};
use config::node::{QoSProfile, TopicRetention};
use peppylib::messaging::{NODE_HEALTH_SERVICE, NODE_READY_SERVICE, SHUTDOWN_SERVICE};
use pyo3::prelude::*;

/// QoS profile for topic messaging.
///
/// Exposes `config::node::QoSProfile` to Python.
#[pyclass(name = "QoSProfile", eq, eq_int, from_py_object)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PyQoSProfile {
    SensorData,
    Standard,
    Reliable,
    Critical,
}

impl From<PyQoSProfile> for QoSProfile {
    fn from(py_qos: PyQoSProfile) -> Self {
        match py_qos {
            PyQoSProfile::SensorData => QoSProfile::SensorData,
            PyQoSProfile::Standard => QoSProfile::Standard,
            PyQoSProfile::Reliable => QoSProfile::Reliable,
            PyQoSProfile::Critical => QoSProfile::Critical,
        }
    }
}

/// What a topic keeps for subscribers that join later.
///
/// Exposes `config::node::TopicRetention` to Python:
/// `TopicRetention.live_only()` or `TopicRetention.latest(depth)`.
#[pyclass(name = "TopicRetention", eq, frozen, from_py_object)]
#[derive(Clone, PartialEq, Eq)]
pub struct PyTopicRetention(TopicRetention);

#[pymethods]
impl PyTopicRetention {
    /// A subscriber receives the messages published after it attaches.
    #[staticmethod]
    fn live_only() -> Self {
        Self(TopicRetention::LiveOnly)
    }

    /// The publishing instance keeps its newest `depth` messages, and a new
    /// subscriber receives them too. A depth of 0 is live only.
    #[staticmethod]
    #[pyo3(signature = (depth=1))]
    fn latest(depth: usize) -> PyResult<Self> {
        TopicRetention::latest(depth)
            .map(Self)
            .map_err(|error| pyo3::exceptions::PyValueError::new_err(error.to_string()))
    }

    fn __repr__(&self) -> String {
        match self.0 {
            TopicRetention::LiveOnly => "TopicRetention.live_only()".to_string(),
            TopicRetention::Latest { depth } => {
                format!("TopicRetention.latest({})", depth.get())
            }
        }
    }
}

impl From<PyTopicRetention> for TopicRetention {
    fn from(retention: PyTopicRetention) -> Self {
        retention.0
    }
}

/// Register the config submodule
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let config_module = PyModule::new(parent_module.py(), "config")?;
    config_module.add("DEFAULT_MESSAGING_PORT", DEFAULT_MESSAGING_PORT)?;
    config_module.add("NODE_HEALTH_SERVICE", NODE_HEALTH_SERVICE)?;
    config_module.add("NODE_READY_SERVICE", NODE_READY_SERVICE)?;
    config_module.add("SHUTDOWN_SERVICE", SHUTDOWN_SERVICE)?;
    config_module.add("RUNTIME_CONFIG_VAR_NAME", RUNTIME_CONFIG_VAR_NAME)?;
    config_module.add("NODE_CONFIG_FILE", NODE_CONFIG_FILE)?;
    config_module.add("PEPPYGEN_OUTPUT_PATH", PEPPYGEN_OUTPUT_PATH)?;
    config_module.add_class::<PyQoSProfile>()?;
    config_module.add_class::<PyTopicRetention>()?;
    parent_module.add_submodule(&config_module)?;
    Ok(())
}
