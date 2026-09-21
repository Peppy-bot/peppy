use names_generator2::get_random;
use pyo3::prelude::*;
use rand::rng;

/// Generate a random name in the format "adjective-animal-###".
///
/// Returns a randomly generated name string like "elegant-swine-421" or
/// "intrepid-porcupine-731".
#[pyfunction]
fn generate_name() -> String {
    get_random(rng())
}

/// Register the names submodule
pub(crate) fn register(parent_module: &Bound<'_, PyModule>) -> PyResult<()> {
    let names_module = PyModule::new(parent_module.py(), "names")?;
    names_module.add_function(wrap_pyfunction!(generate_name, &names_module)?)?;
    parent_module.add_submodule(&names_module)?;
    Ok(())
}
