mod builder;
mod python;
mod rust;
mod types;

use config::{Interfaces, Language};

use crate::error::Result;
use python::PythonGenerator;
use rust::RustGenerator;

use crate::generator::builder::InterfaceGenerator;

// TODO: How will this work if the same interfaces need to be generated for Python since the code
// needs to be compiled first? The solution might be to just to expose the compiled PMI crate with PyO3
// and generate the .py interfaces

/// Called everytime a new change to a peppy configuration is detected
/// The generated code is stored inside crate::interfaces so during testing do not call this function
/// directly as this would create throwaway code that will be picked up by git
pub fn generate_interfaces_code(
    interfaces: &Interfaces,
    for_language: &Language,
) -> Result<Vec<String>> {
    let generator: Box<dyn InterfaceGenerator> = match for_language {
        Language::Rust => Box::new(RustGenerator::new()),
        Language::Python => Box::new(PythonGenerator::new()),
    };
    builder::compose_interfaces(generator.as_ref(), interfaces)
}
