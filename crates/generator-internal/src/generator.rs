mod python;
mod rust;
pub mod types;

use crate::error::Result;
use python::PythonGenerator;
use rust::RustGenerator;
use std::path::Path;
use types::{DeploymentInterface, InterfaceBackend, Language};

/// Generate an interface library for the given language into the provided directory.
pub fn generate_lib_for_language(
    language: Language,
    interfaces: &[DeploymentInterface],
    output_dir: impl AsRef<Path>,
) -> Result<()> {
    let output_dir = output_dir.as_ref();

    match language {
        Language::Rust => generate_with_backend(RustGenerator::new(), interfaces, output_dir),
        Language::Python => generate_with_backend(PythonGenerator::new(), interfaces, output_dir),
    }
}

fn generate_with_backend<B>(
    mut backend: B,
    interfaces: &[DeploymentInterface],
    output_dir: &Path,
) -> Result<()>
where
    B: InterfaceBackend,
{
    for interface in interfaces {
        interface.register_with(&mut backend);
    }
    backend.build(output_dir)
}
