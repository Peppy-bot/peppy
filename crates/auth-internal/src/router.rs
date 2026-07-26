//! Resolving the caller's per-user zenoh-router connection for a remote (`tls/`)
//! session.
//!
//! The flow mirrors the OAuth resolver: reuse the cached router config while it
//! is fresh, otherwise fetch a new one from `POST /me/cli/federation`
//! (refreshing the access token on a `401` via [`client::establish_federation`])
//! and cache it beside the session. The CA the router is validated against is
//! resolved CLI-side at connect time (see [`resolve_router_ca`]), **not** taken
//! from the server's response: a debug build trusts the committed dev CA embedded
//! in the binary; a release build validates against the system trust store. There
//! is no env var or runtime override; dev federation works with zero config.
//!
//! The link is one-way TLS in both build profiles: the router proves its
//! identity and we verify it, but we present no certificate of our own. Only the
//! trust anchor differs between debug and release, which is why both profiles
//! exercise the same transport.

use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon_config::consts::PeppyDirs;

use super::http::HttpClient;
use super::storage::{self, RouterSession};
use super::{client, resolver};
use crate::error::{Error, Result};

/// Re-resolve this many seconds before the cache-freshness deadline (mirrors the
/// OAuth refresh skew) so a slow re-pull + TLS handshake completes before the
/// cached config is treated as stale.
const REPULL_SKEW_SECS: i64 = 30;

/// The only router transport the CLI dials. The federation connect target is
/// always built as `tls/<host>:<port>` (see [`resolve_federation_target_at`]), so a
/// config advertising any other transport is rejected at pull time rather than
/// cached and then silently dialed over TLS anyway.
const SUPPORTED_ROUTER_PROTOCOL: &str = "tls";

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
/// and the client TLS material to validate it with.
pub struct RouterEndpoint {
    pub host: String,
    pub port: u16,
    pub tls: pmi::TlsConfig,
    /// The namespace this endpoint was resolved for, carried out of the same
    /// pull so the caller derives the federation gate and session namespace
    /// from one validated value.
    pub namespace: config::namespace::Namespace,
}

/// The router trust anchor, resolved client-side with zero configuration. In a
/// debug build it is the committed dev CA embedded at compile time, materialized
/// once to a stable file under `cache_dir` (since [`pmi::TlsConfig::client`]
/// takes a path) and that path returned. In a release build there is no embedded
/// CA, so this returns `None` and router certificates validate against the
/// system trust store. No env var, no runtime override. The caller supplies the
/// cache dir (its resolved [`PeppyDirs::cache_dir`]) so the resolve never
/// reaches for the process-global default root.
pub fn resolve_router_ca(cache_dir: &Path) -> Option<PathBuf> {
    resolve_router_ca_from(EMBEDDED_DEV_CA, cache_dir)
}

/// Testable core of [`resolve_router_ca`]: materialize `embedded` (if any) to
/// `<cache_dir>/peppy-dev-ca.pem` and return that path; `None` embedded yields
/// `None` (system trust store). Splitting the bytes + dir out of
/// [`resolve_router_ca`] makes **both** arms unit-testable regardless of the test
/// binary's own build profile. A filesystem failure degrades to `None` (system
/// store) with a warning rather than failing the resolve.
pub fn resolve_router_ca_from(embedded: Option<&[u8]>, cache_dir: &Path) -> Option<PathBuf> {
    materialize_embedded(cache_dir, "peppy-dev-ca.pem", embedded?)
}

/// Materialize `bytes` to `<cache_dir>/<filename>` and return that path. Cheap in
/// the steady state: only (re)writes when the file is missing or its contents differ
/// (e.g. after a fixture change). A filesystem failure degrades to `None` with a
/// warning rather than failing the resolve.
fn materialize_embedded(cache_dir: &Path, filename: &str, bytes: &[u8]) -> Option<PathBuf> {
    let path = cache_dir.join(filename);
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                error = %e, dir = %parent.display(),
                "router TLS: could not create the cache dir to materialize embedded dev \
                 material ({filename}); falling back to the system trust store"
            );
            return None;
        }
        if let Err(e) = std::fs::write(&path, bytes) {
            tracing::warn!(
                error = %e, path = %path.display(),
                "router TLS: could not write embedded dev material ({filename}); falling back"
            );
            return None;
        }
    }
    Some(path)
}

