/// The core node is a special kind of node that has access to the whole context of the peppy daemon and runs as part of the same process
pub mod encoding;
pub mod names;

mod error;
mod services;

// Generated Cap'n Proto types - must be at crate root for correct path resolution
#[allow(clippy::all)]
pub mod ping_capnp {
    include!(concat!(env!("OUT_DIR"), "/ping_capnp.rs"));
}

#[allow(clippy::all)]
pub mod info_capnp {
    include!(concat!(env!("OUT_DIR"), "/info_capnp.rs"));
}

#[allow(clippy::all)]
pub mod launch_capnp {
    include!(concat!(env!("OUT_DIR"), "/launch_capnp.rs"));
}

#[allow(clippy::all)]
pub mod node_capnp {
    include!(concat!(env!("OUT_DIR"), "/node_capnp.rs"));
}

pub use error::{Error, Result};
pub use services::{CoreNode, CoreNodeArguments, FORBIDDEN_ENV_KEYS};
