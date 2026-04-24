mod error;

pub mod core_node;
pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;
pub use error::{
    Error as PeppyError, MissingStandaloneParameters, ParameterDeserializationError,
    Result as PeppyResult,
};
pub use messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};
pub mod config;
pub mod types;

// Core node functions
pub use core_node::{
    info::info,
    stack::{StackList, stack_list},
};

pub use types::{Message, Payload};

pub mod serialization;

// Re-export serialization traits.
pub use serialization::{Deserialize, DeserializeOwned, JsonSchema, Serialize};

/// Derive macros for the serialization traits.
///
/// Usage: `#[derive(peppylib::derive::Serialize, peppylib::derive::Deserialize)]`
pub mod derive {
    pub use schemars_derive::JsonSchema;
    pub use serde_derive::{Deserialize, Serialize};
}

#[allow(clippy::all)]
mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
