//! Resolving the caller's per-user zenoh-router connection for a remote (`tls/`)
//! session.
//!
//! The flow mirrors the OAuth resolver: reuse the cached router config while it
//! is fresh, otherwise fetch a new one from `POST /me/messaging-federation`
//! (refreshing the access token on a `401` via [`client::establish_messaging_federation`])
//! and cache it beside the session. The CA the router is validated against is
//! resolved CLI-side at connect time (see [`resolve_router_ca`]), **not** taken
//! from the server's response: a debug build trusts the committed dev CA embedded
//! in the binary; a release build validates against the system trust store. There
//! is no env var or runtime override — dev federation works with zero config.

use std::path::{Path, PathBuf};
use std::time::Duration;

use super::http::HttpClient;
use super::storage::{self, RouterSession};
use super::{client, resolver};
use crate::error::Result;

/// Re-resolve this many seconds before the cache-freshness deadline (mirrors the
/// OAuth refresh skew) so a slow re-pull + TLS handshake completes before the
/// cached config is treated as stale.
const REPULL_SKEW_SECS: i64 = 30;

/// The committed dev root CA, embedded into the binary **only** in debug builds
/// (`#[cfg(debug_assertions)]`) so a release binary never carries it. The CLI
/// only ever trusts a CA it shipped with, never one the server hands it. To
/// change the dev CA you change the committed `dev-ca/peppy-dev-ca.pem` fixture;
/// a release build embeds nothing and falls back to the system trust store.
#[cfg(debug_assertions)]
const EMBEDDED_DEV_CA: Option<&[u8]> = Some(include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/dev-ca/peppy-dev-ca.pem"
)));
#[cfg(not(debug_assertions))]
const EMBEDDED_DEV_CA: Option<&[u8]> = None;

/// The dialing parameters for a remote router: the `(host, port)` to connect to
/// and the client TLS material to present/validate with.
pub struct RouterEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: pmi::TlsConfig,
}

/// The router trust anchor, resolved CLI-side with zero configuration. In a debug
/// build it is the committed dev CA embedded at compile time, materialized once
/// to a stable file under the cache dir (since [`pmi::TlsConfig::client`] takes a
/// path) and that path returned. In a release build there is no embedded CA, so
/// this returns `None` and router certificates validate against the system trust
/// store. No env var, no runtime override.
pub fn resolve_router_ca() -> Option<PathBuf> {
    resolve_router_ca_from(
        EMBEDDED_DEV_CA,
        &config::consts::PeppyDirs::default().cache_dir(),
    )
}

/// Testable core of [`resolve_router_ca`]: materialize `embedded` (if any) to
/// `<cache_dir>/peppy-dev-ca.pem` and return that path; `None` embedded yields
/// `None` (system trust store). Splitting the bytes + dir out of
/// [`resolve_router_ca`] makes **both** arms unit-testable regardless of the test
/// binary's own build profile. A filesystem failure degrades to `None` (system
/// store) with a warning rather than failing the resolve.
pub fn resolve_router_ca_from(embedded: Option<&[u8]>, cache_dir: &Path) -> Option<PathBuf> {
    let bytes = embedded?;
    let path = cache_dir.join("peppy-dev-ca.pem");
    // Cheap in the steady state: only (re)write when the file is missing or its
    // contents differ from the embedded bytes (e.g. after a CA-fixture change).
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                error = %e, dir = %parent.display(),
                "router CA: could not create the cache dir to materialize the embedded \
                 dev CA; falling back to the system trust store"
            );
            return None;
        }
        if let Err(e) = std::fs::write(&path, bytes) {
            tracing::warn!(
                error = %e, path = %path.display(),
                "router CA: could not write the embedded dev CA; falling back to the \
                 system trust store"
            );
            return None;
        }
    }
    Some(path)
}

