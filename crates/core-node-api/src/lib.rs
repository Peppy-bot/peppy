//! Shared API surface for talking to a core-node daemon.
//!
//! Holds capnp-backed request/response types, service name constants, and
//! parsers for typed views of responses. No transport layer lives here — see
//! `peppylib::core_node::transport` for the `peppylib`-backed poll / send_goal glue.
//!
//! # Boundary with consumer crates
//!
//! Consumers (`core-node-internal`, `peppylib`, `peppylib-py`, `peppy`,
//! `node-stack-internal`) see exactly four categories of public items, and
//! nothing else:
//!
//! 1. **Wire identifiers** ([`names`]): `&'static str` service/topic/tag
//!    constants. Publish and subscribe sides key on the same constant.
//! 2. **Message codecs** ([`encoding`]): one hand-written struct per message,
//!    each with a pure constructor (`new` / `try_new` / builder methods) and a
//!    symmetric `encode() -> Result<Payload>` / `decode(&[u8]) -> Result<Self>`
//!    pair. These are the only sanctioned way to put bytes on, or take bytes
//!    off, the core-node wire.
//! 3. **Typed response views** ([`graph`]): [`SerializedNodeGraph`] and friends —
//!    the JSON shape of every `*_response.graph_json` field, plus query helpers.
//! 4. **The wire [`Payload`] / [`NonEmptyPayload`] types and the unified
//!    [`Error`] / [`Result`].**
//!
//! The capnp schema and the generated `*_capnp` modules are an implementation
//! detail, sealed behind `pub(crate)` — they never appear in a public signature.
//! Construction is explicit (no global init, no hidden singletons), and the
//! crate performs **no I/O or side effects**: it is a pure data-transform layer.
//! The side-effecting helpers that used to live here have moved to the crate
//! that owns the effect — `wall_now_ns` (system clock) to `peppylib::clock`,
//! launcher-path resolution to the `peppy` CLI, and `RepoSource` identity
//! (filesystem canonicalization) to `core-node`'s `services::repo`.

#![forbid(unsafe_code)]

pub mod encoding;
pub mod error;
pub mod graph;
pub mod names;
mod payload;

pub use error::{Error, Result};
pub use graph::{
    InstanceState, NodeNotFound, NodeStage, SerializedEdge, SerializedInstance, SerializedNode,
    SerializedNodeGraph,
};
pub use payload::{EmptyPayloadError, NonEmptyPayload, Payload};

// Generated Cap'n Proto types - must be at crate root for correct path resolution
#[allow(clippy::all)]
pub(crate) mod ping_capnp {
    include!(concat!(env!("OUT_DIR"), "/ping_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod clock_capnp {
    include!(concat!(env!("OUT_DIR"), "/clock_capnp.rs"));
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
pub(crate) mod benchmark_capnp {
    include!(concat!(env!("OUT_DIR"), "/benchmark_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod node_capnp {
    include!(concat!(env!("OUT_DIR"), "/node_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod repo_capnp {
    include!(concat!(env!("OUT_DIR"), "/repo_capnp.rs"));
}

#[allow(clippy::all)]
pub(crate) mod datastore_capnp {
    include!(concat!(env!("OUT_DIR"), "/datastore_capnp.rs"));
}
