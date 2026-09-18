// peppylib's shipped library contains no `unsafe` in production code paths.
// This denies any new unsafe crate-wide; the only opt-outs are the two scoped
// `#[allow(unsafe_code)]` helpers in the `testing` module (never compiled into
// production builds — the `testing` feature is enabled via dev-dependencies
// only), which need load-bearing FFI/ctor test infrastructure with no safe
// equivalent.
#![deny(unsafe_code)]

mod error;

pub mod core_node;
pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;
pub use error::{Error as PeppyError, ParameterDeserializationError, Result as PeppyResult};
pub use messaging::{
    ActionMessenger, CoreNodePresence, CoreNodePresenceMessenger, LivelinessEvent, LivelinessToken,
    LivelinessWatch, MessengerHandle, ServiceMessenger, SessionScope, TopicMessenger,
    TopicPublisher,
};
pub mod config;
pub mod types;

// Core node helpers, namespaced by subsystem: `peppylib::datastore::store`,
// `peppylib::clock::subscribe`, `peppylib::stack::list`, and their types. Each
// is a crate-root module so there is a single public path per subsystem (the
// raw wire transport stays under `peppylib::core_node::transport`). `info` is a
// single verb-less call, so it stays flat as a function rather than a module.
pub mod clock;
pub mod datastore;
mod info;
pub mod stack;
pub use info::info;

// Node-invariant test machinery (ephemeral router, mock cores, harness core)
// for generated per-node test code and this workspace's own suites. Gated so
// none of it can reach a production binary: node crates enable the feature
// via `[dev-dependencies]`, which `cargo build` never resolves.
#[cfg(feature = "testing")]
pub mod testing;

pub use types::{Message, Payload};

#[allow(clippy::all)]
mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}

#[allow(clippy::all)]
mod action_cancel_capnp {
    include!(concat!(env!("OUT_DIR"), "/action_cancel_capnp.rs"));
}

#[allow(clippy::all)]
mod peer_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/peer_update_capnp.rs"));
}

mod observation_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/observation_update_capnp.rs"));
}

#[allow(clippy::all)]
mod slot_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/slot_update_capnp.rs"));
}

#[allow(clippy::all)]
mod endpoints_capnp {
    include!(concat!(env!("OUT_DIR"), "/endpoints_capnp.rs"));
}