/// Resolves the caller's router connection: returns a cached endpoint while it
/// is fresh, else pulls a new config (provisioning the router on first call) and
/// caches it. `api_url` and `pat` follow the same resolution the auth commands
/// use; `ca_certificate` is the trust anchor (see [`resolve_router_ca`]) and is
/// applied fresh to the live TLS at connect time (never cached on disk).
///
/// Only the pull path needs a credential, so a fresh cache is reused without
/// touching the token at all.
pub fn resolve_router_endpoint(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    core_node: &str,
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
        _ => pull_and_cache(creds_path, http, api_url, core_node, pat, now)?,
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
/// cached `repull_after` uses the same clock reading as the freshness check. The
/// trust anchor is resolved fresh at connect time (see [`resolve_router_ca`]), so
/// it is deliberately not part of the cached `RouterSession`.
fn pull_and_cache(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    core_node: &str,
    pat: Option<String>,
    now: i64,
) -> Result<String> {
    let mut cred = resolver::resolve(creds_path, http, pat)?;
    let cfg = client::establish_messaging_federation(http, api_url, core_node, &mut cred)?;

    // Reload before caching so we don't clobber a concurrent refresh's rotation
    // (the same load-before-write discipline the token refresh uses).
    let mut creds = storage::load(creds_path)?;
    creds.router = Some(RouterSession {
        endpoint: cfg.endpoint.clone(),
        protocol: cfg.protocol.clone(),
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
/// fails, so the daemon always starts. The pull also tells the backend this
/// daemon's `core_node` name so its active liveness health-check can address the
/// `/health` service over the federated link; there is no longer a client-side
/// keepalive re-pull (the backend now probes the daemon instead).
///
/// `connect_timeout` bounds the (blocking) config pull so a slow/unreachable
/// backend can't stall the caller (federation at startup / on a login-poke)
/// beyond it; on timeout the pull errors and this returns `None`.
pub fn resolve_federation_target(
    api_url: &str,
    core_node: &str,
    connect_timeout: Duration,
) -> Option<(String, pmi::TlsConfig)> {
    resolve_federation_target_at(
        &storage::default_path(),
        api_url,
        core_node,
        resolver::pat_from_env(),
        resolve_router_ca(),
        connect_timeout,
    )
}

/// Testable core of [`resolve_federation_target`] with the creds path, PAT, CA,
/// and timeout made explicit (so it can be exercised against a stub backend
/// without touching the process-global credentials file or `PEPPY_API_KEY`).
/// Mirrors the [`super::profile::resolve_api_url`] / `resolve_api_url_from` split.
pub fn resolve_federation_target_at(
    creds_path: &Path,
    api_url: &str,
    core_node: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
    connect_timeout: Duration,
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
    let http = HttpClient::with_timeout(connect_timeout);
    match resolve_router_endpoint(creds_path, &http, api_url, core_node, pat, ca_certificate) {
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

/// Client trust material: validate the router against the resolved trust anchor
/// (or the system store if `None`), with name verification on — the dialed
/// capability host must match the router certificate. No client certificate (the
/// gateway is SNI-passthrough; the CLI is not doing mTLS).
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
    fn resolve_router_ca_from_materializes_embedded_bytes() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let bytes: &[u8] = b"-----BEGIN CERTIFICATE-----\ndev\n-----END CERTIFICATE-----\n";
        let path = resolve_router_ca_from(Some(bytes), tmp.path()).expect("materialized");
        assert_eq!(path, tmp.path().join("peppy-dev-ca.pem"));
        assert_eq!(std::fs::read(&path).expect("read back"), bytes);
        // Idempotent: a second call returns the same path without changing it.
        let again = resolve_router_ca_from(Some(bytes), tmp.path()).expect("materialized again");
        assert_eq!(again, path);
        assert_eq!(std::fs::read(&path).expect("read back"), bytes);
    }

    #[test]
    fn resolve_router_ca_from_none_is_system_store() {
        let tmp = tempfile::tempdir().expect("temp dir");
        assert!(
            resolve_router_ca_from(None, tmp.path()).is_none(),
            "no embedded CA ⇒ system trust store"
        );
        assert!(
            !tmp.path().join("peppy-dev-ca.pem").exists(),
            "nothing is written when there is no embedded CA"
        );
    }

    /// Breaking-change guard: the legacy router-CA env override and the
    /// env-reading CA helper are gone for good — the router CA reads no env var.
    /// The needles are assembled from fragments (and the prose avoids spelling
    /// them) so this assertion never matches its own source text.
    #[test]
    fn legacy_env_var_and_helper_stay_removed() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/auth/router.rs"));
        let env_needle = concat!("PEPPY_ROUTER_", "CA_CERT");
        let helper_needle = concat!("ca_", "from_env");
        assert!(
            !src.contains(env_needle),
            "the legacy router-CA env override must stay removed"
        );
        assert!(
            !src.contains(helper_needle),
            "the legacy env-reading CA helper must stay removed"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    fn debug_build_embeds_the_committed_dev_ca() {
        // In a debug build the embedded CA is exactly the committed fixture, and
        // it is materialized verbatim to the cache path.
        let committed: &[u8] = include_bytes!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/dev-ca/peppy-dev-ca.pem"
        ));
        assert_eq!(EMBEDDED_DEV_CA, Some(committed));

        let tmp = tempfile::tempdir().expect("temp dir");
        let path = resolve_router_ca_from(EMBEDDED_DEV_CA, tmp.path()).expect("debug embeds a CA");
        assert_eq!(std::fs::read(&path).expect("read back"), committed);
    }

    #[test]
    fn seconds_clamp_into_i64() {
        assert_eq!(saturating_secs_to_i64(3000), 3000);
        assert_eq!(saturating_secs_to_i64(u64::MAX), i64::MAX);
    }
}
