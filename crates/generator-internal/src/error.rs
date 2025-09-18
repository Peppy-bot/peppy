use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),

    // -- Subscriber errors
    #[error("missing topic message format for subscriber `{0}`")]
    SubscriberTopicMessageFormatMissing(String),
    #[error("missing service message format for subscriber `{0}`")]
    SubscriberServiceMessageFormatMissing(String),
    #[error("missing action message format for subscriber `{0}`")]
    SubscriberActionMessageFormatMissing(String),
}
