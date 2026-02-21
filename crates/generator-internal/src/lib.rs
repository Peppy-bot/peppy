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

/// Returns a stable, per-node target directory for Rust builds.
///
/// Each node gets its own target directory (keyed by `node_name` + `tag`) so
/// parallel builds don't contend on cargo's target-directory lock.  The path
/// also includes `RUST_CRATES_HASH` + `CARGO_PKG_VERSION` so a peppy upgrade
/// that changes the vendored crates starts from a clean slate.
pub fn rust_node_target_dir(node_name: &str, tag: &str) -> std::path::PathBuf {
    let cache_key = format!("{}-{}", env!("RUST_CRATES_HASH"), env!("CARGO_PKG_VERSION"));
    config::consts::peppy_data_dir()
        .join("cache/rust")
        .join(cache_key)
        .join("targets")
        .join(format!("{node_name}_{tag}"))
}
