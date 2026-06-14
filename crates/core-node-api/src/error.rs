use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    #[error("capnp encoding error: {0}")]
    Capnp(#[from] capnp::Error),

    #[error("capnp schema error: {0}")]
    CapnpNotInSchema(#[from] capnp::NotInSchema),

    #[error("invalid UTF-8 in message: {0}")]
    Utf8(#[from] std::str::Utf8Error),

    #[error("decoding error: {0}")]
    Decoding(String),

    #[error("encoding error: {0}")]
    Encoding(String),

    #[error(transparent)]
    InvalidDatastoreKey(#[from] crate::encoding::DatastoreKeyError),

    #[error(transparent)]
    ParsingError(#[from] config::ParsingError),
}
