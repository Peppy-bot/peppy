use std::path::PathBuf;

use config::ConfigError;
use git2::Error as GitError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error("{0} not implemented yet")]
    NotImplemented(&'static str),
    #[error("{0} could not be found")]
    FileNotFound(PathBuf),
    #[error("Failed to download bundle `{url}`: {reason}")]
    HttpDownload { url: String, reason: String },
    #[error("Failed to extract bundle `{url}`: {reason}")]
    BundleExtraction { url: String, reason: String },
    #[error("Checksum mismatch for bundle `{0}`")]
    ChecksumMismatch(String),
    #[error("Unsupported checksum algorithm `{0}`")]
    UnsupportedChecksum(String),
    #[error("Invalid checksum `{0}`: {1}")]
    InvalidChecksum(String, String),

    // -- config-internal
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),

    // -- deployment errors
    // {0}: node_name + tag, {1}: Reason
    #[error("Failed to resolve deployment {0}: {1}")]
    DeploymentNotResolvable(String, String),
}
