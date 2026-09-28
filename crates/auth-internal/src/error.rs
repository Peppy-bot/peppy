pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- filesystem (credential and enrollment reads/writes)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- transport/HTTP failures (unreachable backend, unexpected status)
    #[error("{0}")]
    Http(String),

    // -- OAuth / identity / enrollment failures with a user-actionable message
    #[error("{0}")]
    Auth(String),

    // -- no usable session
    #[error("Not authenticated. Run `peppy platform login`.")]
    NotAuthenticated,
}
