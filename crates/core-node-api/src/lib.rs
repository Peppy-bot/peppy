//! Shared API surface for talking to a core-node daemon.
//!
//! Holds capnp-backed request/response types, service name constants, and
//! parsers for typed views of responses. No transport layer lives here — see
//! `peppylib::core_node::transport` for the `peppylib`-backed poll / send_goal glue.

pub mod encoding;
pub mod error;
pub mod graph;
pub mod names;
mod payload;

pub use error::{Error, Result};
pub use graph::{SerializedEdge, SerializedInstance, SerializedNode, SerializedNodeGraph};
pub use payload::Payload;

// Generated Cap'n Proto types - must be at crate root for correct path resolution
#[allow(clippy::all)]
pub(crate) mod ping_capnp {
    include!(concat!(env!("OUT_DIR"), "/ping_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod info_capnp {
    include!(concat!(env!("OUT_DIR"), "/info_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod launch_capnp {
    include!(concat!(env!("OUT_DIR"), "/launch_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod node_capnp {
    include!(concat!(env!("OUT_DIR"), "/node_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod repo_capnp {
    include!(concat!(env!("OUT_DIR"), "/repo_capnp.rs"));
}
