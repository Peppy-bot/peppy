mod deployment;
mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;
pub mod interfaces;

// Class that creates a map from the `deployments` to the actual nodes expected inputs/output messages
pub use deployment::DeploymentMappingBuilder;

//pub use generator::generate_interfaces_code;
