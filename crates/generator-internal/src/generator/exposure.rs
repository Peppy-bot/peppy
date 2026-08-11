//! MCP exposure bundles: the publication-time half of `mcp_exposure/v1`.
//!
//! An exposure document selects members of pinned contracts; this module
//! turns that selection into the versioned exposure bundle the generated MCP
//! server node serves from. [`json_schema`] owns the canonical mapping from
//! `message_format` definitions to the public JSON Schemas, [`validate`]
//! checks an exposure against its pinned contract documents and builds the
//! bundle, and [`bundle`] is the bundle's serializable shape.

mod bundle;
mod json_schema;
mod validate;

pub use bundle::{
    BundleContractPin, BundleIdentity, BundleNode, BundleServer, EXPOSURE_BUNDLE_FORMAT,
    ExposureBundle, ResourceEntry, ResourcePolicies, TaskEntry, ToolEntry,
};
pub use json_schema::{
    MaxSerializedSize, SCHEMA_MAPPING_VERSION, max_serialized_json_bytes,
    message_format_to_json_schema,
};
pub use validate::{ExposureValidationError, ResolvedContractDocument, build_exposure_bundle};