/// Resolves the caller's router connection: returns a cached endpoint while it
/// is fresh, else pulls a new config and caches it. `api_url` and `pat` follow the
/// same resolution the auth commands use; `ca_certificate` (the trust anchor, see
/// [`resolve_router_ca`]) is applied fresh to the live TLS at connect
/// time (never cached on disk). `core_node_name` is the daemon's core-node name,
/// carried in the pull's POST body to register the daemon in the backend's
/// core-node registry (see [`client::establish_federation`]); it also
/// tags the cache, so a resolve under a different name (a renamed daemon) always
/// re-pulls and re-registers rather than reusing a still-fresh cache.
/// `router_zid` is the managed router's pinned transport identity, carried in
/// the same POST and tagging the cache the same way.
///
/// Only the pull path needs a credential, so a fresh cache is reused without
/// touching the token at all.
pub fn resolve_router_endpoint(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
    core_node_name: &str,
    router_zid: &pmi::RouterId,
) -> Result<RouterEndpoint> {
    let now = storage::now_unix();
    // Load once: the cached router config and the active session's subject come
    // from the same snapshot, so the identity check below sees a consistent view.
    let creds = storage::load(creds_path)?;
    let active_subject = creds
        .session
        .as_ref()
        .map(|s| s.subject.clone())
        .unwrap_or_default();
    // The cache is identity-bound two ways: `login`/`logout` clear it with the
    // session, AND `RouterSession.subject` tags it with the backend identity it was
    // pulled for. Reuse the cached endpoint (and its `namespace`) only for a
    // *session* resolve (no active PAT) whose non-empty subject still matches the
    // cache, so a config pulled under one identity is never replayed under another
    // (e.g. a PAT-pulled workspace leaking onto the on-disk session once the PAT is gone).
    // An active PAT always re-pulls: a PAT is bound to its own backend subject at
    // pull time (see `pull_and_cache`), which the session-derived `active_subject`
    // cannot match on this fast path, and re-pulling is cheap (federation resolves
    // only at startup and on a login/logout poke).
    //
    // The cache is additionally bound to the core-node name it was pulled under
    // (`rs.core_node_name`): the pull's POST is what registers the daemon in the
    // backend's core-node registry, so a renamed daemon (the `CoreNodeNameTaken`
    // collision-fix workflow: set `core_node_name`, restart) must re-pull — and
    // thereby register its new name — instead of reusing a still-fresh cache and
    // staying absent from the registry until the cache goes stale.
    //
    // The router zid deliberately does NOT tag the cache. It changes only when
    // the workspace changes (which clears this cache with the session) or when
    // the identity record is lost, and adding it to the cached `RouterSession`
    // would invalidate every credentials file on disk to close that second,
    // narrow case. A re-minted identity simply re-registers on the next stale
    // pull; until then the row reads `indirect`, which is a less specific answer
    // rather than a wrong one.
    let reuse_cache = pat.is_none() && !active_subject.is_empty();
    let (endpoint, namespace) = match creds.router {
        Some(rs)
            if reuse_cache
                && !rs.is_stale(now, REPULL_SKEW_SECS)
                && rs.subject == active_subject
                && rs.core_node_name == core_node_name =>
        {
            (rs.endpoint, rs.namespace)
        }
        _ => pull_and_cache(
            creds_path,
            http,
            api_url,
            pat,
            now,
            core_node_name,
            router_zid,
        )?,
    };

    let (host, port) = client::split_locator(&endpoint)?;
    Ok(RouterEndpoint {
        host,
        port,
        tls: client_tls(ca_certificate),
        namespace,
    })
}

