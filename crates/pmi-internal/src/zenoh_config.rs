//! Generation of Zenoh session and router configs.
//!
//! This is the single source of truth that replaced the former set of per-shape
//! Askama templates (one each for the fail-fast client, the reconnecting client,
//! the watchdog probe, and the router). A typed [`ZenohConfigSpec`] keeps the
//! mode / connect / listen / scouting / timestamping settings in one place so
//! adding a knob touches one struct instead of four string templates.
//!
//! ## Discovery model: gossip-only, multicast off
//!
//! Every generated config disables multicast scouting and relies on gossip
//! seeded by the configured connect endpoints (the router). Nodes open a `peer`
//! session that connects to the router, learn each other's locators via gossip,
//! and then form direct peer-to-peer links so data no longer relays through the
//! router. Multicast is left off on purpose: on a shared host it bridges
//! otherwise-independent peer groups (and would cross-link unrelated test runs),
//! and with a known seed it adds nothing gossip does not already cover.

use crate::zenohd::ZenohNetProtocol;
use serde_json::json;

/// The Zenoh roles this codebase generates configs for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SessionMode {
    /// A node/daemon session: connects to a seed (the router), then forms direct
    /// peer-to-peer links with peers discovered via gossip.
    Peer,
    /// A short-lived liveness probe: connects to exactly one router endpoint and
    /// never discovers peers, so "is *our* router up?" stays deterministic.
    Client,
    /// The zenohd router itself.
    Router,
}

/// Inputs for [`build_zenoh_config`].
pub(crate) struct ZenohConfigSpec {
    pub mode: SessionMode,
    /// Seed endpoints to dial (`<proto>/<host>:<port>`). Empty for the router.
    pub connect_endpoints: Vec<String>,
    /// Endpoints to listen on. Empty leaves Zenoh's default (clients do not
    /// listen). Peers listen on a loopback ephemeral port by default.
    pub listen_endpoints: Vec<String>,
    /// Retry the connection forever (`timeout_ms: -1`, `exit_on_failure: false`)
    /// instead of failing fast. Used by long-lived sessions so a router restart
    /// is recovered transparently.
    pub reconnect: bool,
    /// Enable gossip scouting so peers sharing a seed discover each other and
    /// form direct links. Multicast scouting is always off (see module docs).
    pub gossip: bool,
}

/// Builds the JSON5-equivalent config value for a session or the router.
pub(crate) fn build_zenoh_config(spec: &ZenohConfigSpec) -> serde_json::Value {
    let mode = match spec.mode {
        SessionMode::Peer => "peer",
        SessionMode::Client => "client",
        SessionMode::Router => "router",
    };

    let mut config = json!({
        "mode": mode,
        "scouting": {
            "multicast": { "enabled": false },
            "gossip": { "enabled": spec.gossip }
        }
    });

    if !spec.connect_endpoints.is_empty() {
        let mut connect = json!({ "endpoints": spec.connect_endpoints });
        if spec.reconnect {
            connect["timeout_ms"] = json!(-1);
            connect["exit_on_failure"] = json!(false);
            connect["retry"] = json!({
                "period_init_ms": 1000,
                "period_max_ms": 4000,
                "period_increase_factor": 2.0
            });
        }
        config["connect"] = connect;
    }

    if !spec.listen_endpoints.is_empty() {
        // Zenoh reads `listen.endpoints` against the session's OWN mode, so the
        // key must match `mode` above — the same per-role-map rule as
        // `timestamping` below. Deriving it exhaustively (rather than
        // `else => "peer"`) keeps a client's endpoints from being silently
        // misfiled under `peer` if one ever listens.
        let role = match spec.mode {
            SessionMode::Router => "router",
            SessionMode::Peer => "peer",
            SessionMode::Client => "client",
        };
        config["listen"] = json!({ "endpoints": { role: spec.listen_endpoints } });
    }

    // Stamp data at the producer so consumers can measure real delivery latency.
    // Zenoh matches `enabled` against the session's OWN mode, so the key must
    // match `mode` above — a `peer` session ignores an `enabled.client` entry.
    // Stamping at the source keeps peer mode (no router in the direct path) on
    // par with router mode, where the client/router already stamps.
    config["timestamping"] = match spec.mode {
        SessionMode::Router => {
            json!({ "enabled": { "router": true }, "drop_future_timestamp": false })
        }
        SessionMode::Peer => json!({ "enabled": { "peer": true }, "drop_future_timestamp": false }),
        SessionMode::Client => {
            json!({ "enabled": { "client": true }, "drop_future_timestamp": false })
        }
    };

    config
}

/// Serializes a spec to a JSON string (valid JSON5) for the zenohd config file.
pub(crate) fn render_config_string(spec: &ZenohConfigSpec) -> String {
    serde_json::to_string(&build_zenoh_config(spec)).expect("zenoh config value serializes to JSON")
}

/// Renders a spec into a parsed [`zenoh::config::Config`] for opening a session.
pub(crate) fn render_config(spec: &ZenohConfigSpec) -> zenoh::config::Config {
    zenoh::config::Config::from_json5(&render_config_string(spec))
        .expect("generated zenoh config parses")
}

/// Rewrites the unroutable wildcard bind address to a connectable loopback host.
pub(crate) fn connectable_host(host: &str) -> String {
    if host == "0.0.0.0" {
        "127.0.0.1".to_string()
    } else {
        host.to_string()
    }
}

