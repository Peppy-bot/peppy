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

pub mod serialization;

// Re-export common serialization traits from our wrapper module
pub use serialization::{Deserialize, DeserializeOwned, JsonSchema, Serialize};

#[allow(clippy::all)]
mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
