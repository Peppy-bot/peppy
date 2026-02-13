use config::{ConfigError, node::PeppygenLanguage};
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
    #[error(
        "Invalid parameter field name `{name}`: contains invalid characters. Allowed: {allowed}"
    )]
    InvalidParameterFieldName { name: String, allowed: &'static str },
    #[error(
        "unsupported parameter specification type `{kind}` at `{path}`. Expected a type string or object."
    )]
    UnsupportedParameterSpecType { path: String, kind: &'static str },
    #[error("unsupported parameter type name `{type_name}` at `{path}`")]
    UnsupportedParameterTypeName { path: String, type_name: String },
    #[error(
        "Unauthorized message field name `{field}` at `{path}` in `{context}`. \
This field name is reserved by peppy transport metadata and cannot be used inside `message_format`."
    )]
    UnauthorizedMessageFieldName {
        field: String,
        path: String,
        context: String,
    },
    #[error("unsupported nested schema type in array `{field}`")]
    UnsupportedArrayItemSchema { field: String },
    #[error("internal generator invariant violated: {context}")]
    InvariantViolation { context: String },
    #[error(
        "unsupported fixed-length array item type `{item}` in field `{field}` for `{language:?}` generator"
    )]
    UnsupportedFixedArrayItemType {
        language: PeppygenLanguage,
        field: String,
        item: &'static str,
    },
    #[error(
        "unsupported optional scalar type `{item}` in field `{field}` for `{language:?}` generator"
    )]
    UnsupportedOptionalScalarType {
        language: PeppygenLanguage,
        field: String,
        item: &'static str,
    },
}
