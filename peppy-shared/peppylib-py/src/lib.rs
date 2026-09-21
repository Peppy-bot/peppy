use pyo3::prelude::*;

mod clock;
mod config;
mod core_node;
mod datastore;
mod messaging;
mod names;
mod py_future;
mod runtime;
mod services;

/// Python module implemented in Rust.
/// The function name must match `lib.name` in Cargo.toml.
#[pymodule]
fn _peppylib(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add(
        "__version__",
        option_env!("PEPPY_GIT_TAG").unwrap_or("0.0.1"),
    )?;
    py_future::register_interpreter_exit_gate(m.py())?;
    config::register(m)?;
    core_node::register(m)?;
    messaging::register(m)?;
    names::register(m)?;
    runtime::register(m)?;
    services::register(m)?;
    Ok(())
}
