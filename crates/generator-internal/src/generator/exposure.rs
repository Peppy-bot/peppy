//! MCP exposure bundles: the publication-time half of `mcp_exposure/v1`.
//!
//! An exposure document selects members of pinned contracts; this module
//! turns that selection into the versioned exposure bundle the generated MCP
//! server node serves from, and into the node itself. [`json_schema`] owns
//! the canonical mapping from `message_format` definitions to the public
//! JSON Schemas, [`validate`] checks an exposure against its pinned contract
//! documents and builds the bundle, [`bundle`] is the bundle's serializable
//! shape, and [`node`] emits the thin MCP server node crate composing the
//! shared `peppy-mcp-runtime`.

mod bundle;
mod json_schema;
mod node;
mod validate;

pub use bundle::{
    BundleContractPin, BundleIdentity, BundleNode, BundleServer, EXPOSURE_BUNDLE_FORMAT,
    ExposureBundle, ResourceEntry, ResourcePolicies, TaskEntry, ToolEntry,
};
pub use json_schema::{
    MaxSerializedSize, SCHEMA_MAPPING_VERSION, max_serialized_json_bytes,
    message_format_to_json_schema,
};
pub use node::{
    GeneratedFile, GeneratedServerNode, generate_exposure_node, generate_exposure_node_from_bundle,
};
pub use validate::{ExposureValidationError, ResolvedContractDocument, build_exposure_bundle};
