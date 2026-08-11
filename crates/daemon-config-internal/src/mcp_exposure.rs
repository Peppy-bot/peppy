mod parse;
mod types;

// Defines the parsing of MCP exposure documents
// (`peppy_schema: "mcp_exposure/v1"`). An exposure selects members of pinned
// Peppy contracts and gives them stable public names, MCP-facing prose, and
// operational policies; the request and response shapes stay derived from
// the contracts. Filenames are not fixed; any `.json5` whose body carries
// the `mcp_exposure/v1` schema tag is an exposure.
pub use parse::PeppyMcpExposureParser;
pub use types::{
    ActionExposure, ActionOperation, ExposureTarget, FreshnessPolicy, ImageCodec, ImageFieldMap,
    ImageRepresentation, JpegQuality, MaxHz, McpExposure, OversizePolicy, PinnedContractRef,
    PublicName, RestrictBounds, ServerIdentity, ServiceExposure, ServiceOperation, TopicExposure,
    UpdatePolicy,
};
