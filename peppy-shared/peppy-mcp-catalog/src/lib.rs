//! The MCP exposure catalog: the `mcp_exposure/v1` document model, its
//! validation against the contracts it names, and the versioned catalog
//! (bundle) that validation derives.
//!
//! An exposure document selects members of contracts and gives them stable
//! public names, MCP-facing prose, and operational policies. Validating it
//! against the resolved contracts yields the bundle: the public catalog
//! (stable names, prose, policies, derived JSON Schemas) plus the identity
//! and contract slots of the process serving it. The `peppy` binary derives
//! bundles when it checks a hub and when it serves exposures; the MCP server
//! runtime serves exactly what a bundle declares.
//!
//! Entry points:
//!
//! - [`McpExposure`], the parsed document, coherent on its own.
//! - [`build_exposure_bundle`] with [`ResolvedContract`], the validation and
//!   derivation, reporting every violation at once; its [`ValidatedExposure`]
//!   carries the bundle beside the contract member behind each entry.
//! - [`message_format_to_json_schema`], the canonical mapping from
//!   `message_format` definitions to the published JSON Schemas.
//! - [`ExposureBundle`] with [`ExposureBundle::from_json_str`] and
//!   [`ExposureBundle::to_json_string`], the catalog shape and its canonical
//!   JSON.
//! - [`EXPOSURE_BUNDLE_FORMAT`] and [`SCHEMA_MAPPING_VERSION`], the two
//!   version gates a reader checks before trusting a bundle.
//! - [`policy`], the operational policy vocabulary shared by the document
//!   model and the bundle.

#![forbid(unsafe_code)]

pub mod bundle;
pub mod document;
pub mod policy;
pub mod schema;
pub mod validate;

pub use bundle::{
    BundleContractPin, BundleIdentity, BundleServer, EXPOSURE_BUNDLE_FORMAT, ExposureBundle,
    I64_DECIMAL_PATTERN, ResourceEntry, ResourcePolicies, SCHEMA_MAPPING_VERSION, TaskEntry,
    ToolEntry, U64_DECIMAL_PATTERN, is_canonical_i64_decimal, is_canonical_u64_decimal,
};
pub use document::{
    ActionExposure, ExposureManifest, ExposureTarget, McpExposure, PinnedContractRef, PublicName,
    RestrictBounds, ServerIdentity, ServiceExposure, TopicExposure,
};
pub use policy::{
    ActionOperation, FreshnessPolicy, ImageCodec, ImageFieldMap, ImageRepresentation, JpegQuality,
    MaxHz, OversizePolicy, ServiceOperation, UpdatePolicy,
};
pub use schema::{MaxSerializedSize, max_serialized_json_bytes, message_format_to_json_schema};
pub use validate::{
    BoundMember, ExposureValidationError, ResolvedContract, ValidatedExposure,
    build_exposure_bundle,
};
