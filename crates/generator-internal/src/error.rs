use config::ConfigError;
use thiserror::Error;

pub type Result<T> = core::result::Result<T, Error>;

#[derive(Debug, Error)]
pub enum Error {
    // -- general
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Template(#[from] askama::Error),
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error("unknown template `{0}`")]
    UnknownTemplate(String),

    // -- Subscriber errors
    #[error("missing topic message format for subscriber `{0}`")]
    SubscriberTopicMessageFormatMissing(String),
    #[error("subscribed topic `{0}` must specify a node")]
    SubscriberTopicNodeMissing(String),
    #[error("missing service message format for subscriber `{0}`")]
    SubscriberServiceMessageFormatMissing(String),

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
    #[error("The node name `{0}` or tag `{1}` could not be found")]
    NoMatchingNode(String, String),
    #[error("failed to parse generated node module for `{node}`")]
    NodeModuleParseError {
        node: String,
        #[source]
        source: syn::Error,
    },
    #[error("Failed encoding `{0}`")]
    MessageEncoding(ConfigError),
}
