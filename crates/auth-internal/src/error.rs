pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    // -- filesystem (credential cache reads/writes)
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    // -- transport/HTTP failures (unreachable backend, unexpected status)
    #[error("{0}")]
    Http(String),

    // -- OAuth / identity failures with a user-actionable message
    #[error("{0}")]
    Auth(String),

    // -- no usable credential and not on an interactive terminal
    #[error("Not authenticated. Run `peppy auth login` or set PEPPY_API_KEY.")]
    NotAuthenticated,
}
