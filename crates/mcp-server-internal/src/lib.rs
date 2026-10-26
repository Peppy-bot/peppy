//! `peppy mcp serve`: the built-in MCP server.
//!
//! The daemon starts one such process per `exposures` deployment and hands
//! it a serve spec (the pinned exposure and contract documents) through
//! [`daemon_config::mcp_deployment::SPEC_ENV_VAR`], beside the runtime
//! config every node receives. From those documents alone the process
//! derives the deployment's manifest and catalogs, runs as the node the
//! daemon planned (its contract slots filled by the launcher's `links`, or
//! by the robots of the stack on a per-robot surface), and serves each
//! exposure at `/<name>/<tag>/mcp` on one loopback port. The tools of a
//! daemon target call the daemon that serves its interface, within the
//! scope the launch gave the target, and each call of a member that moves a
//! robot keeps the fleet rules of the stack ([`MoveRules`], which `peppy
//! node call` keeps too).
//!
//! Message conversion is the runtime codec of `peppy-codec`: the layout
//! generated nodes use, laid out from each contract's `message_format`
//! when the process starts.

mod bridges;
mod daemon_scopes;
mod fleet;
mod moves;
mod serve;
#[cfg(test)]
mod test_support;

pub use moves::{FleetStop, MarkedGoal, MoveClearance, MoveRefused, MoveRules};
pub use serve::{ServeError, serve};
