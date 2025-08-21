use thiserror::Error;

#[derive(Error, Debug)]
pub enum MessengerError {
    #[error("Failed to connect to backend")]
    ConnectionError,

    #[error("Failed to publish message to topic {topic}")]
    PublishError { topic: String },

    #[error("Failed to subscribe to topic {topic}")]
    SubscribeError { topic: String },

    #[error("Failed to shutdown backend")]
    ShutdownError,

    #[error("Backend operation failed: {0}")]
    BackendError(String),
}
