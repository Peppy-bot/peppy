/// The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process
pub mod names;
pub mod transport;

mod error;
mod services;

pub use error::{Error, Result};
pub use services::{CoreNode, CoreNodeArguments, FORBIDDEN_ENV_KEYS};
