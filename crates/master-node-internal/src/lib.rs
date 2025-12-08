/// The master node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process
mod commands;
pub mod encoding;
mod error;
mod node;

// Generated Cap'n Proto types - must be at crate root for correct path resolution
#[allow(clippy::all)]
#[allow(dead_code)]
pub mod messages_capnp {
    include!(concat!(env!("OUT_DIR"), "/messages_capnp.rs"));
}

pub use error::{Error, Result};
pub use node::MasterNode;
