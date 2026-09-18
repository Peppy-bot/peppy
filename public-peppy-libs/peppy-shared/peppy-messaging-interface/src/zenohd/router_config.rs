//! Generation of the zenohd router's own config file. This is the daemon's
//! deployment config (how to run the router), distinct from the client session
//! configs that live with the messaging adapter. Both are produced by the
//! shared [`crate::zenoh_config`] builder.

use super::ZenohNetProtocol;
use crate::error::{Error, Result};
use crate::router_id::RouterId;
use crate::zenoh_config::{TlsConfig, render_config_string, router_spec};
use std::path::{Path, PathBuf};

/// Resolves the zenohd router config path. Honors a `ZENOH_CONFIG` override;
/// otherwise renders a router config to a temp file keyed by messaging port and
/// returns its path. `tls` (when the protocol is `Tls`, or when a `tls/`
/// `connect_endpoint` is present) carries the listener's certificate/key and/or
/// the connect-side trust root; `None` renders a plaintext listener unchanged.
/// `connect_endpoints` federates this router to those upstream routers (empty for
/// a standalone router); see [`crate::zenoh_config::router_spec`].
///
/// `router_id` pins the spawned router's transport identity. It is ignored on
/// the `ZENOH_CONFIG` path, where the operator owns the whole config including
/// whatever `id` it does or does not carry.
pub(crate) fn router_config_path(
    protocol: ZenohNetProtocol,
    host: &str,
    messaging_port: u16,
    connect_endpoints: Vec<String>,
    tls: Option<TlsConfig>,
    router_id: &RouterId,
) -> Result<PathBuf> {
    if let Some(config_path) = config_override() {
        return Ok(config_path);
    }

    let config_path = std::env::temp_dir().join(format!("zenohd_config_{}.json5", messaging_port));
    render_router_config_to_path(
        &config_path,
        protocol,
        host,
        messaging_port,
        connect_endpoints,
        tls,
        router_id,
    )?;
    Ok(config_path)
}

/// Renders the router config and writes it to `config_path`, *bypassing* the
/// `ZENOH_CONFIG` override resolution that [`router_config_path`] does. The
/// refederation path ([`crate::ZenohAdapter::refederate`]) uses this to rewrite
/// the file captured by [`ZenohdFacade::managed`](super::ZenohdFacade::managed) in place:
/// going back through `router_config_path` would re-read the process-global
/// `ZENOH_CONFIG`, which — if it changed after startup — could redirect the write
/// to a different path or skip it entirely (the override early-return), leaving
/// the running router's actual config file stale. (The operator-pinned case is
/// already filtered out by `facade.is_pinned()` before this is reached.)
pub(crate) fn render_router_config_to_path(
    config_path: &Path,
    protocol: ZenohNetProtocol,
    host: &str,
    messaging_port: u16,
    connect_endpoints: Vec<String>,
    tls: Option<TlsConfig>,
    router_id: &RouterId,
) -> Result<()> {
    // The router seeds gossip discovery for the peer mesh, so gossip stays on;
    // multicast is off everywhere (see `crate::zenoh_config`). The router listens
    // on `host` as given (typically `0.0.0.0`) so nodes can reach it. Shares
    // `router_spec` with the out-of-process render path (`render_router_config`).
    let config_content = render_config_string(&router_spec(
        protocol,
        host,
        messaging_port,
        true,
        connect_endpoints,
        tls,
        router_id,
    ));

    std::fs::write(config_path, config_content)
        .map_err(|e| Error::ConfigurationError(format!("Failed to write zenohd config: {}", e)))?;

    Ok(())
}

/// The operator-pinned router config path from `ZENOH_CONFIG`, if set. When
/// present, [`router_config_path`] returns it untouched — we never render our own
/// config over an operator-owned one. Callers that *re-render* to apply a change
/// (e.g. [`crate::ZenohAdapter::refederate`]) consult this to detect that the
/// re-render would be a no-op, so they can skip the work it would otherwise
/// trigger (a pointless zenohd restart). This is the single source of truth for
/// the override so the two call sites cannot drift.
pub(crate) fn config_override() -> Option<PathBuf> {
    // A blank or whitespace-only `ZENOH_CONFIG` is treated as unset (not an empty
    // path), so startup falls back to the rendered temp config instead of trying
    // to read `""`. Mirrors `ZenohAdapter::resolve_session_config_override`.
    std::env::var("ZENOH_CONFIG")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}
