//! Resolving the caller's per-user zenoh-router connection for a remote (`tls/`)
//! session.
//!
//! The flow mirrors the OAuth resolver: reuse the cached router config while it
//! is fresh, otherwise pull a new one from `GET /me/zenoh-router-config`
//! (refreshing the access token on a `401` via [`client::get_zenoh_router_config`])
//! and cache it beside the session. The CA the router is validated against is
//! CLI-side deployment config (the trust root the routers' certificates chain
//! to), **not** part of the server's response — it comes from
//! `PEPPY_ROUTER_CA_CERT` and is always taken fresh at connect time (the cached
//! endpoint may be reused, but the trust root is never stale).

use std::path::{Path, PathBuf};

use super::http::HttpClient;
use super::storage::{self, RouterSession};
use super::{client, resolver};
use crate::error::Result;

/// Re-pull this many seconds before the server's deadline (mirrors the OAuth
/// refresh skew) so a slow pull + TLS handshake still lands inside the live
/// window the server's reaper honours.
const REPULL_SKEW_SECS: i64 = 30;

/// The dialing parameters for a remote router: the `(host, port)` to connect to
/// and the client TLS material to present/validate with.
pub struct RouterEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: pmi::TlsConfig,
}

/// The deployment trust root from `PEPPY_ROUTER_CA_CERT`, if set. `None` falls
/// back to the system trust store (only viable when the router presents a
/// publicly-trusted certificate — a dev `fastcert`/private CA must set this).
pub fn ca_from_env() -> Option<PathBuf> {
    std::env::var_os("PEPPY_ROUTER_CA_CERT")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// Resolves the caller's router connection: returns a cached endpoint while it
/// is fresh, else pulls a new config (provisioning the router on first call) and
/// caches it. `api_url` and `pat` follow the same resolution the auth commands
/// use; `ca_certificate` is the deployment trust root (see [`ca_from_env`]).
///
/// Only the pull path needs a credential, so a fresh cache is reused without
/// touching the token at all.
pub fn resolve_router_endpoint(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
) -> Result<RouterEndpoint> {
    let now = storage::now_unix();
    let cached = storage::load(creds_path)?.router;
    // The cache is identity-bound: `login`/`logout` clear it with the session
    // (`storage::Credentials` doc). The fresh-cache branch reuses the endpoint
    // without re-resolving a credential, which is fine for the daemon's
    // federation poll (`resolve_federation_target`): a fresh cache means the
    // upstream is unchanged, so no re-pull (and no last_seen refresh) is needed
    // until it goes stale. FOLLOW-UP: tag the cached `RouterSession` with the
    // identity it was pulled for and verify it on reuse, so a cache that survives
    // an identity change is re-pulled rather than reused. A blanket "require a
    // session" guard is wrong here — it would disable caching for a
    // `PEPPY_API_KEY` PAT, which has no session.
    let endpoint = match cached {
        Some(rs) if !rs.is_stale(now, REPULL_SKEW_SECS) => rs.endpoint,
        _ => pull_and_cache(creds_path, http, api_url, pat, ca_certificate.as_ref(), now)?,
    };

    let (host, port) = client::split_locator(&endpoint)?;
    Ok(RouterEndpoint {
        host,
        port,
        tls: client_tls(ca_certificate),
    })
}

/// Pulls a fresh router config (refreshing the token on a 401), caches it beside
/// the session, and returns the endpoint locator. `now` is threaded in so the
/// cached `repull_after` uses the same clock reading as the freshness check.
fn pull_and_cache(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<&PathBuf>,
    now: i64,
) -> Result<String> {
    let mut cred = resolver::resolve(creds_path, http, pat)?;
    let cfg = client::get_zenoh_router_config(http, api_url, &mut cred)?;

    // Reload before caching so we don't clobber a concurrent refresh's rotation
    // (the same load-before-write discipline the token refresh uses).
    let mut creds = storage::load(creds_path)?;
    creds.router = Some(RouterSession {
        endpoint: cfg.endpoint.clone(),
        protocol: cfg.protocol.clone(),
        ca_certificate: ca_certificate.map(|p| p.to_string_lossy().into_owned()),
        repull_after: now.saturating_add(saturating_secs_to_i64(cfg.reconnect_after_secs)),
    });
    storage::save(creds_path, &creds)?;
    Ok(cfg.endpoint)
}

/// Best-effort federation target for the daemon's *local* router: the upstream
/// `tls/<host>:<port>` connect endpoint plus the connect-side trust, resolved by
/// pulling the caller's per-user router config (provisioning it on first pull).
///
/// Returns `None` — and the local router stays standalone (plaintext-only) — when
/// the user is not logged in, no backend is configured/reachable, or the pull
/// fails, so the daemon always starts. The pull doubles as the cloud router's
/// idle keepalive (it refreshes `last_seen_at` server-side); the daemon's
/// periodic re-resolve sustains it (and re-provisions a reaped router).
pub fn resolve_federation_target(api_url: &str) -> Option<(String, pmi::TlsConfig)> {
    let pat = std::env::var("PEPPY_API_KEY")
        .ok()
        .filter(|v| !v.is_empty());
    resolve_federation_target_at(&storage::default_path(), api_url, pat, ca_from_env())
}

/// Testable core of [`resolve_federation_target`] with the creds path, PAT, and
/// CA made explicit (so it can be exercised against a stub backend without
/// touching the process-global credentials file or `PEPPY_API_KEY`). Mirrors the
/// [`super::profile::resolve_api_url`] / `resolve_api_url_from` split.
pub fn resolve_federation_target_at(
    creds_path: &Path,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
) -> Option<(String, pmi::TlsConfig)> {
    // Skip the network entirely when there is plainly no identity to pull for, so
    // an un-provisioned dev box does not log a spurious auth error every poll.
    let logged_in = pat.is_some()
        || storage::load(creds_path)
            .map(|c| c.session.is_some())
            .unwrap_or(false);
    if !logged_in {
        return None;
    }
    let http = HttpClient::new();
    match resolve_router_endpoint(creds_path, &http, api_url, pat, ca_certificate) {
        Ok(ep) => Some((format!("tls/{}:{}", ep.host, ep.port), ep.tls)),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "router federation: config pull failed; local router stays standalone"
            );
            None
        }
    }
}

