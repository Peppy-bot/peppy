//! The Cap'n Proto types that capnpc generates: one `include!` for each schema.
//!
//! `lib.rs` brings each module into scope at the crate root, because capnpc
//! writes paths that start there (`crate::<name>_capnp`).

#![allow(
    warnings,
    clippy::all,
    clippy::pedantic,
    clippy::nursery,
    clippy::restriction,
    reason = "capnpc writes the code of these modules"
)]

pub(crate) mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}

pub(crate) mod action_cancel_capnp {
    include!(concat!(env!("OUT_DIR"), "/action_cancel_capnp.rs"));
}

pub(crate) mod binding_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/binding_update_capnp.rs"));
}

pub(crate) mod peer_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/peer_update_capnp.rs"));
}

pub(crate) mod observation_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/observation_update_capnp.rs"));
}

pub(crate) mod slot_update_capnp {
    include!(concat!(env!("OUT_DIR"), "/slot_update_capnp.rs"));
}

pub(crate) mod endpoints_capnp {
    include!(concat!(env!("OUT_DIR"), "/endpoints_capnp.rs"));
}
