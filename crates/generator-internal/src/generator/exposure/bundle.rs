//! The serializable shape of a versioned exposure bundle.
//!
//! The model lives in the shared `peppy-mcp-catalog` crate: the MCP server
//! runtime in `public-peppy-libs` parses the same bytes this generator
//! writes, so writer and reader share one definition. This module re-exports
//! it for the validator and the bundle's consumers.

pub use peppy_mcp_catalog::{
    BundleContractPin, BundleIdentity, BundleNode, BundleServer, EXPOSURE_BUNDLE_FORMAT,
    ExposureBundle, ResourceEntry, ResourcePolicies, TaskEntry, ToolEntry,
};
