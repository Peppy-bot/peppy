use config::consts::DEFAULT_MESSAGING_PORT;
use config::node::QoSProfile;
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

/// Register the config submodule
pub fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let config_module = PyModule::new(parent_module.py(), "config")?;
    config_module.add("DEFAULT_MESSAGING_PORT", DEFAULT_MESSAGING_PORT)?;
    config_module.add_class::<PyQoSProfile>()?;
    parent_module.add_submodule(&config_module)?;
    Ok(())
}
