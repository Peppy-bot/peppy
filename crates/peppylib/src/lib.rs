mod error;

pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;

pub use error::{Error as PeppyError, Result as PeppyResult};
pub use messaging::{ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger};
pub use pmi::start_zenohd_process;

/// Reports an error to stderr in a human-readable format and exits with code 1.
/// Use this in your node's main function instead of returning Result directly.
///
/// # Example
/// ```ignore
/// fn main() {
///     if let Err(e) = my_node::runner::run(|params, runner| async move {
///         // node logic
///         Ok(())
///     }) {
///         peppylib::report_error_and_exit(e);
///     }
/// }
/// ```
pub fn report_error_and_exit(error: impl std::fmt::Display) -> ! {
    eprintln!("Error: {error}");
    std::process::exit(1);
}

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
