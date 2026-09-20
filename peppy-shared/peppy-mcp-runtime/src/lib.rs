//! The shared MCP server runtime the `peppy` binary serves exposures with.
//!
//! The host process derives one exposure bundle per exposure it serves,
//! registers one bridge per exposed member, and hands them to this crate.
//! The runtime then owns everything protocol-shaped: one Streamable HTTP
//! listener on `127.0.0.1` speaking MCP `2026-07-28` with one endpoint per
//! exposure at `/<name>/<tag>/mcp`, each endpoint's catalog served through
//! `server/discover`, `tools/list`, and `resources/list` with their caching
//! hints, snapshot freshness on an injected clock, `subscriptions/listen`
//! notifications, tool-input validation against the bundle's derived
//! schemas, action-backed MCP tasks (SEP-2663) with confirmation,
//! cooperative cancellation, and whole-goal deadlines, and the mapping from
//! bridge failures to MCP errors.
//!
//! Entry points:
//!
//! - [`ExposureServer::builder`] takes a parsed
//!   [`ExposureBundle`](peppy_mcp_catalog::ExposureBundle) plus one
//!   [`ToolHandler`] per exposed service and one [`TaskHandler`] per
//!   exposed action and builds the server for one exposure.
//! - [`ExposureServer::ingest`] hands out the [`ResourceIngest`] a topic
//!   pump feeds: [`ResourceIngest::admit`] applies the update-rate gate
//!   before any decoding work, [`ResourceIngest::publish`] applies the
//!   representation and size policies and stores the snapshot.
//! - [`ExposureSet::serve`] serves every server of the set on one listener,
//!   each under its [`ExposureServer::endpoint_path`], until the supplied
//!   cancellation token fires; every other path answers 404.
//! - [`Clock`] injects the time source for freshness and rate gating; the
//!   host passes its sim-time-aware clock, tests pass a counter.

#![forbid(unsafe_code)]

pub mod bridge;
pub mod clock;
pub mod error;
pub mod representation;
pub mod server;
mod state;
pub mod tasks;

pub use clock::Clock;
pub use error::{BuildError, PublishError, ToolCallError};
pub use peppy_mcp_catalog as catalog;
pub use server::{ExposureServer, ExposureServerBuilder, ExposureSet, ToolHandler};
pub use state::{AdmitToken, ResourceIngest};
pub use tasks::{ActionContext, ActionExit, TaskHandler};
