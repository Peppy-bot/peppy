//! Authentication engine for `peppy login` / `logout` / `whoami`.
//!
//! `peppy` is a public OAuth client of the project's Zitadel instance and a
//! caller of the `platform-backend` resource server. The login flow is:
//!
//! 1. [`cli_config::fetch`]: `GET {api_url}/cli-config` (public) → `issuer`,
//!    `client_id`, `scopes` (sent to Zitadel verbatim).
//! 2. [`discovery::discover`]: OIDC discovery against the `issuer` to learn the
//!    `device_authorization` and `token` endpoints.
//! 3. [`device`]: RFC 8628 device grant: open the browser, poll for the token.
//! 4. [`storage`]: cache the tokens (and `issuer`/`client_id`) under
//!    `~/.peppy/conf/credentials.json5` (`0600`).
//!
//! Subsequent commands resolve a bearer via [`resolver`] (PAT env → cached token
//! → proactive refresh) and call the backend through [`client`], which refreshes
//! once on a `401` for session credentials.
//!
//! This module is the engine only; the user-facing command structs live in
//! `crate::commands::{login, logout, whoami}`.

pub mod cli_config;
pub mod client;
pub mod device;
pub mod discovery;
pub mod http;
pub mod profile;
pub mod refresh;
pub mod resolver;
pub mod storage;

pub use cli_config::CliConfig;
pub use client::Principal;
pub use resolver::{Credential, CredentialKind};
pub use storage::{Credentials, ProfileCreds};
