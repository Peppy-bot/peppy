//! Shared state and TLS material for user-managed router federation.
//!
//! The CLI owns registry and PKI mutations. The daemon consumes the same
//! registry and locator builders, which keeps the persisted endpoint key and
//! the per-endpoint Zenoh TLS fragments consistent across both processes.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use daemon_config::consts::PeppyDirs;
use thiserror::Error;

pub mod links;
pub mod pki;
pub mod registry;

pub use links::{
    IdentityPaths, PeerLink, backend_connect_locator, listener_locator, peer_connect_locator,
    peer_links, peer_probe_tls, resolve_identity_paths,
};
pub use pki::{ca_init, issue};
pub use registry::{FederationPeer, Federations, RegistryLock, load, lock, save, with_registry};

/// Display name reserved for the federation managed by `peppy auth`.
pub const RESERVED_BACKEND_NAME: &str = "platform-backend";

/// Directory name containing the conventional fleet identity.
pub const FEDERATION_DIR_NAME: &str = "federation";
/// Registry file name under the Peppy configuration directory.
pub const FEDERATIONS_FILE: &str = "federations.json5";
/// Fleet CA certificate file name.
pub const CA_CERT_FILE: &str = "ca.pem";
/// Fleet CA private-key file name.
pub const CA_KEY_FILE: &str = "ca.key";
/// Machine certificate file name.
pub const CERT_FILE: &str = "cert.pem";
/// Machine private-key file name.
pub const KEY_FILE: &str = "key.pem";

/// `<PEPPY_HOME>/conf/federation`, the conventional identity directory.
pub fn federation_dir(dirs: &PeppyDirs) -> PathBuf {
    dirs.conf_dir().join(FEDERATION_DIR_NAME)
}

/// `<PEPPY_HOME>/conf/federations.json5`, the user-peer registry.
pub fn registry_path(dirs: &PeppyDirs) -> PathBuf {
    dirs.conf_dir().join(FEDERATIONS_FILE)
}

/// Errors surfaced while parsing or mutating federation state and identity.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("invalid federation registry: {0}")]
    Registry(String),
    #[error("invalid federation TLS configuration: {0}")]
    Tls(String),
    #[error("federation PKI operation failed: {0}")]
    Pki(String),
}

pub type Result<T> = std::result::Result<T, Error>;
