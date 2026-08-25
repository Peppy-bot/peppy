//! `peppy mcp serve`: the built-in MCP server.
//!
//! The daemon starts one such process per `exposures` deployment and hands
//! it a serve spec (the pinned exposure and contract documents) through
//! [`daemon_config::mcp_deployment::SPEC_ENV_VAR`], beside the runtime
//! config every node receives. From those documents alone the process
//! derives the deployment's manifest and catalogs, runs as the node the
//! daemon planned (its contract slots filled by the launcher's `links`),
//! and serves each exposure at `/<name>/<tag>/mcp` on one loopback port.
//!
//! Message conversion is the runtime codec of `message-codec`: the layout
//! generated nodes use, laid out from each contract's `message_format`
//! when the process starts.

mod bridges;
mod serve;

pub use serve::{ServeError, serve};
