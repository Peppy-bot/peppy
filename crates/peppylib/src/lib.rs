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

// Core node helpers, namespaced by subsystem: `peppylib::datastore::store`,
// `peppylib::clock::subscribe`, `peppylib::stack::list`, and their types.
// `info` is a single verb-less call, so it stays flat.
pub use core_node::info::info;
pub use core_node::{clock, datastore, stack};

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

#[allow(clippy::all)]
mod action_cancel_capnp {
    include!(concat!(env!("OUT_DIR"), "/action_cancel_capnp.rs"));
}
