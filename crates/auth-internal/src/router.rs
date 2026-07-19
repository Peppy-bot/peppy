//! Resolving the caller's platform-router connection for a remote (`tls/`)
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

use std::path::{Path, PathBuf};
use std::time::Duration;

use daemon_config::consts::PeppyDirs;
use daemon_config::peppy_config::ParsedEndpointBuf;
use secrecy::ExposeSecret;

use super::http::HttpClient;
use super::storage::{self, RouterSession};
use super::{client, resolver};
use crate::error::{Error, Result};

/// Re-resolve this many seconds before the cache-freshness deadline (mirrors the
/// OAuth refresh skew) so a slow re-pull + TLS handshake completes before the
/// cached config is treated as stale.
const REPULL_SKEW_SECS: i64 = 30;

/// The only router transport the CLI dials: every router endpoint must parse
/// as a `tls/<host>:<port>` locator (see [`parse_router_endpoint`]), so a
/// config advertising any other transport is rejected at pull time rather than
/// cached and then silently dialed over TLS anyway.
const SUPPORTED_ROUTER_PROTOCOL: &str = "tls";

/// Parses a router endpoint locator with the daemon's strict endpoint grammar
/// (`tls/<host>:<port>`; wildcard hosts, whitespace, and config/metadata
/// suffixes rejected; IPv6 hosts come back unbracketed). The one grammar for
/// the whole locator surface: what the backend advertises is validated here,
/// at the pull boundary, exactly as the daemon will dial it, so an endpoint
/// can never be cached and only fail once it is federated to.
pub fn parse_router_endpoint(
    endpoint: &str,
) -> Result<daemon_config::peppy_config::ParsedEndpointBuf> {
    daemon_config::peppy_config::ParsedEndpointBuf::parse(endpoint, SUPPORTED_ROUTER_PROTOCOL)
        .map_err(|error| Error::Auth(format!("malformed router endpoint {endpoint:?}: {error}")))
}

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

/// The committed dev **client** leaf + key the CLI presents for mTLS, embedded the
/// same debug-only way as [`EMBEDDED_DEV_CA`]. A release binary carries neither
/// and instead requires the enrolled exact-name production identity from
/// [`crate::identity`]. The development router requires a client cert signed by
/// the isolated dev CA; these fixtures are never trusted in production.
#[cfg(debug_assertions)]
const EMBEDDED_DEV_CLIENT_CERT: Option<&[u8]> = Some(include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/dev-ca/peppy-dev-client.pem"
)));
#[cfg(debug_assertions)]
const EMBEDDED_DEV_CLIENT_KEY: Option<&[u8]> = Some(include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/dev-ca/peppy-dev-client-key.pem"
)));

/// The dialing parameters for a remote router: the parsed `tls` endpoint to
/// connect to and the client TLS material to present/validate with.
pub struct RouterEndpoint {
    pub endpoint: ParsedEndpointBuf,
    pub tls: pmi::TlsConfig,
    /// The namespace this endpoint was resolved for (the backend's
    /// `workspace_id`, validated at the HTTP boundary), carried out of the
    /// same pull so the caller derives the federation gate and the session
    /// namespace from one source.
    pub namespace: config::namespace::Namespace,
    /// Absolute validity bound from the fully validated enrolled leaf. `None`
    /// exists only for the explicit debug shared-certificate identity.
    pub certificate_not_after: Option<i64>,
}

/// Complete client-auth material for one immutable identity generation.
/// `workspace_id`/`subject` are present for enrolled production identities and
/// absent only for the explicit debug shared-certificate path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouterClientIdentity {
    pub certificate: PathBuf,
    pub private_key: PathBuf,
    pub generation: String,
    pub workspace_id: Option<config::namespace::Namespace>,
    pub subject: Option<String>,
    pub certificate_not_after: Option<i64>,
}

/// The router trust anchor, resolved client-side with zero configuration. In a
/// debug build it is the committed dev CA embedded at compile time, materialized
/// once to a stable file under `cache_dir` (since [`pmi::TlsConfig::client`]
/// takes a path) and that path returned. In a release build there is no embedded
/// CA. When the platform conventionally exposes its system bundle through
/// `SSL_CERT_FILE`, return that path explicitly because zenohd does not consult
/// the variable itself; otherwise return `None` and let zenohd use its default
/// system store. This is the same platform trust source used by the control-plane
/// HTTP client, not a Peppy-specific trust bypass. The caller supplies the cache
/// dir (its resolved [`PeppyDirs::cache_dir`]) so debug materialization never
/// reaches for the process-global default root.
pub fn resolve_router_ca(cache_dir: &Path) -> Option<PathBuf> {
    resolve_router_ca_from(EMBEDDED_DEV_CA, cache_dir)
        .or_else(|| platform_ca_bundle(std::env::var_os("SSL_CERT_FILE")))
}