/// The liveness-probe config shared by the router watchdog and the
/// ephemeral-router readiness check: a plain client targeting one router
/// endpoint, no peer discovery.
pub(crate) fn render_probe_config(
    protocol: ZenohNetProtocol,
    host: &str,
    port: u16,
) -> zenoh::config::Config {
    render_config(&ZenohConfigSpec {
        mode: SessionMode::Client,
        connect_endpoints: vec![format!("{protocol}/{}:{port}", connectable_host(host))],
        listen_endpoints: Vec::new(),
        reconnect: false,
        gossip: false,
    })
}

/// The loopback ephemeral listen endpoint a peer binds. Loopback-only by design:
/// it keeps the new inbound socket off the network (co-located peering only).
/// Cross-host peering is a deliberate opt-in that lives behind a custom
/// `ZENOH_SESSION_CONFIG`, not this default.
pub(crate) fn loopback_listen_endpoint(protocol: ZenohNetProtocol) -> String {
    format!("{protocol}/127.0.0.1:0")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_spec(reconnect: bool, gossip: bool) -> ZenohConfigSpec {
        ZenohConfigSpec {
            mode: SessionMode::Peer,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: vec![loopback_listen_endpoint(ZenohNetProtocol::Tcp)],
            reconnect,
            gossip,
        }
    }

    #[test]
    fn peer_config_has_peer_mode_listen_and_gossip_only_scouting() {
        let cfg = build_zenoh_config(&peer_spec(false, true));

        assert_eq!(cfg["mode"], "peer");
        assert_eq!(cfg["connect"]["endpoints"][0], "tcp/127.0.0.1:7448");
        // Peers listen on a loopback ephemeral port under the per-mode key.
        assert_eq!(cfg["listen"]["endpoints"]["peer"][0], "tcp/127.0.0.1:0");
        // Discovery is gossip-only.
        assert_eq!(cfg["scouting"]["multicast"]["enabled"], false);
        assert_eq!(cfg["scouting"]["gossip"]["enabled"], true);
        // A peer session stamps under its own role, not `client` — Zenoh reads
        // `enabled` against the session's mode, so an `enabled.client` entry here
        // would be silently ignored and leave samples unstamped.
        assert_eq!(cfg["timestamping"]["enabled"]["peer"], true);
        assert_eq!(cfg["timestamping"]["drop_future_timestamp"], false);
        assert!(cfg["timestamping"]["enabled"].get("client").is_none());
        // Fail-fast: no infinite-retry connect block.
        assert!(cfg["connect"].get("timeout_ms").is_none());
    }

    #[test]
    fn reconnecting_peer_config_retries_forever() {
        let cfg = build_zenoh_config(&peer_spec(true, true));

        assert_eq!(cfg["mode"], "peer");
        assert_eq!(cfg["connect"]["timeout_ms"], -1);
        assert_eq!(cfg["connect"]["exit_on_failure"], false);
        assert_eq!(cfg["connect"]["retry"]["period_init_ms"], 1000);
        assert_eq!(cfg["connect"]["retry"]["period_max_ms"], 4000);
    }

    #[test]
    fn gossip_can_be_disabled_to_force_router_relay() {
        let cfg = build_zenoh_config(&peer_spec(false, false));
        assert_eq!(cfg["scouting"]["gossip"]["enabled"], false);
    }

    #[test]
    fn probe_config_is_a_multicast_free_client() {
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: Vec::new(),
            reconnect: false,
            gossip: false,
        });

        assert_eq!(cfg["mode"], "client");
        assert_eq!(cfg["scouting"]["multicast"]["enabled"], false);
        // A client stamps under its own role so its outgoing data carries a
        // source timestamp without depending on the router to add one.
        assert_eq!(cfg["timestamping"]["enabled"]["client"], true);
        // A client never listens for inbound peers.
        assert!(cfg.get("listen").is_none());
    }

    #[test]
    fn client_listen_endpoints_land_under_the_client_key() {
        // Clients do not listen today, so this guards the per-role-map rule
        // directly: were a client ever given a listen endpoint, it must land
        // under its own `client` key, not be silently misfiled under `peer`
        // (the same mismatch that left peer-mode samples unstamped).
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Client,
            connect_endpoints: vec!["tcp/127.0.0.1:7448".to_string()],
            listen_endpoints: vec!["tcp/127.0.0.1:0".to_string()],
            reconnect: false,
            gossip: false,
        });

        assert_eq!(cfg["mode"], "client");
        assert_eq!(cfg["listen"]["endpoints"]["client"][0], "tcp/127.0.0.1:0");
        assert!(cfg["listen"]["endpoints"].get("peer").is_none());
    }

    #[test]
    fn router_config_listens_under_router_key_with_router_timestamping() {
        let cfg = build_zenoh_config(&ZenohConfigSpec {
            mode: SessionMode::Router,
            connect_endpoints: Vec::new(),
            listen_endpoints: vec!["tcp/0.0.0.0:7448".to_string()],
            reconnect: false,
            gossip: true,
        });

        assert_eq!(cfg["mode"], "router");
        assert_eq!(cfg["listen"]["endpoints"]["router"][0], "tcp/0.0.0.0:7448");
        assert_eq!(cfg["timestamping"]["enabled"]["router"], true);
        assert_eq!(cfg["timestamping"]["drop_future_timestamp"], false);
        // Routers do not dial out.
        assert!(cfg.get("connect").is_none());
    }

    #[test]
    fn generated_session_configs_parse_as_zenoh_config() {
        // Guards the JSON5 schema: a malformed block would otherwise only
        // surface as a panic at session/router open.
        render_config(&peer_spec(true, true));
        render_config(&peer_spec(false, true));
        render_probe_config(ZenohNetProtocol::Tcp, "0.0.0.0", 7448);
    }
}
