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

    // -- config-internal
    #[error(transparent)]
    Config(#[from] ConfigError),

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),
}
