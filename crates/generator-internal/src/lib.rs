mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;
pub mod interfaces;

pub use generator::generate_lib_for_language;
pub use generator::types::{DeploymentInterface, Language};
