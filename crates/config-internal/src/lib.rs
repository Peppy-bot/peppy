mod common;
mod config;
mod error;
mod watch;

pub mod consts;
pub mod encoding;
pub mod node;
pub mod runtime;
pub mod peppy_config {
    pub use crate::config::*;
}

pub use common::{AnyType, NodeParameters};

// To watch projects
pub use watch::{FSNodeConfigWatcher, NodeIndexState};

pub use error::{Error as ConfigError, Result as ConfigResult};
