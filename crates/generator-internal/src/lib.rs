mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;

pub use generator::generate_lib_for_build_system;
pub use generator::types::{DeploymentInterface, InterfaceVariant, SubscribedActionMessage};
