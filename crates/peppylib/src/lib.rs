mod error;

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

pub use types::Payload;

// Re-export common serialization traits to avoid forcing users to depend on serde/schemars directly
pub use schemars::JsonSchema;
pub use serde::de::DeserializeOwned;
pub use serde::{Deserialize, Serialize};

#[allow(clippy::all)]
mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
