mod error;

pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;
pub use error::{Error as PeppyError, ParameterDeserializationError, Result as PeppyResult};
pub use messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};
pub use schemars;
pub use serde;
pub mod config;

#[allow(clippy::all)]
pub mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
