//! Re-exports the capnp wire types from `core-node-api` and layers on the
//! peppylib-backed transport helpers (`poll` / `send_goal`) that used to live
//! directly on those types.
//!
//! Downstream crates import the types through this module (so the old paths
//! like `core_node::encoding::StackListRequest` keep working) and bring the
//! transport methods back into scope via
//! [`prelude`](self::prelude): `use core_node::encoding::prelude::*;`.

pub use core_node_api::encoding::*;

pub mod transport;
pub use transport::prelude;
