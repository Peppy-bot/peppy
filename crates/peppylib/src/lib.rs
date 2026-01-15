mod error;

pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;

pub use error::{Error as PeppyError, Result as PeppyResult};
pub use messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};
pub use pmi::start_zenohd_process;
pub use runtime::StandaloneConfig;
pub use runtime::runner::run_standalone;

// Reexport useful modules for the user of the lib
pub mod config {
    pub use config::NodeArguments;
    pub use config::consts::{
        NODE_CONFIG_FILE, NODE_CONFIG_FINGERPRINT_FILE, RUNTIME_CONFIG_VAR_NAME,
    };
    pub use config::node::*;

    pub fn deserialize_parameters<T>(args: &NodeArguments) -> Result<T, crate::PeppyError>
    where
        T: serde::de::DeserializeOwned,
    {
        let json_value = serde_json::to_value(args).map_err(|e| {
            crate::PeppyError::ParameterDeserialization(format!(
                "failed to serialize parameters: {}",
                e
            ))
        })?;
        serde_json::from_value(json_value).map_err(|e| {
            crate::PeppyError::ParameterDeserialization(format!(
                "failed to deserialize parameters: {}",
                e
            ))
        })
    }
}

#[allow(clippy::all)]
pub mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}
