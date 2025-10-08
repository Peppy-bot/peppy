mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;
pub mod interfaces;

pub use generator::InterfaceGenerator;
pub use generator::types::{DeploymentInterface, Language};
