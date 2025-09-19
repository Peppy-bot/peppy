use config::ConfigError;
use git2::Error as GitError;
use thiserror::Error;

use std::str::Utf8Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Git(#[from] GitError),
    #[error(transparent)]
    Utf8(#[from] Utf8Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("{0} not implemented yet")]
    NotImplemented(&'static str),

    // -- Subscriber errors
    #[error("missing topic message format for subscriber `{0}`")]
    SubscriberTopicMessageFormatMissing(String),
    #[error("missing service message format for subscriber `{0}`")]
    SubscriberServiceMessageFormatMissing(String),
    #[error("missing action message format for subscriber `{0}`")]
    SubscriberActionMessageFormatMissing(String),

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),
}