fn platform_ca_bundle(value: Option<std::ffi::OsString>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
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

/// The client identity (cert + key) the CLI presents for mTLS, resolved
/// client-side with zero configuration, mirroring [`resolve_router_ca`]
/// (including the caller-supplied `cache_dir`). In a debug build it is the
/// committed dev client leaf embedded at compile time, materialized to the cache
/// dir (since [`pmi::TlsConfig`] takes paths). A release build takes a separate
/// implementation below that requires a valid enrolled production identity;
/// missing material never constructs a one-way TLS target.
#[cfg(debug_assertions)]
pub fn resolve_router_client_identity(
    peppy_dirs: &PeppyDirs,
    _api_url: &str,
    _core_node_name: &str,
) -> Result<RouterClientIdentity> {
    resolve_router_client_identity_from(
        EMBEDDED_DEV_CLIENT_CERT,
        EMBEDDED_DEV_CLIENT_KEY,
        &peppy_dirs.cache_dir(),
    )
    .ok_or_else(|| {
        Error::Auth(
            "could not materialize the debug mTLS client identity; staying standalone".into(),
        )
    })
}

#[cfg(not(debug_assertions))]
pub fn resolve_router_client_identity(
    peppy_dirs: &PeppyDirs,
    api_url: &str,
    core_node_name: &str,
) -> Result<RouterClientIdentity> {
    let creds = storage::load(&storage::credentials_path(peppy_dirs))?;
    let subject = creds
        .session
        .as_ref()
        .map(|session| session.subject.as_str());
    let (metadata, paths) =
        crate::identity::load_active_identity(peppy_dirs, api_url, subject, core_node_name)?;
    Ok(RouterClientIdentity {
        certificate: paths.certificate,
        private_key: paths.private_key,
        generation: paths.generation,
        workspace_id: paths.workspace_id,
        subject: Some(metadata.subject),
        certificate_not_after: Some(metadata.not_after),
    })
}

/// Testable core of [`resolve_router_client_identity`]: materialize the embedded
/// client cert + key (if both present) under `cache_dir` and return their paths;
/// missing either yields `None`. The debug caller converts that into a fail-closed
/// standalone error rather than constructing a certificate-less target.
pub fn resolve_router_client_identity_from(
    cert: Option<&[u8]>,
    key: Option<&[u8]>,
    cache_dir: &Path,
) -> Option<RouterClientIdentity> {
    let cert_path = materialize_embedded(cache_dir, "peppy-dev-client.pem", cert?)?;
    let key_path = materialize_embedded(cache_dir, "peppy-dev-client-key.pem", key?)?;
    Some(RouterClientIdentity {
        certificate: cert_path,
        private_key: key_path,
        generation: "debug-shared-v1".into(),
        workspace_id: None,
        subject: None,
        certificate_not_after: None,
    })
}

/// Materialize `bytes` to `<cache_dir>/<filename>` and return that path. Cheap in
/// the steady state: only (re)writes when the file is missing or its contents differ
/// (e.g. after a fixture change). A filesystem failure degrades to `None` with a
/// warning rather than failing the resolve. Shared by the dev CA and client identity.
fn materialize_embedded(cache_dir: &Path, filename: &str, bytes: &[u8]) -> Option<PathBuf> {
    let path = cache_dir.join(filename);
    if std::fs::read(&path).ok().as_deref() != Some(bytes) {
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            tracing::warn!(
                error = %e, dir = %parent.display(),
                "router TLS: could not create the cache dir to materialize embedded dev \
                 material ({filename}); falling back to the system trust store / no client cert"
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
/// [`resolve_router_ca`]) and `client_identity` (the mTLS cert + key, see
/// [`resolve_router_client_identity`]) are applied fresh to the live TLS at connect
/// time (never cached on disk). `core_node_name` is the daemon's core-node name,
/// carried in the pull's POST body to register the daemon in the backend's
/// core-node registry (see [`client::establish_federation`]); it also
/// tags the cache, so a resolve under a different name (a renamed daemon) always
/// re-pulls and re-registers rather than reusing a still-fresh cache.
///
/// Only the pull path needs a credential, so a fresh cache is reused without
/// touching the token at all.
pub fn resolve_router_endpoint(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
    client_identity: RouterClientIdentity,
    core_node_name: &str,
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
    // (e.g. a PAT-pulled workspace leaking onto the on-disk session once the PAT
    // is gone).
    // An active PAT always re-pulls: a PAT is bound to its own backend subject at
    // pull time (see `pull_and_cache`), which the session-derived `active_subject`
    // cannot match on this fast path, and re-pulling is cheap (federation resolves
    // only at startup and on a login/logout poke).
    //
    // The cache is additionally bound to the core-node name it was pulled under
    // (`rs.core_node_name`): the pull's POST is what registers the daemon in the
    // backend's core-node registry, so a renamed daemon (the `CoreNodeNameTaken`
    // collision-fix workflow: set `core_node_name`, restart) must re-pull, and
    // thereby register its new name, instead of reusing a still-fresh cache and
    // staying absent from the registry until the cache goes stale.
    if let Some(identity_subject) = client_identity.subject.as_deref()
        && !active_subject.is_empty()
        && identity_subject != active_subject
    {
        return Err(Error::Auth(
            "core-node certificate and OAuth session belong to different platform accounts".into(),
        ));
    }
    let reuse_cache = pat.is_none() && !active_subject.is_empty();
    let (endpoint, namespace) = match creds.router {
        Some(rs)
            if reuse_cache
                && !rs.is_stale(now, REPULL_SKEW_SECS)
                && rs.subject == active_subject
                && rs.core_node_name == core_node_name
                && rs.certificate_generation == client_identity.generation
                && client_identity
                    .workspace_id
                    .as_ref()
                    .is_none_or(|workspace| workspace == &rs.namespace) =>
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
            &client_identity,
        )?,
    };

    Ok(RouterEndpoint {
        endpoint: parse_router_endpoint(&endpoint)?,
        tls: client_tls(ca_certificate, &client_identity),
        namespace,
        certificate_not_after: client_identity.certificate_not_after,
    })
}

/// Pulls a fresh router config (refreshing the token on a 401), caches it beside
/// the session, and returns the endpoint locator. `now` is threaded in so the
/// cached `repull_after` uses the same clock reading as the freshness check. The
/// trust anchor is resolved fresh at connect time (see [`resolve_router_ca`]), so
/// it is deliberately not part of the cached `RouterSession`. The pull identifies
/// the daemon by `core_node_name` (the POST body), upserting it into the
/// backend's core-node registry.
fn pull_and_cache(
    creds_path: &Path,
    http: &HttpClient,
    api_url: &str,
    pat: Option<String>,
    now: i64,
    core_node_name: &str,
    client_identity: &RouterClientIdentity,
) -> Result<(String, config::namespace::Namespace)> {
    let mut cred = resolver::resolve(creds_path, http, pat)?;
    // The identity this pull is actually authenticated as drives the cache tag
    // below. A PAT is not the on-disk session, so it must not be tagged with the
    // session subject; doing so would let the session reuse the PAT's workspace once the
    // PAT is gone (a cross-identity leak).
    let is_pat = matches!(cred.kind, resolver::CredentialKind::Pat);
    // Resolve PAT ownership before discovery so a certificate enrolled by a
    // different principal is rejected client-side as well as by the backend.
    let pat_subject = if is_pat {
        let subject = client::get_me(http, api_url, &mut cred)?.sub;
        if let Some(identity_subject) = client_identity.subject.as_deref()
            && identity_subject != subject
        {
            return Err(Error::Auth(
                "PEPPY_API_KEY and the stored core-node certificate belong to different platform accounts; run `peppy platform login` with the intended key"
                    .into(),
            ));
        }
        Some(subject)
    } else {
        None
    };
    let cfg = match client::establish_federation(http, api_url, &mut cred, core_node_name) {
        Err(Error::DiscoveryWorkspaceMismatch { current }) => {
            let Some(certificate) = client_identity.workspace_id.as_ref() else {
                return Err(Error::Auth(
                    "the platform reported workspace drift for a client identity without a workspace binding"
                        .into(),
                ));
            };
            if certificate == &current {
                return Err(Error::Auth(format!(
                    "the platform reported a workspace mismatch for `{core_node_name}`, but its current workspace matches the local certificate; refusing an ambiguous re-enrollment"
                )));
            }
            return Err(Error::WorkspaceMismatch {
                discovered: current,
                certificate: certificate.clone(),
            });
        }
        result => result?,
    };

    if let Some(workspace) = client_identity.workspace_id.as_ref()
        && workspace != &cfg.namespace
    {
        return Err(Error::WorkspaceMismatch {
            discovered: cfg.namespace,
            certificate: workspace.clone(),
        });
    }

    // Validate the config *before* it is written to `creds.router`: a malformed
    // endpoint or an unsupported transport must not poison the on-disk
    // `RouterSession`. A poisoned cache would otherwise re-fail the endpoint
    // parse on every reuse until it goes stale (instead of being re-pulled).
    // The connect target is always `tls/<host>:<port>`, so reject any other
    // advertised transport here rather than caching it and dialing TLS anyway.
    if cfg.protocol != SUPPORTED_ROUTER_PROTOCOL {
        return Err(Error::Auth(format!(
            "router config advertised unsupported transport {:?}; only \
             `{SUPPORTED_ROUTER_PROTOCOL}` is supported",
            cfg.protocol
        )));
    }
    // Parse the locator now (the same check the connect path does) so an
    // unparseable endpoint is rejected before it is persisted.
    parse_router_endpoint(&cfg.endpoint)?;
    let exact_session = if is_pat {
        None
    } else {
        resolver::ensure_session_credential_current(&cred)?
    };

    // Publish only the router field under the shared credentials transaction
    // lock. In particular, an OAuth pull finishing after logout must observe
    // the missing session and must never save its stale pre-network snapshot.
    storage::update(creds_path, |creds| {
        // Tag the cache with the backend identity the config was pulled for so
        // it cannot outlive an identity change. A PAT uses `/me`; an OAuth pull
        // must still have the same-origin live session when it commits.
        let subject = if is_pat {
            pat_subject
                .clone()
                .expect("PAT subject was resolved before federation discovery")
        } else {
            let session = creds.session.as_ref().ok_or(Error::NotAuthenticated)?;
            let expected = exact_session.as_ref().ok_or(Error::NotAuthenticated)?;
            if crate::profile::normalize_api_origin(&session.api_url)?
                != crate::profile::normalize_api_origin(api_url)?
            {
                return Err(Error::Auth(
                    "platform session changed origin while router discovery was in flight".into(),
                ));
            }
            if let Some(identity_subject) = client_identity.subject.as_deref()
                && identity_subject != session.subject
            {
                return Err(Error::Auth(
                    "platform session changed identity while router discovery was in flight".into(),
                ));
            }
            if session.api_url != expected.api_url
                || session.issuer != expected.issuer
                || session.client_id != expected.client_id
                || session.subject != expected.subject
                || session.access_token.expose_secret() != expected.access_token.expose_secret()
                || session.refresh_token.expose_secret() != expected.refresh_token.expose_secret()
            {
                return Err(Error::NotAuthenticated);
            }
            session.subject.clone()
        };
        creds.router = Some(RouterSession {
            endpoint: cfg.endpoint.clone(),
            protocol: cfg.protocol.clone(),
            repull_after: now.saturating_add(saturating_secs_to_i64(cfg.reconnect_after_secs)),
            namespace: cfg.namespace.clone(),
            subject,
            // Tag the cache with the name this pull registered, so a rename
            // forces a re-pull (and re-registration) even while cache-fresh.
            core_node_name: core_node_name.to_string(),
            certificate_generation: client_identity.generation.clone(),
        });
        Ok(())
    })?;
    Ok((cfg.endpoint, cfg.namespace))
}

/// What one federation resolve produced: the desired platform upstream (when
/// the credentials grant one) and the namespace those credentials resolve to.
/// Both come out of the same resolve, so the daemon's federation gate and its
/// namespace-change detection can never disagree within one poll.
pub struct ResolvedFederation {
    /// The upstream to federate the local router to: the parsed `tls` dial
    /// endpoint plus the connect-side mTLS material. `None` means the local
    /// router stays (or turns) standalone: logged out, no backend reachable,
    /// or a `local` namespace (fail closed).
    pub upstream: Option<(ParsedEndpointBuf, pmi::TlsConfig)>,
    /// The namespace the credentials currently resolve to; `local`, the
    /// standalone default, when nothing resolved.
    pub namespace: config::namespace::Namespace,
    /// Newly activated production identity, retained until the daemon has
    /// reloaded Zenoh and verified the real mTLS link. Always `None` in the
    /// explicit debug shared-certificate path.
    pub rotation: Option<crate::identity::IdentityRotation>,
    /// Delay until the earlier of router-config re-pull and certificate
    /// renewal/expiry maintenance. `None` means the logged-out loop may idle.
    pub maintenance_after: Option<Duration>,
    /// Remaining hard validity window of the active client certificate. The
    /// daemon retains this independent deadline so a later resolver timeout or
    /// transport error cannot preserve an upstream past certificate expiry.
    pub certificate_expires_after: Option<Duration>,
    /// A renewal/rebinding failure that did not invalidate the still-current
    /// generation. The daemon uses this to back off retries while continuing
    /// the old valid link.
    pub renewal_error: Option<String>,
    /// A transient discovery/config-pull failure. Unlike intentional
    /// standalone states (logout, missing/expired identity, local namespace),
    /// the daemon must preserve an already-applied still-valid link and retry.
    pub resolve_error: Option<String>,
    /// Whether this daemon resolve observed a non-empty environment PAT. The
    /// token itself never leaves the resolver; only this boolean reaches the
    /// control/status surface so logout can refuse honestly.
    pub pat_active: bool,
    /// Typed signal used only by the production wrapper to force immediate
    /// same-owner certificate re-enrollment on workspace drift.
    workspace_mismatch: Option<config::namespace::Namespace>,
}

impl ResolvedFederation {
    /// A standalone (no-upstream) resolve under `namespace`.
    fn standalone(namespace: config::namespace::Namespace) -> Self {
        Self {
            upstream: None,
            namespace,
            rotation: None,
            maintenance_after: None,
            certificate_expires_after: None,
            renewal_error: None,
            resolve_error: None,
            pat_active: false,
            workspace_mismatch: None,
        }
    }
}

/// Best-effort federation target for the daemon's *local* router: the upstream
/// `tls` connect endpoint plus the connect-side mTLS material, resolved by
/// pulling the platform router's connection config, together with the
/// namespace that resolve produced.
///
/// The upstream is `None`, and the local router stays standalone
/// (plaintext-only), when the user is not logged in, no backend is
/// configured/reachable, or the pull fails, so the daemon always starts. The
/// daemon dials the returned endpoint over mTLS, presenting the embedded dev
/// client cert in debug builds or its enrolled identity in production. The
/// daemon schedules config freshness and certificate renewal separately from
/// Zenoh's ordinary link reconnection.
///
/// `connect_timeout` bounds the (blocking) config pull so a slow/unreachable
/// backend can't stall the caller (federation at startup / on a login-poke)
/// beyond it; on timeout the pull errors and the upstream is `None`.
///
/// `core_node_name` is the daemon's core-node name, sent in every pull's POST
/// body so the backend registry records which daemon federated (and when it
/// last pulled).
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
) -> ResolvedFederation {
    let cache_dir = peppy_dirs.cache_dir();
    let pat = resolver::pat_from_env();
    let pat_active = pat.is_some();

    // A login publishes this marker before changing OAuth/PAT mode and removes
    // it only after the exact-name identity is ready for daemon apply. This is
    // an intentional standalone state, not a transient resolve error: never
    // maintain or reuse a same-subject prior identity while the binding is
    // incomplete. Unsafe/unreadable marker state also fails closed.
    match crate::identity::binding_incomplete(peppy_dirs) {
        Ok(false) => {}
        Ok(true) => {
            tracing::warn!(
                "router federation: platform login binding is incomplete; local router stays standalone"
            );
            let mut resolved =
                ResolvedFederation::standalone(config::namespace::Namespace::local());
            resolved.pat_active = pat_active;
            return resolved;
        }
        Err(error) => {
            tracing::warn!(
                error = %error,
                "router federation: binding-transition marker is unsafe or unreadable; local router stays standalone"
            );
            let mut resolved =
                ResolvedFederation::standalone(config::namespace::Namespace::local());
            resolved.pat_active = pat_active;
            return resolved;
        }
    }

    #[cfg(not(debug_assertions))]
    let (mut rotation, mut renewal_error) = {
        let http = HttpClient::with_timeout(connect_timeout);
        match crate::identity::maintain_identity(
            peppy_dirs,
            &http,
            api_url,
            pat.clone(),
            core_node_name,
        ) {
            Ok(rotation) => (rotation, None),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "router federation: core-node certificate maintenance failed; retaining a still-valid prior generation when available"
                );
                (None, Some(error.to_string()))
            }
        }
    };
    #[cfg(debug_assertions)]
    let (mut rotation, mut renewal_error): (
        Option<crate::identity::IdentityRotation>,
        Option<String>,
    ) = (None, None);

    let mut resolved = resolve_federation_target_at(
        &storage::credentials_path(peppy_dirs),
        api_url,
        pat.clone(),
        resolve_router_ca(&cache_dir),
        match resolve_router_client_identity(peppy_dirs, api_url, core_node_name) {
            Ok(identity) => Some(identity),
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "router federation: no usable exact-name mTLS identity; local router stays standalone"
                );
                None
            }
        },
        connect_timeout,
        core_node_name,
    );

    #[cfg(not(debug_assertions))]
    if let Some(expected_workspace) = resolved.workspace_mismatch.take() {
        // A resolve that raced with another old-workspace rotation must not
        // preserve that unverified generation merely because a receipt exists.
        // Restore its prior pointer, then enroll a fresh key for the workspace
        // the denied discovery response identified.
        if let Some(stale_rotation) = rotation.take()
            && let Err(error) = stale_rotation.rollback()
        {
            renewal_error = Some(format!(
                "could not roll back the stale-workspace certificate before re-enrollment: {error}"
            ));
            resolved.resolve_error = renewal_error.clone();
            resolved.maintenance_after =
                next_maintenance_after(&storage::credentials_path(peppy_dirs), true, true);
            resolved.pat_active = pat_active;
            return resolved;
        }
        let http = HttpClient::with_timeout(connect_timeout);
        match crate::identity::rotate_identity_for_binding_change(
            peppy_dirs,
            &http,
            api_url,
            pat.clone(),
            core_node_name,
        ) {
            Ok(new_rotation) => {
                if let Some(candidate) = new_rotation.as_ref()
                    && candidate.activated().workspace_id != expected_workspace
                {
                    let actual = candidate.activated().workspace_id.clone();
                    let rollback_error = new_rotation
                        .and_then(|candidate| candidate.rollback().err())
                        .map(|error| format!("; rollback also failed: {error}"))
                        .unwrap_or_default();
                    renewal_error = Some(format!(
                        "certificate re-enrollment returned workspace {actual}, but denied discovery identified current workspace {expected_workspace}{rollback_error}"
                    ));
                    resolved = ResolvedFederation::standalone(expected_workspace);
                    resolved.resolve_error = renewal_error.clone();
                    resolved.maintenance_after =
                        next_maintenance_after(&storage::credentials_path(peppy_dirs), true, true);
                    resolved.pat_active = pat_active;
                    return resolved;
                }
                renewal_error = None;
                rotation = new_rotation;
                resolved = resolve_federation_target_at(
                    &storage::credentials_path(peppy_dirs),
                    api_url,
                    pat.clone(),
                    resolve_router_ca(&cache_dir),
                    match resolve_router_client_identity(peppy_dirs, api_url, core_node_name) {
                        Ok(identity) => Some(identity),
                        Err(error) => {
                            tracing::warn!(
                                error = %error,
                                "router federation: re-enrolled identity could not be loaded"
                            );
                            None
                        }
                    },
                    connect_timeout,
                    core_node_name,
                );
            }
            Err(error) => {
                tracing::warn!(
                    error = %error,
                    "router federation: immediate certificate rotation for workspace drift failed; staying standalone"
                );
                renewal_error = Some(error.to_string());
            }
        }
    }

    // An activated generation is not accepted until it produces a real
    // upstream that the daemon can apply and probe. Restore the prior valid
    // generation immediately when discovery itself fails closed.
    if resolved.upstream.is_none()
        && let Some(rejected) = rotation.take()
    {
        let message = "new core-node certificate activated, but federation discovery did not produce a usable upstream";
        let error = match rejected.rollback() {
            Ok(()) => message.into(),
            Err(rollback) => format!("{message}; rollback also failed: {rollback}"),
        };
        renewal_error = Some(error.clone());
        resolved.resolve_error = Some(error);
    }
    resolved.rotation = rotation;
    resolved.renewal_error = renewal_error;
    resolved.maintenance_after = next_maintenance_after(
        &storage::credentials_path(peppy_dirs),
        resolved.renewal_error.is_some(),
        resolved.resolve_error.is_some(),
    );
    resolved.pat_active = pat_active;
    resolved
}