/// Pulls a fresh router config (refreshing the token on a 401), caches it beside
/// the session, and returns the endpoint locator. `now` is threaded in so the
/// cached `repull_after` uses the same clock reading as the freshness check. The
/// trust anchor is resolved fresh at connect time (see [`resolve_router_ca`]), so
/// it is deliberately not part of the cached `RouterSession`. The pull identifies
/// the daemon by `core_node_name` and `router_zid` (the POST body), upserting
/// both into the backend's core-node registry.
fn pull_and_cache(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    now: i64,
    core_node_name: &str,
    router_zid: &pmi::RouterId,
) -> Result<(String, config::namespace::Namespace)> {
    let mut cred = resolver::resolve(creds_path, http, pat)?;
    // The identity this pull is actually authenticated as drives the cache tag
    // below. A PAT is not the on-disk session, so it must not be tagged with the
    // session subject; doing so would let the session reuse the PAT's workspace once the
    // PAT is gone (a cross-identity leak).
    let is_pat = matches!(cred.kind, resolver::CredentialKind::Pat);
    let cfg = client::establish_federation(http, api_url, &mut cred, core_node_name, router_zid)?;

    // Validate the config *before* it is written to `creds.router`: a malformed
    // endpoint or an unsupported transport must not poison the on-disk
    // `RouterSession`. A poisoned cache would otherwise re-fail `split_locator` on
    // every reuse until it goes stale (instead of being re-pulled). The connect
    // target is always `tls/<host>:<port>`, so reject any other advertised
    // transport here rather than caching it and dialing TLS anyway.
    if cfg.protocol != SUPPORTED_ROUTER_PROTOCOL {
        return Err(Error::Auth(format!(
            "router config advertised unsupported transport {:?}; only \
             `{SUPPORTED_ROUTER_PROTOCOL}` is supported",
            cfg.protocol
        )));
    }
    // Parse the locator now (the same check the connect path does) so an
    // unparseable endpoint is rejected before it is persisted.
    client::split_locator(&cfg.endpoint)?;

    // Reload before caching so we don't clobber a concurrent refresh's rotation
    // (the same load-before-write discipline the token refresh uses).
    let mut creds = storage::load(creds_path)?;
    // Tag the cache with the backend identity the config was pulled for so a stale
    // cache that outlives an identity change is re-pulled (see
    // `resolve_router_endpoint`). For a session that is the session subject; for a
    // PAT it is the PAT owner's stable, non-secret subject from the backend (`/me`);
    // never the session subject (which is a different identity) or an empty
    // string (which an empty active subject would spuriously match).
    let subject = if is_pat {
        client::get_me(http, api_url, &mut cred)?.sub
    } else {
        creds
            .session
            .as_ref()
            .map(|s| s.subject.clone())
            .unwrap_or_default()
    };
    creds.router = Some(RouterSession {
        endpoint: cfg.endpoint.clone(),
        protocol: cfg.protocol.clone(),
        repull_after: now.saturating_add(saturating_secs_to_i64(cfg.reconnect_after_secs)),
        namespace: cfg.namespace.clone(),
        subject,
        // Tag the cache with the name this pull registered, so a rename forces
        // a re-pull (and re-registration) even while the cache is still fresh.
        core_node_name: core_node_name.to_string(),
    });
    storage::save(creds_path, &creds)?;
    Ok((cfg.endpoint, cfg.namespace))
}

/// Best-effort federation target for the daemon's *local* router: the upstream
/// `tls/<host>:<port>` connect endpoint plus the connect-side trust anchor,
/// resolved by pulling the shared router's connection config.
///
/// Returns `None`, and the local router stays standalone (plaintext-only), when
/// the user is not logged in, no backend is configured/reachable, or the pull
/// fails, so the daemon always starts. The daemon dials the returned endpoint over
/// one-way TLS, verifying the router's certificate and name and presenting none of
/// its own; there is no client-side keepalive re-pull.
///
/// `connect_timeout` bounds the (blocking) config pull so a slow/unreachable
/// backend can't stall the caller (federation at startup / on a login-poke)
/// beyond it; on timeout the pull errors and this returns `None`.
///
/// `core_node_name` is the daemon's core-node name and `router_zid` its managed
/// router's pinned transport identity; both are sent in every pull's POST body
/// so the backend registry records which daemon federated (and when it last
/// pulled), and can match this site against the shared router's session list.
///
/// `peppy_dirs` is the caller's resolved peppy data root: the credentials file
/// and the materialized dev TLS material both derive from it, so a daemon
/// running under an injected root (a test) never reads the machine's real
/// peppy home. Only the PAT stays ambient (`PEPPY_API_KEY` is an explicit
/// operator override, read here).
pub fn resolve_federation_target(
    peppy_dirs: &PeppyDirs,
    api_url: &str,
    connect_timeout: Duration,
    core_node_name: &str,
    router_zid: &pmi::RouterId,
) -> Option<(String, pmi::TlsConfig)> {
    let cache_dir = peppy_dirs.cache_dir();
    resolve_federation_target_at(
        &storage::credentials_path(peppy_dirs),
        api_url,
        resolver::pat_from_env(),
        resolve_router_ca(&cache_dir),
        connect_timeout,
        core_node_name,
        router_zid,
    )
}