/// Client trust material: validate the router against the deployment CA (or the
/// system store if unset), with name verification on — the dialed capability
/// host must match the router certificate. No client certificate (the gateway
/// is SNI-passthrough; the CLI is not doing mTLS).
fn client_tls(ca_certificate: Option<PathBuf>) -> pmi::TlsConfig {
    match ca_certificate {
        // `TlsConfig::client` sets `root_ca_certificate` and leaves
        // `verify_name_on_connect` on (its default).
        Some(ca) => pmi::TlsConfig::client(ca),
        None => pmi::TlsConfig::default(),
    }
}

/// Clamps a `u64` seconds value into the `i64` unix-time domain so an absurd
/// `reconnect_after_secs` can't wrap `repull_after` negative.
fn saturating_secs_to_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_tls_with_ca_keeps_name_verification_on() {
        let tls = client_tls(Some(PathBuf::from("/etc/peppy/ca.pem")));
        assert_eq!(
            tls.root_ca_certificate.as_deref(),
            Some(std::path::Path::new("/etc/peppy/ca.pem"))
        );
        assert!(tls.verify_name_on_connect, "name verification must stay on");
        assert!(!tls.enable_mtls, "the CLI does not do mTLS");
        assert!(tls.connect_certificate.is_none());
    }

    #[test]
    fn client_tls_without_ca_falls_back_to_system_store() {
        let tls = client_tls(None);
        assert!(tls.root_ca_certificate.is_none());
        assert!(tls.verify_name_on_connect);
    }

    #[test]
    fn seconds_clamp_into_i64() {
        assert_eq!(saturating_secs_to_i64(3000), 3000);
        assert_eq!(saturating_secs_to_i64(u64::MAX), i64::MAX);
    }
}
