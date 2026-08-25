mod parse;

// The `mcp_exposure/v1` document model lives in the shared `peppy-mcp-catalog`
// crate beside the validation that derives a catalog from it, so the daemon,
// the built-in server and the hub check all read one definition. Re-exported
// here with the daemon's own parser, which maps a file to the daemon's
// error vocabulary the way the contract and pairing parsers do.
pub use parse::PeppyMcpExposureParser;
pub use peppy_mcp_catalog::{
    ActionExposure, ActionOperation, ExposureManifest, ExposureTarget, FreshnessPolicy, ImageCodec,
    ImageFieldMap, ImageRepresentation, JpegQuality, MaxHz, McpExposure, OversizePolicy,
    PinnedContractRef, PublicName, RestrictBounds, ServerIdentity, ServiceExposure,
    ServiceOperation, TopicExposure, UpdatePolicy,
};
