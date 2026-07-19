//! Authentication engine for `peppy platform login` / `logout` / `whoami` and the
//! daemon's router federation.
//!
//! `peppy` is a public OAuth client of the project's Zitadel instance and a
//! caller of the `platform-backend` resource server. The login flow is:
//!
//! 1. [`cli_config::fetch`]: `GET {api_url}/cli/auth-config` (public) → `issuer`,
//!    `client_id`, `scopes` (sent to Zitadel verbatim).
//! 2. [`discovery::discover`]: OIDC discovery against the `issuer` to learn the
//!    `device_authorization` and `token` endpoints.
//! 3. [`device`]: RFC 8628 device grant: start the flow, poll for the token.
//! 4. [`storage`]: cache the tokens (and `issuer`/`client_id`) under
//!    `~/.peppy/conf/credentials.json5` (`0600`).
//!
//! Subsequent commands resolve a bearer via [`resolver`] (PAT env → cached token
//! → proactive refresh) and call the backend through [`client`], which refreshes
//! once on a `401` for session credentials.
//!
//! # Boundary with consumer crates
//!
//! This crate owns everything non-interactive: credential storage and
//! resolution, the blocking HTTP client, the device-flow *protocol* (start and
//! poll), backend URL resolution, workspace-namespace and federation-target resolution, and
//! the router-config cache (plus the debug-only embedded dev-CA/client-cert
//! material). Consumers (the `peppy` CLI and the daemon) own every interactive
//! or process-level concern: the user-facing command structs, printing the
//! verification URL, opening the browser, spinners and prompts, clap dispatch,
//! and logging initialization.

#![forbid(unsafe_code)]

pub mod cli_config;
pub mod client;
pub mod device;
pub mod discovery;
mod error;
pub mod http;
pub mod identity;
pub mod profile;
pub mod refresh;
pub mod resolver;
pub mod router;
pub mod storage;

pub use cli_config::CliConfig;
pub use client::Principal;
pub use error::{Error as AuthError, Result};
pub use identity::{CoreNodeIdentity, IdentityPaths, IdentityRotation};
pub use resolver::{Credential, CredentialKind};
pub use storage::{Credentials, ProfileCreds, RouterSession};
