//! Generated Cap'n Proto types — one `include!` per schema in `schemas/`.
//!
//! These modules are an implementation detail, sealed behind `pub(crate)`: the
//! capnp schema never appears in a public signature. `lib.rs` re-exports each of
//! them at the crate root (`crate::*_capnp`) because capnpc emits crate-root-
//! relative paths — the generated code refers to its sibling types via absolute
//! `crate::<name>_capnp::...` paths, so the modules must be reachable there.

#![allow(clippy::all)]

pub(crate) mod health_capnp {
    include!(concat!(env!("OUT_DIR"), "/health_capnp.rs"));
}

pub(crate) mod clock_capnp {
    include!(concat!(env!("OUT_DIR"), "/clock_capnp.rs"));
}

pub(crate) mod info_capnp {
    include!(concat!(env!("OUT_DIR"), "/info_capnp.rs"));
}

pub(crate) mod launch_capnp {
    include!(concat!(env!("OUT_DIR"), "/launch_capnp.rs"));
}

pub(crate) mod benchmark_capnp {
    include!(concat!(env!("OUT_DIR"), "/benchmark_capnp.rs"));
}

pub(crate) mod node_capnp {
    include!(concat!(env!("OUT_DIR"), "/node_capnp.rs"));
}

pub(crate) mod repo_capnp {
    include!(concat!(env!("OUT_DIR"), "/repo_capnp.rs"));
}

pub(crate) mod datastore_capnp {
    include!(concat!(env!("OUT_DIR"), "/datastore_capnp.rs"));
}

pub(crate) mod federation_capnp {
    include!(concat!(env!("OUT_DIR"), "/federation_capnp.rs"));
}
