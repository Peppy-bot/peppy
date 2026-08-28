//! The built-in MCP server on the daemon side: resolving the exposures a
//! launcher lists through this machine's caches, registering the server in
//! the node stack from pinned documents, deriving an exposure's catalog on
//! demand, and validating a hub's exposures for `peppy repo index --check
//! --validate-mcp-exposures`.
//!
//! What the server is made of (its identity, manifest and catalogs) is
//! derived by `daemon_config::mcp_deployment`; this module is the
//! repository-facing shell around it.

pub(crate) mod built_in;
mod catalog;
mod index_check;
mod resolve;
#[cfg(test)]
mod tests;

pub use built_in::{PeppyExecutable, resolve_peppy_executable};
pub use catalog::derive_exposure_catalog;
pub use index_check::{ExposureFinding, check_repository_exposures};
pub use resolve::resolve_exposure_plan;
pub(crate) use resolve::{materialize_exposure_deployment, resolve_exposure_deployment};
