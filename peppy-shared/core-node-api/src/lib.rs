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
//! 1. **Wire identifiers**: the [`ServiceId`] / [`ActionId`] / [`TopicId`]
//!    enums from the method [`registry`] — one variant per wire method, with
//!    `.name()` as the only enum-to-string step — plus the single non-method
//!    constant [`names::CORE_NODE_TAG`]. Publish and subscribe sides key on
//!    the same declaration.
//! 2. **Message codecs** ([`encoding`]): one hand-written struct per message,
//!    each with a pure constructor (`new` / `try_new` / builder methods) and a
//!    symmetric `encode() -> Result<Payload>` / `decode(&[u8]) -> Result<Self>`
//!    pair. These are the only sanctioned way to put bytes on, or take bytes
//!    off, the core-node wire.
//! 3. **Typed response views** ([`graph`]): [`SerializedNodeGraph`] and friends —
//!    the JSON shape of every `*_response.graph_json` field, plus query helpers.
//! 4. **The wire [`Payload`] / [`NonEmptyPayload`] types and the unified
//!    [`Error`] / [`Result`].**
//! 5. **Protocol policy constants** ([`env`]): [`FORBIDDEN_ENV_KEYS`], the
//!    env-var blocklist enforced by the daemon when validating incoming goals
//!    and by the CLI when filtering the caller environment before sending.
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

mod capnp_generated;
pub mod encoding;
pub mod env;
pub mod error;
pub mod graph;
pub mod names;
mod payload;
pub mod registry;

pub use env::FORBIDDEN_ENV_KEYS;
pub use error::{Error, Result};
pub use graph::{
    InstanceEndpoint, InstanceState, NodeNotFound, NodeStage, SerializedEdge, SerializedInstance,
    SerializedNode, SerializedNodeGraph, SerializedPairingSlot,
};
pub use payload::{EmptyPayloadError, NonEmptyPayload, Payload};
pub use registry::{ActionGoal, ActionId, LaunchScoped, ServiceId, ServiceRequest, TopicId};

// The generated Cap'n Proto modules must be reachable at the crate root as
// `crate::*_capnp` because capnpc emits crate-root-relative paths. They live in
// `capnp_generated`; re-export them here so those paths resolve.
pub(crate) use capnp_generated::{
    benchmark_capnp, clock_capnp, datastore_capnp, federation_capnp, health_capnp, info_capnp,
    launch_capnp, node_capnp, repo_capnp,
};
