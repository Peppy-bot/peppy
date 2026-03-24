mod common;
mod config;
mod error;
mod node_index;
mod parsing;
pub mod source;

pub mod consts;
pub mod encoding;
pub mod fingerprint;
pub mod node;
pub mod runtime;
pub mod peppy_config {
    pub use crate::config::*;
}

#[cfg(feature = "test_helpers")]
pub mod test_helpers;

pub use common::{
    AnyType, NodeArguments, TypeMismatch, for_each_parameter_leaf_path, is_array_parameter_schema,
    parameter_leaf_paths, resolve_parameter_path,
};

// Node configuration index (filesystem snapshot)
pub use node_index::{FSNodeConfigIndex, NodeIndexState};

pub use error::{Error as ConfigError, ParsingError, Result as ConfigResult};
