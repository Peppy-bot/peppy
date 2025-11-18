/// The master node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process
mod commands;
mod error;
mod node;

use error::{Error, Result};
pub use node::MasterNode;
