mod error;
mod generator;

// Exposes all the generated interfaces
pub use error::Error as GeneratorError;

pub use generator::generate_peppygen_lib;
pub use generator::python::PythonGenerator;
pub use generator::rust::RustGenerator;
pub use generator::types::{
    DeploymentInterface, InterfaceVariant, LanguageGenerator, SubscribedActionMessage,
};

/// Returns the shared Cargo target directory for Rust nodes.
///
/// All daemon-managed Rust nodes compile into this directory so they share
/// compiled artifacts for common dependencies (tokio, serde, capnp, etc.).
pub fn rust_shared_target_dir() -> std::path::PathBuf {
    let cache_key = format!("{}-{}", env!("RUST_CRATES_HASH"), env!("CARGO_PKG_VERSION"));
    config::consts::peppy_data_dir()
        .join("libs/rust")
        .join(&cache_key)
        .join("target")
}