/// Testable core of [`resolve_federation_target`] with the creds path, PAT, CA,
/// and timeout made explicit (so it can be exercised against a stub backend
/// without touching the process-global credentials file or `PEPPY_API_KEY`).
/// Mirrors the [`super::profile::resolve_api_url`] / `resolve_api_url_from` split.
pub fn resolve_federation_target_at(
    creds_path: &Path,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
    connect_timeout: Duration,
    core_node_name: &str,
    router_zid: &pmi::RouterId,
) -> Option<(String, pmi::TlsConfig)> {
    // Skip the network entirely when there is plainly no identity to pull for, so
    // an un-provisioned dev box does not log a spurious auth error every poll. Take
    // that fast path only on a *definitive* no-identity (no PAT and a clean load
    // that returns no session). A real credential *read* failure is not "no
    // identity": log it and fall through rather than swallowing it into a silent
    // logged-out state, so the resolve path below surfaces the underlying error.
    if pat.is_none() {
        match storage::load(creds_path) {
            Ok(creds) if creds.session.is_none() => return None,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "router federation: could not read stored credentials to check login \
                     state; not taking the logged-out fast path"
                );
            }
        }
    }
    let http = HttpClient::with_timeout(connect_timeout);
    match resolve_router_endpoint(
        creds_path,
        &http,
        api_url,
        pat,
        ca_certificate,
        core_node_name,
        router_zid,
    ) {
        // Fail-closed gate, single source: only a validated, non-local namespace
        // federates. The daemon session is resolved from the same cached value.
        Ok(ep) if !ep.namespace.is_local() => {
            Some((format!("tls/{}:{}", ep.host, ep.port), ep.tls))
        }
        Ok(ep) => {
            tracing::warn!(
                namespace = %ep.namespace,
                "router federation: local namespace keeps the router standalone"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "router federation: config pull failed; local router stays standalone"
            );
            None
        }
    }
}

/// Resolve the daemon's session namespace from credentials. A missing file or a
/// valid document without a router session is legitimate standalone operation
/// and resolves to `local`; malformed, rejected, unreadable, or invalid state
/// propagates as an error.
pub fn session_namespace(creds_path: &Path) -> Result<config::namespace::Namespace> {
    Ok(storage::load(creds_path)?
        .router
        .map(|router| router.namespace)
        .unwrap_or_else(config::namespace::Namespace::local))
}

/// Client TLS material for dialing the shared router: validate it against the
/// resolved trust anchor (`ca_certificate`, or the system store if `None`), with
/// name verification on, so the dialed host must match the router's certificate
/// SAN. The link is one-way: we present no certificate of our own, in either build
/// profile.
///
/// `TlsConfig::client` sets `root_ca_certificate` and leaves
/// `verify_name_on_connect` on (its default); `default` trusts the system store.
fn client_tls(ca_certificate: Option<PathBuf>) -> pmi::TlsConfig {
    match ca_certificate {
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
    fn client_tls_pins_the_resolved_ca_and_verifies_the_router_name() {
        let tls = client_tls(Some(PathBuf::from("/etc/peppy/ca.pem")));
        assert_eq!(
            tls.root_ca_certificate.as_deref(),
            Some(std::path::Path::new("/etc/peppy/ca.pem"))
        );
        assert!(tls.verify_name_on_connect, "name verification must stay on");
    }

    #[test]
    fn client_tls_without_ca_falls_back_to_system_store() {
        // A release build embeds no dev CA, so the router validates against the
        // system trust store. Name verification stays on either way: that is the
        // only difference between the two profiles.
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
    /// env-reading CA helper are gone for good; the router CA reads no env var.
    /// The needles are assembled from fragments (and the prose avoids spelling
    /// them) so this assertion never matches its own source text.
    #[test]
    fn legacy_env_var_and_helper_stay_removed() {
        let src = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/router.rs"));
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
