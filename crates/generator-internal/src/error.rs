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

    // -- nodes errors
    #[error("Cannot find the node in `{0}`")]
    NodeNotFound(String),
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
    #[error("array parameters are not yet supported in generated code (at `{path}`)")]
    UnsupportedArrayParameter { path: String },
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
    #[error("unsupported fixed-length array item type `{item}` in field `{field}`")]
    UnsupportedFixedArrayItemType { field: String, item: &'static str },
    #[error(
        "unsupported optional scalar type `{item}` in field `{field}` for `{language:?}` generator"
    )]
    UnsupportedOptionalScalarType {
        language: PeppygenLanguage,
        field: String,
        item: &'static str,
    },
    #[error(
        "generated type name collision in `{context}`: fields `{first_field}` and `{second_field}` \
both produce the type name `{type_name}`"
    )]
    GeneratedTypeNameCollision {
        context: String,
        type_name: String,
        first_field: String,
        second_field: String,
    },
    #[error(
        "field name normalization collision in `{context}` for `{language:?}` generator: \
`{first_field}` and `{second_field}` both normalize to `{normalized}` as `{normalization}`"
    )]
    FieldNameNormalizationCollision {
        language: PeppygenLanguage,
        context: String,
        normalization: &'static str,
        normalized: String,
        first_field: String,
        second_field: String,
    },
}
