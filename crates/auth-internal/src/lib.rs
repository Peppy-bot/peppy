//! Platform engine for the `peppy platform` commands and the daemon's router
//! federation.
//!
//! `peppy` is a public OAuth client of the platform's Zitadel instance and a
//! caller of the `platform-backend` resource server. Signing in is:
//!
//! 1. [`cli_config::fetch`]: `GET {api_url}/cli/auth-config` (public) gives the
//!    `issuer`, `client_id` and `scopes` (sent to Zitadel verbatim), and the
//!    platform's own device page, `device_verification_uri`; a platform without
//!    one is refused.
//! 2. [`discovery::discover`]: OIDC discovery against the `issuer` to learn the
//!    `device_authorization`, `token` and `revocation` endpoints.
//! 3. [`device`]: RFC 8628 device grant: start the flow, build the link a
//!    person approves it on ([`device::verification_link`]), poll for the token.
//! 4. [`storage`]: cache the tokens (and `issuer`/`client_id`) under
//!    `~/.peppy/conf/credentials.json5` (`0600`).
//!
//! Later commands resolve a bearer via [`resolver`] (the cached session,
//! refreshed proactively when it is about to expire) and call the backend
//! through [`client::PlatformApi`], which refreshes once on a `401`. Signing
//! out revokes both tokens at the issuer ([`revoke`]).
//!
//! Joining a project's cloud router is a separate step from signing in:
//! [`csr`] mints this machine's key pair and certificate signing request,
//! [`client::PlatformApi::enroll_peer`] exchanges the request for a signed
//! certificate, and
//! [`enrollment`] persists the result under `~/.peppy/conf/peer/`. The daemon
//! reads that directory at startup. It keeps the certificate valid with
//! [`renewal`], which asks the platform for a new leaf with the bearer of the
//! cached session.
//!
//! The workspace and the project the person selected as the default target of
//! the commands are the [`selection`], a third file with its own lifecycle.
//!
//! # Boundary with consumer crates
//!
//! This crate owns everything non-interactive: credential and enrollment
//! storage, credential resolution, the blocking HTTP client, the device-flow
//! *protocol* (start and poll), backend URL resolution, and the typed platform
//! API. Consumers (the `peppy` CLI and the daemon) own every interactive or
//! process-level concern: the user-facing command structs, printing the
//! verification URL, opening the browser, spinners and prompts, clap dispatch,
//! and logging initialization.

#![forbid(unsafe_code)]

pub mod cli_config;
pub mod client;
pub mod csr;
pub mod device;
pub mod discovery;
mod document;
pub mod enrollment;
mod error;
mod fs_perms;
pub mod http;
pub mod profile;
pub mod refresh;
pub mod renewal;
pub mod resolver;
pub mod revoke;
pub mod selection;
pub mod storage;
#[cfg(any(test, feature = "test-support"))]
pub mod test_support;

pub use cli_config::CliConfig;
pub use client::{PlatformApi, Principal};
pub use enrollment::{
    CertificateValidity, Enrollment, EnrollmentBundle, EnrollmentDocument, FederationIdentity,
    IssuedMaterial, RouterEndpoint,
};
pub use error::{Error as AuthError, Problem, ProblemKind, Result};
pub use resolver::Credential;
pub use selection::PlatformSelection;
pub use storage::{Credentials, ProfileCreds};
