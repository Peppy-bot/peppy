//! Peppy's client of OCI registries: the publish repository that `peppy node
//! publish` pushes the `.sif` of container nodes to, and the `oci`
//! repositories that a daemon pulls them from.
//!
//! One repository holds one artifact per node, peppy version and platform,
//! under an immutable tag (see [`core_node_api::encoding::ArtifactTag`]).
//! Each artifact is an OCI image manifest with a `.sif` layer, a
//! `peppy.json5` layer and annotations (see [`manifest`]).

mod auth;
mod client;
pub mod digest;
mod error;
pub mod image;
pub mod manifest;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use auth::{Credential, CredentialStore};
pub use client::{Access, DEFAULT_UPLOAD_CHUNK_BYTES, MAX_SMALL_BLOB_BYTES, RegistryClient};
pub use error::{RegistryError, Result};
