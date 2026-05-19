use pyo3::prelude::*;

mod clock;
mod config;
mod core_node;
mod messaging;
mod names;
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
    // Re-export the reserved default link_id segment so generated Python
    // code can reference the same constant as the Rust side without
    // duplicating the literal.
    m.add("DEFAULT_LINK_ID", peppylib::messaging::DEFAULT_LINK_ID)?;
    config::register(m)?;
    core_node::register(m)?;
    messaging::register(m)?;
    names::register(m)?;
    runtime::register(m)?;
    services::register(m)?;
    Ok(())
}
