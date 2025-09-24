use pyo3::prelude::*;

/// Example function: returns the sum of two integers as a string.
#[pyfunction]
fn sum_as_string(a: usize, b: usize) -> PyResult<String> {
    Ok((a + b).to_string())
}

/// Python module implemented in Rust.
/// The function name must match `lib.name` in Cargo.toml.
#[pymodule]
fn _peppylib(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sum_as_string, m)?)?;
    Ok(())
}
