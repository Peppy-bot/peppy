mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;

pub use generator::rust::RustGenerator;
pub use generator::types::{
    DeploymentInterface, InterfaceVariant, LanguageGenerator, SubscribedActionMessage,
};
pub use generator::{generate_lib_for_build_system, generate_lib_for_build_system_with_subscribed};
