mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;

pub use generator::generate_peppygen_lib;
pub use generator::rust::RustGenerator;
pub use generator::types::{
    DeploymentInterface, InterfaceVariant, LanguageGenerator, SubscribedActionMessage,
};
