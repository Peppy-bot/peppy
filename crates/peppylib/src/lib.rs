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

pub use schemars;
pub use serde;

#[allow(clippy::all)]
pub mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