/// Testable core of [`resolve_federation_target`] with the creds path, PAT, CA,
/// client identity, and timeout made explicit (so it can be exercised against a
/// stub backend without touching the process-global credentials file or
/// `PEPPY_API_KEY`). Mirrors the [`super::profile::resolve_api_url`] /
/// `resolve_api_url_from` split.
pub fn resolve_federation_target_at(
    creds_path: &Path,
    api_url: &str,
    pat: Option<String>,
    ca_certificate: Option<PathBuf>,
    client_identity: Option<RouterClientIdentity>,
    connect_timeout: Duration,
    core_node_name: &str,
) -> ResolvedFederation {
    let pat_active = pat.is_some();
    // Skip the network entirely when there is plainly no identity to pull for, so
    // an un-provisioned dev box does not log a spurious auth error every poll. Take
    // that fast path only on a *definitive* no-identity (no PAT and a clean load
    // that returns no session). A real credential *read* failure is not "no
    // identity": log it and fall through rather than swallowing it into a silent
    // logged-out state, so the resolve path below surfaces the underlying error.
    if pat.is_none() {
        match storage::load(creds_path) {
            Ok(creds) if creds.session.is_none() => {
                // Logged out: the namespace comes from the same snapshot
                // (normally absent, since logout clears the router cache).
                return ResolvedFederation::standalone(
                    creds
                        .router
                        .map(|router| router.namespace)
                        .unwrap_or_else(config::namespace::Namespace::local),
                );
            }
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
    let Some(client_identity) = client_identity else {
        tracing::warn!(
            "router federation: client certificate identity is missing; local router stays standalone"
        );
        return ResolvedFederation::standalone(session_namespace(creds_path));
    };
    let http = HttpClient::with_timeout(connect_timeout);
    match resolve_router_endpoint(
        creds_path,
        &http,
        api_url,
        pat,
        ca_certificate,
        client_identity,
        core_node_name,
    ) {
        // Fail-closed gate, single source: federate only under a non-local
        // namespace. An invalid workspace id already failed the HTTP parse, and
        // the daemon's session namespace is resolved from the same cached
        // `namespace`, so a config that cannot federate also cannot carry a
        // federating namespace: an unprefixed/`local` session can never reach
        // the shared multi-tenant router.
        Ok(ep) if !ep.namespace.is_local() => ResolvedFederation {
            upstream: Some((ep.endpoint, ep.tls)),
            namespace: ep.namespace,
            rotation: None,
            maintenance_after: None,
            certificate_expires_after: ep
                .certificate_not_after
                .map(|not_after| conservative_certificate_validity(not_after, storage::now_unix())),
            renewal_error: None,
            resolve_error: None,
            pat_active,
            workspace_mismatch: None,
        },
        Ok(ep) => {
            tracing::warn!(
                "router federation: resolved namespace is the local namespace; \
                 local router stays standalone (fail closed)"
            );
            ResolvedFederation::standalone(ep.namespace)
        }
        Err(Error::WorkspaceMismatch {
            discovered,
            certificate,
        }) => {
            tracing::warn!(
                %discovered,
                %certificate,
                "router federation: workspace binding changed; forcing immediate core-node certificate rotation"
            );
            let mut resolved = ResolvedFederation::standalone(discovered.clone());
            resolved.workspace_mismatch = Some(discovered);
            resolved
        }
        Err(e) => {
            tracing::warn!(
                error = %e,
                "router federation: config pull failed; an already-applied valid link is preserved while the daemon retries"
            );
            let mut resolved = ResolvedFederation::standalone(session_namespace(creds_path));
            resolved.resolve_error = Some(e.to_string());
            resolved
        }
    }
}

/// Computes a stable per-generation renewal jitter and combines it with the
/// router cache deadline. A failed due renewal uses a bounded retry delay while
/// certificate expiry remains an independent hard wakeup, ensuring the daemon
/// removes its upstream as soon as no valid generation remains.
fn next_maintenance_after(
    creds_path: &Path,
    renewal_failed: bool,
    resolution_failed: bool,
) -> Option<Duration> {
    let creds = storage::load(creds_path).ok()?;
    let now = storage::now_unix();
    let mut deadlines = Vec::new();
    if let Some(router) = creds.router {
        let repull_at = router.repull_after.saturating_sub(REPULL_SKEW_SECS);
        if !resolution_failed || repull_at > now {
            deadlines.push(repull_at);
        }
    }
    if let Some(identity) = creds.core_node_identity {
        let renew_at = identity.renewal_at();
        if !renewal_failed || renew_at > now {
            deadlines.push(renew_at);
        }
        // Expiry is a hard fail-closed deadline even when renewal cannot
        // authenticate or the issuer remains unavailable.
        if identity.not_after > now {
            deadlines.push(identity.not_after);
        }
    }
    deadlines
        .into_iter()
        .min()
        .map(|deadline| Duration::from_secs(deadline.saturating_sub(now).max(1) as u64))
}

/// Certificate metadata is stored with whole-second precision while the
/// current instant can already be partway through `now`. Subtract one second
/// from the integer delta so the monotonic daemon deadline can be early by at
/// most one second, but never late by almost one second.
fn conservative_certificate_validity(not_after: i64, now: i64) -> Duration {
    Duration::from_secs(not_after.saturating_sub(now).saturating_sub(1).max(0) as u64)
}

/// The namespace the daemon opens its session under: the cached router
/// config's namespace, or `local` (the standalone default) when there is no
/// usable cache (logged out, not yet pulled, or unreadable).
pub fn session_namespace(creds_path: &Path) -> config::namespace::Namespace {
    storage::load(creds_path)
        .ok()
        .and_then(|creds| creds.router.map(|router| router.namespace))
        .unwrap_or_else(config::namespace::Namespace::local)
}

/// Client TLS material for dialing the shared router: validate it against the
/// resolved trust anchor (`ca_certificate`, or the system store if `None`), with
/// name verification on: the dialed host must match the router's certificate SAN.
/// The complete identity is required by this constructor, presented as the mTLS
/// client certificate, and mutual TLS is always enabled. Identity absence is
/// handled before this point by returning no upstream.
fn client_tls(
    ca_certificate: Option<PathBuf>,
    client_identity: &RouterClientIdentity,
) -> pmi::TlsConfig {
    // `TlsConfig::client` sets `root_ca_certificate` and leaves
    // `verify_name_on_connect` on (its default); `default` trusts the system store.
    let mut tls = match ca_certificate {
        Some(ca) => pmi::TlsConfig::client(ca),
        None => pmi::TlsConfig::default(),
    };
    tls.connect_certificate = Some(client_identity.certificate.clone());
    tls.connect_private_key = Some(client_identity.private_key.clone());
    tls.enable_mtls = true;
    tls
}

/// Clamps a `u64` seconds value into the `i64` unix-time domain so an absurd
/// `reconnect_after_secs` can't wrap `repull_after` negative.
fn saturating_secs_to_i64(secs: u64) -> i64 {
    i64::try_from(secs).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_identity() -> RouterClientIdentity {
        RouterClientIdentity {
            certificate: PathBuf::from("/etc/peppy/client.pem"),
            private_key: PathBuf::from("/etc/peppy/client-key.pem"),
            generation: "test-generation".into(),
            workspace_id: None,
            subject: None,
            certificate_not_after: None,
        }
    }

    #[test]
    fn incomplete_login_binding_is_intentional_standalone_without_maintenance() {
        let tmp = tempfile::tempdir().expect("temp dir");
        let dirs = PeppyDirs::new(tmp.path());
        crate::identity::arm_binding_incomplete(&dirs).expect("arm marker");

        let resolved = resolve_federation_target(
            &dirs,
            "http://127.0.0.1:1",
            Duration::from_secs(1),
            "core-node-binding-transition",
        );

        assert!(resolved.upstream.is_none());
        assert!(resolved.namespace.is_local());
        assert!(resolved.rotation.is_none());
        assert!(resolved.renewal_error.is_none());
        assert!(
            resolved.resolve_error.is_none(),
            "the marker is an intentional standalone decision, not a transient error that preserves an old link"
        );
        assert!(resolved.maintenance_after.is_none());
    }

    #[test]
    fn whole_second_certificate_projection_is_never_late() {
        assert_eq!(
            conservative_certificate_validity(101, 100),
            Duration::ZERO,
            "less than one conservatively knowable second must be treated as expired"
        );
        assert_eq!(
            conservative_certificate_validity(102, 100),
            Duration::from_secs(1)
        );
        assert_eq!(conservative_certificate_validity(100, 100), Duration::ZERO);
    }

    #[test]
    fn client_tls_with_ca_and_client_identity_enables_mtls() {
        let tls = client_tls(Some(PathBuf::from("/etc/peppy/ca.pem")), &test_identity());
        assert_eq!(
            tls.root_ca_certificate.as_deref(),
            Some(std::path::Path::new("/etc/peppy/ca.pem"))
        );
        assert!(tls.verify_name_on_connect, "name verification must stay on");
        assert!(tls.enable_mtls, "a client identity must enable mTLS");
        assert_eq!(
            tls.connect_certificate.as_deref(),
            Some(std::path::Path::new("/etc/peppy/client.pem"))
        );
        assert_eq!(
            tls.connect_private_key.as_deref(),
            Some(std::path::Path::new("/etc/peppy/client-key.pem"))
        );
    }

    #[test]
    fn client_tls_without_ca_falls_back_to_system_store() {
        let tls = client_tls(None, &test_identity());
        assert!(tls.root_ca_certificate.is_none());
        assert!(tls.verify_name_on_connect);
        assert!(
            tls.enable_mtls,
            "system roots do not weaken client authentication"
        );
        assert!(tls.connect_certificate.is_some());
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

    #[test]
    fn conventional_platform_ca_bundle_is_forwarded() {
        assert_eq!(
            platform_ca_bundle(Some("/etc/ssl/custom-ca.pem".into())).as_deref(),
            Some(Path::new("/etc/ssl/custom-ca.pem"))
        );
        assert!(platform_ca_bundle(Some("".into())).is_none());
        assert!(platform_ca_bundle(None).is_none());
    }

    /// Breaking-change guard: the legacy router-CA env override and the
    /// env-reading CA helper are gone for good; the router CA has no Peppy-
    /// specific trust override (the conventional platform bundle is separate).
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

    #[test]
    fn router_endpoint_parses_with_the_strict_dial_grammar() {
        let parsed = parse_router_endpoint("tls/7f3a.zenoh.localhost:7443").expect("valid");
        assert_eq!(
            (parsed.host(), parsed.port()),
            ("7f3a.zenoh.localhost", 7443)
        );
        // IPv6 hosts come back unbracketed, ready for a TLS dial/probe.
        let ipv6 = parse_router_endpoint("tls/[2001:db8::1]:7443").expect("valid ipv6");
        assert_eq!((ipv6.host(), ipv6.port()), ("2001:db8::1", 7443));
    }

    #[test]
    fn router_endpoint_grammar_rejects_what_the_daemon_cannot_dial() {
        for endpoint in [
            // One grammar end to end: a schemeless locator is no longer
            // tolerated at the pull boundary either.
            "cap.zenoh.localhost:7443",
            // Wrong transport: the CLI only dials `tls/`.
            "tcp/cap.zenoh.localhost:7443",
            "tls/cap.zenoh.localhost",
            "tls/:7443",
            "tls/cap.zenoh.localhost:https",
            // A listen wildcard is not dialable.
            "tls/0.0.0.0:7443",
            // Config/metadata suffixes never come from the backend.
            "tls/cap.zenoh.localhost:7443#enable_mtls=true",
        ] {
            assert!(
                parse_router_endpoint(endpoint).is_err(),
                "{endpoint:?} must be rejected"
            );
        }
    }
}
