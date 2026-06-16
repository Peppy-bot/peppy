// peppylib's shipped library contains no `unsafe`. This denies any new unsafe
// crate-wide (production and test code alike); the only opt-out is the scoped
// `#![allow(unsafe_code)]` in `messaging/tests.rs`, which needs load-bearing
// FFI/ctor test infrastructure with no safe equivalent.
#![deny(unsafe_code)]

mod error;

pub mod core_node;
pub mod encoding;
pub mod messaging;
pub mod runtime;
pub mod services;
pub use error::{Error as PeppyError, ParameterDeserializationError, Result as PeppyResult};
pub use messaging::{
    ActionMessenger, MessengerHandle, ServiceMessenger, TopicMessenger, TopicPublisher,
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

pub use types::{Message, Payload};

#[allow(clippy::all)]
mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}

#[allow(clippy::all)]
mod action_cancel_capnp {
    include!(concat!(env!("OUT_DIR"), "/action_cancel_capnp.rs"));
}
