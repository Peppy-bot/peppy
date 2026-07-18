//! Global daemon configuration read from `~/.peppy/conf/peppy_config.json5`.
//!
//! This is the single user-facing switch for the messaging topology. The daemon
//! reads it ONCE at startup (see `peppy service serve`), creating the file from the
//! bundled default template if it is missing, and applies the result to its own
//! core-node session and to every node it spawns. Editing the file takes effect
//! after a daemon restart.
//!
//! A well-formed file that omits settings (typically one written by an older
//! peppy before a new knob existed) is completed in place where the text
//! scanner can safely locate the insertion point. Missing entries are appended
//! with their default values and explanatory comments. Any omission the scanner
//! cannot safely splice is named in a warning and remains defaulted in memory.
//! The user's own values, comments, and unknown root-level keys are otherwise
//! preserved byte-for-byte (see [`completion`]); when appending defaults,
//! completion may add a structural separator comma after the prior final entry.
//! Each setting added this way is logged at info level, so the first start after
//! a peppy upgrade shows exactly which new settings appeared in the file.
//!
//! A non-empty `PEPPY_CONFIG` environment variable bypasses that on-disk flow.
//! Its value is tried first as a config file path and then, if the file cannot
//! be read, as an inline JSON5 document. An override is read-only: peppy never
//! creates, completes, or rewrites either source, and any invalid or incomplete
//! source fails loud.
//!
//! Unlike `repositories.json5`, a malformed `peppy_config.json5` fails loud at
//! startup ([`load_or_create`] returns `Err`) instead of falling back to
//! defaults: the topology and buffer sizes determine the whole mesh's routing
//! model and backpressure, so a hand-edited typo must surface immediately rather
//! than silently reverting to the peer topology. A malformed file is never
//! rewritten.

mod completion;

use crate::atomic_write::publish_atomic;
use crate::consts::PeppyDirs;
use crate::error::{Error, ParsingError, Result};
use config::consts::{ALLOWED_CONFIG_CHARS, PEPPY_CONFIG_ENV};
use config::peppy_config::{
    DEFAULT_DAEMON_GRACE_SECS, DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE, DEFAULT_SHUTDOWN_GRACE_SECS,
    DEFAULT_STANDARD_BUFFER_SIZE, SubscriberBufferConfig,
};
use config::runtime::Name;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::{Path, PathBuf};

/// File name of the global daemon config under `~/.peppy/conf`.
pub const PEPPY_CONFIG_FILE: &str = "peppy_config.json5";

/// The backend resource-server URL for this build: the local dev backend in
/// debug builds, the prod backend in release builds. The single source of truth
/// for both the seeded `resource_servers` block and the built-in fallback the
/// `peppy auth login` / `whoami` / `logout` commands resolve when no `--api-url` /
/// `PEPPY_API_URL` override is given.
#[cfg(debug_assertions)]
pub const DEFAULT_API_URL: &str = "http://127.0.0.1:3000";
#[cfg(not(debug_assertions))]
pub const DEFAULT_API_URL: &str = "https://api.peppy.bot";

/// Minimum accepted grace period, in seconds. Must comfortably exceed the
/// heartbeat interval and the router-watchdog restart window so a brief daemon
/// blip never trips a node's watchdog.
pub const MIN_DAEMON_GRACE_SECS: u64 = 30;
/// Cadence, in seconds, of the daemon-liveness heartbeat each spawned node's
/// watchdog listens for (published by the daemon; see
/// `core_node::services::clock::publish_daemon_heartbeat`). Defined next to
/// `MIN_DAEMON_GRACE_SECS` so the invariant between them is enforced where
/// both values live.
pub const DAEMON_HEARTBEAT_INTERVAL_SECS: u64 = 5;
// Compile-time guard on the watchdog's false-trip margin: even several missed
// beats must fit inside the smallest accepted grace period.
const _: () = assert!(MIN_DAEMON_GRACE_SECS >= 3 * DAEMON_HEARTBEAT_INTERVAL_SECS);

/// Minimum accepted cooperative-shutdown grace period, in seconds. At least 1 so
/// the cooperative shutdown signal is actually given a chance to land before the
/// force-kill (a 0 would cancel the in-flight send and amount to an immediate
/// SIGKILL).
pub const MIN_SHUTDOWN_GRACE_SECS: u64 = 1;

/// Default bound, in seconds, on resolving the caller's per-user cloud router
/// when the daemon federates its local router to it: at startup (where it gates
/// `serve` reporting ready) and again whenever `auth login`/`logout` pokes the
/// daemon. A slow or unreachable backend must not stall federation past this, so
/// the daemon falls back to standalone and retries in the background. 30 mirrors
/// the historical hardcoded HTTP-client timeout, so behavior is unchanged until
/// edited.
pub const DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS: u64 = 30;
/// Minimum accepted federation connect timeout, in seconds. At least 1 so a
/// hand-edited 0 cannot collapse the bound to "give up immediately" (a 0 would
/// also mean "no timeout" to the HTTP client, the opposite of the intent).
pub const MIN_FEDERATION_CONNECT_TIMEOUT_SECS: u64 = 1;

/// Maximum accepted `core_node_name` length, in characters. The name is
/// embedded in every zenoh key expression that addresses this daemon
/// (`service/node/{name}/core/…`), so the familiar DNS-label cap keeps those
/// keys bounded without constraining any realistic name (derived defaults stay
/// well under it).
pub const MAX_CORE_NODE_NAME_LEN: usize = 63;

// The bundled default config, written verbatim on first create so its comments
// survive. Kept inline (not `include_str!` from an asset file) so the template
// lives next to the completion logic that splices it and the crate needs no
// sibling asset directory.
//
// The template is split into one snippet per entry so `completion` can splice a
// missing section or field (comments included) into a user's existing file. The
// numeric values are spliced in from the `DEFAULT_*` constants at compile time
// via `concatcp!`, so neither the template nor a spliced snippet can drift from
// the serde `Default` impls the parser falls back to when an entry is absent.

/// Comment block at the top of the bundled config file.
const TEMPLATE_HEADER: &str = r#"// Daemon settings are read once at startup, so edits to them take effect only
// after restart. resource_servers is refreshed by each CLI authentication command.
"#;

/// The `core_node_name` entry with its explanatory comment. Spelled out as an
/// explicit `null` (parsed as `None` → derive the default) rather than
/// omitting the key: completion treats a `null` entry as present, so splicing
/// this snippet into an older file is idempotent.
const CORE_NODE_NAME_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"  // Fixed name for this daemon's core node, or null to derive a
  // machine-specific default (cn-...). Names must be unique across all
  // daemons reachable over the same router/federation: a daemon whose name is
  // already in use refuses to boot. At most "#,
    MAX_CORE_NODE_NAME_LEN,
    r#" characters from
  // ""#,
    ALLOWED_CONFIG_CHARS,
    r#"".
  // `peppy service serve --core-node-name` overrides this for one run.
  core_node_name: null,
"#
);

/// The `zenoh.managed.local_nodes_topology` entry with its explanatory comment,
/// indented for the `managed` block.
const LOCAL_NODES_TOPOLOGY_FIELD_SNIPPET: &str = r#"      // How the nodes on this machine exchange data with each other. Traffic to
      // and from other machines always relays through the local zenohd router and
      // its federation, regardless of this setting (node sessions only accept
      // direct links over loopback).
      //   "peer"   - Zenoh peer sessions with gossip: local nodes form direct
      //              peer-to-peer links and data stops relaying through the router.
      //   "router" - gossip off: all traffic relays through the central zenohd
      //              router.
      // Container nodes in a separate network namespace (Lima on macOS) always use
      // the router path regardless of this setting.
      local_nodes_topology: "peer",
"#;

/// The `zenoh.managed.subscriber_buffers.standard_buffer_size` entry, indented
/// for the `subscriber_buffers` block.
const STANDARD_BUFFER_FIELD_SNIPPET: &str = const_format::concatcp!(
    "        standard_buffer_size: ",
    DEFAULT_STANDARD_BUFFER_SIZE,
    ",\n"
);

/// The `zenoh.managed.subscriber_buffers.high_throughput_buffer_size` entry,
/// indented for the `subscriber_buffers` block.
const HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET: &str = const_format::concatcp!(
    "        high_throughput_buffer_size: ",
    DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
    ",\n"
);

/// The whole `zenoh.managed.subscriber_buffers` block with its explanatory
/// comment.
const SUBSCRIBER_BUFFERS_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"      // Subscriber channel buffer sizes (number of in-flight messages) per QoS
      // tier, used by local node sessions in both managed topologies. They matter
      // most in peer mode, where no router relay buffers between a publisher and
      // a subscriber. Defaults match peppy's built-in behavior; only edit to tune
      // backpressure.
      subscriber_buffers: {
"#,
    STANDARD_BUFFER_FIELD_SNIPPET,
    HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET,
    "      },\n"
);

/// The `lifecycle.daemon_grace_secs` entry with its comment, indented for the
/// `lifecycle` block.
const DAEMON_GRACE_FIELD_SNIPPET: &str = const_format::concatcp!(
    r#"    // Node lifecycle knobs. `daemon_grace_secs` is the grace period a spawned node
    // waits, after the daemon's heartbeat goes silent, before shutting itself down
    // to avoid orphaning.
    daemon_grace_secs: "#,
    DEFAULT_DAEMON_GRACE_SECS,
    ",\n"
);

/// The `lifecycle.shutdown_grace_secs` entry with its comment, indented for the
/// `lifecycle` block.
const SHUTDOWN_GRACE_FIELD_SNIPPET: &str = const_format::concatcp!(
    r#"    // How long a clean shutdown (ctrl+C / `systemctl stop`) and `peppy node
    // stop` wait for a node to exit cooperatively before force-killing its
    // process group. Seconds; minimum 1. A robot node uses this window to park
    // actuators and release hardware before it is killed.
    shutdown_grace_secs: "#,
    DEFAULT_SHUTDOWN_GRACE_SECS,
    ",\n"
);

/// The whole `lifecycle` block.
const LIFECYCLE_SECTION_SNIPPET: &str = const_format::concatcp!(
    "  lifecycle: {\n",
    DAEMON_GRACE_FIELD_SNIPPET,
    "\n",
    SHUTDOWN_GRACE_FIELD_SNIPPET,
    "  },\n"
);

/// The `resource_servers.api` entry, indented for the `resource_servers` block.
const API_FIELD_SNIPPET: &str = const_format::concatcp!("    api: \"", DEFAULT_API_URL, "\",\n");

/// The whole `resource_servers` block with its explanatory comment. Only the
/// CLI auth commands read this URL; the daemon ignores it but seeds and
/// completes the block like every other knob.
const RESOURCE_SERVERS_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"  // Backend resource-server URL the `peppy auth login` / `whoami` / `logout`
  // commands talk to. Baked in at compile time (the dev backend in debug
  // builds, prod in release); --api-url / PEPPY_API_URL override it at runtime.
  resource_servers: {
"#,
    API_FIELD_SNIPPET,
    "  },\n"
);

/// The `zenoh.managed.federation.connect_timeout_secs` entry with its comment,
/// indented for the `federation` block.
const FEDERATION_TIMEOUT_FIELD_SNIPPET: &str = const_format::concatcp!(
    r#"        // Seconds the daemon spends resolving your per-user cloud router before
        // giving up for this attempt (it retries in the background). Bounds the
        // federation done at startup and on each `peppy auth login`/`logout`;
        // minimum 1. If the backend is unreachable within this window the daemon
        // stays standalone rather than blocking.
        connect_timeout_secs: "#,
    DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
    ",\n"
);

/// The optional `zenoh.managed.federation.listen_endpoint` entry.
const FEDERATION_LISTEN_ENDPOINT_FIELD_SNIPPET: &str = r#"        // Optional inbound federation listener. Federation links always require mTLS;
        // generate the fleet identity with `peppy federation ca init` and
        // `peppy federation ca issue`. Example: "tls/0.0.0.0:7449". null
        // disables inbound federation. Restart the daemon to apply a change.
        listen_endpoint: null,
"#;

/// The optional machine-certificate path override.
const FEDERATION_CERT_PATH_FIELD_SNIPPET: &str = r#"        // Machine certificate override, or null for <conf>/federation/cert.pem.
        // Paths become Zenoh endpoint fragments and cannot contain #, ;, or =.
        cert_path: null,
"#;

/// The optional machine-private-key path override.
const FEDERATION_KEY_PATH_FIELD_SNIPPET: &str = r#"        // Machine private-key override, or null for <conf>/federation/key.pem.
        // Paths become Zenoh endpoint fragments and cannot contain #, ;, or =.
        key_path: null,
"#;

/// The optional fleet-CA path override.
const FEDERATION_CA_PATH_FIELD_SNIPPET: &str = r#"        // Fleet CA override, or null for <conf>/federation/ca.pem.
        // Paths become Zenoh endpoint fragments and cannot contain #, ;, or =.
        ca_path: null,
"#;

/// The whole `zenoh.managed.federation` block with its explanatory comment.
const FEDERATION_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"      // Managed zenoh-router federation settings for the platform backend and
      // user peers. User federation is always protected by mutual TLS.
      federation: {
"#,
    FEDERATION_TIMEOUT_FIELD_SNIPPET,
    "\n",
    FEDERATION_LISTEN_ENDPOINT_FIELD_SNIPPET,
    "\n",
    FEDERATION_CERT_PATH_FIELD_SNIPPET,
    "\n",
    FEDERATION_KEY_PATH_FIELD_SNIPPET,
    "\n",
    FEDERATION_CA_PATH_FIELD_SNIPPET,
    "      },\n"
);

/// The whole `zenoh.managed` block. Every child setting is meaningful only
/// while peppy owns the router process.
const MANAGED_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"    // Settings peppy can only honor because it owns the router process. Replace
    // this whole block with `external` to use your own router.
    managed: {
"#,
    LOCAL_NODES_TOPOLOGY_FIELD_SNIPPET,
    "\n",
    SUBSCRIBER_BUFFERS_SECTION_SNIPPET,
    "\n",
    FEDERATION_SECTION_SNIPPET,
    "    },\n"
);

/// The whole `zenoh` block, composed from its child snippets exactly like the
/// template composes the top-level sections, so a splice into a partial `zenoh`
/// block matches what a fresh template spells out.
const ZENOH_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"  // The zenoh messaging transport. Configure exactly one of two blocks:
  //   managed:  peppy starts, monitors, restarts, and stops its bundled
  //             zenohd router, and every knob inside `managed` applies.
  //   external: you run the router and peppy only dials it; peppy never
  //             starts, reconfigures, restarts, stops, or federates it,
  //             local nodes always relay through it, and the managed knobs
  //             do not exist. Spelled, in place of `managed`, as:
  //               external: { endpoint: "tcp/<host>:<port>" },
  zenoh: {
"#,
    MANAGED_SECTION_SNIPPET,
    "  },\n"
);

/// The full bundled default config, composed from the snippets above.
const DEFAULT_PEPPY_CONFIG_TEMPLATE: &str = const_format::concatcp!(
    TEMPLATE_HEADER,
    "{\n",
    CORE_NODE_NAME_SECTION_SNIPPET,
    "\n",
    ZENOH_SECTION_SNIPPET,
    "\n",
    LIFECYCLE_SECTION_SNIPPET,
    "\n",
    RESOURCE_SERVERS_SECTION_SNIPPET,
    "}\n"
);

/// The messaging topology of the nodes local to this machine.
///
/// `Peer` keeps gossip on so co-located nodes form direct peer-to-peer links;
/// `Router` turns gossip off so every node routes through the central
/// `zenohd`. Local-only by construction: node sessions accept direct links
/// over a loopback-bound listener, so cross-machine traffic always relays
/// through the routers regardless of this choice. The `gossip()` mapping is
/// the single source of truth tying this user-facing choice to the
/// `DiscoveryConfig.gossip` flag the sessions actually read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalNodesTopology {
    #[default]
    Peer,
    Router,
}

impl LocalNodesTopology {
    /// Whether this is the peer topology (direct peer-to-peer links).
    pub fn is_peer(self) -> bool {
        matches!(self, LocalNodesTopology::Peer)
    }

    /// LocalNodesTopology to gossip mapping: peer enables gossip, router disables it.
    pub fn gossip(self) -> bool {
        self.is_peer()
    }
}

/// Node lifecycle knobs. `daemon_grace_secs` is the grace period a spawned node
/// waits, after the daemon's heartbeat goes silent, before shutting itself down
/// to avoid orphaning. A clean ctrl+C / `systemctl stop` is immediate and does
/// not consult this value; it only governs an uncatchable daemon death.
///
/// `#[serde(default)]` fills any field a partial `lifecycle` block omits from
/// [`LifecycleConfig::default`], matching the `SubscriberBufferConfig` pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct LifecycleConfig {
    pub daemon_grace_secs: u64,
    /// Cooperative-shutdown grace period, in seconds: how long a clean daemon
    /// shutdown and `peppy node stop` wait for a node to exit on its own before
    /// force-killing its process group. Unlike `daemon_grace_secs` (the
    /// uncatchable-death watchdog), this governs the catchable/explicit stop
    /// paths.
    pub shutdown_grace_secs: u64,
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            daemon_grace_secs: DEFAULT_DAEMON_GRACE_SECS,
            shutdown_grace_secs: DEFAULT_SHUTDOWN_GRACE_SECS,
        }
    }
}

/// The backend resource server the CLI auth commands talk to. The endpoint
/// paths (`/cli/auth-config`, `/me`, `/logout`) are appended by the caller; `api`
/// holds only the base URL. A single URL, baked in per build: there is no
/// dev/prod selection at runtime, so the file stores exactly the build's
/// backend ([`DEFAULT_API_URL`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResourceServers {
    pub api: String,
}

impl Default for ResourceServers {
    fn default() -> Self {
        Self {
            api: DEFAULT_API_URL.to_string(),
        }
    }
}

/// Per-user zenoh-router federation knobs. `connect_timeout_secs` bounds the
/// backend round-trip the daemon makes to resolve the caller's cloud router so a
/// slow or unreachable backend never stalls the federation step past it (read at
/// startup, where it gates `serve` reporting ready, and on every login/logout
/// poke).
///
/// `#[serde(default)]` fills any field a partial `federation` block omits from
/// [`FederationConfig::default`], matching the `LifecycleConfig` pattern.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FederationConfig {
    pub connect_timeout_secs: u64,
    /// Optional inbound fleet-mTLS listener. `None` leaves the managed router
    /// without a user-federation listener.
    pub listen_endpoint: Option<String>,
    /// Machine certificate override, or the conventional federation identity
    /// path when absent.
    pub cert_path: Option<PathBuf>,
    /// Machine private-key override, or the conventional federation identity
    /// path when absent.
    pub key_path: Option<PathBuf>,
    /// Fleet CA override, or the conventional federation identity path when
    /// absent.
    pub ca_path: Option<PathBuf>,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
            listen_endpoint: None,
            cert_path: None,
            key_path: None,
            ca_path: None,
        }
    }
}

/// Whether an endpoint is opened for outbound dialing or inbound listening.
/// Listener parsing accepts wildcard hosts; dial parsing rejects them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointPurpose {
    Dial,
    Listen,
}

/// Parsed, syntax-checked `<scheme>/<host>:<port>` endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ParsedEndpoint<'a> {
    /// Host without IPv6 brackets.
    pub host: &'a str,
    pub port: u16,
}

/// Parses the deliberately narrow locator surface Peppy manages. The caller
/// supplies the required transport scheme and whether wildcard hosts are
/// meaningful. Hostnames are checked syntactically but never resolved.
pub fn parse_endpoint<'a>(
    endpoint: &'a str,
    expected_scheme: &str,
    purpose: EndpointPurpose,
) -> std::result::Result<ParsedEndpoint<'a>, String> {
    if endpoint.is_empty() {
        return Err("must not be empty".to_string());
    }
    if endpoint.trim() != endpoint {
        return Err("must not contain leading or trailing whitespace".to_string());
    }

    let expected_prefix = format!("{expected_scheme}/");
    let Some(address) = endpoint.strip_prefix(&expected_prefix) else {
        return Err(format!(
            "must use the {expected_scheme}/<host>:<port> locator form"
        ));
    };
    if address.contains(['?', '#', ';', '=']) {
        return Err("metadata and endpoint configuration are not supported".to_string());
    }

    let (host, port, bracketed) = split_endpoint_host_port(address)?;
    validate_endpoint_host(host, bracketed, purpose)?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("port must be an integer from 1 through 65535".to_string());
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| "port must be an integer from 1 through 65535".to_string())?;
    if port == 0 {
        return Err("port must be an integer from 1 through 65535".to_string());
    }
    Ok(ParsedEndpoint { host, port })
}

/// Owned form of [`ParsedEndpoint`] that keeps the canonical endpoint text
/// together with its parsed host and port, so a syntax-checked endpoint can be
/// stored and passed around without consumers re-parsing (and re-wording
/// impossible parse failures for) the same string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ParsedEndpointBuf {
    text: String,
    /// Host without IPv6 brackets.
    host: String,
    port: u16,
}

impl ParsedEndpointBuf {
    /// Parses and takes ownership of an endpoint via [`parse_endpoint`].
    pub fn parse(
        text: impl Into<String>,
        expected_scheme: &str,
        purpose: EndpointPurpose,
    ) -> std::result::Result<Self, String> {
        let text = text.into();
        let parsed = parse_endpoint(&text, expected_scheme, purpose)?;
        let host = parsed.host.to_string();
        let port = parsed.port;
        Ok(Self { text, host, port })
    }

    /// The canonical `<scheme>/<host>:<port>` text this was parsed from.
    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub fn host(&self) -> &str {
        &self.host
    }

    pub fn port(&self) -> u16 {
        self.port
    }
}

impl std::fmt::Display for ParsedEndpointBuf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.text)
    }
}

impl Serialize for ParsedEndpointBuf {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.text)
    }
}

fn split_endpoint_host_port(address: &str) -> std::result::Result<(&str, &str, bool), String> {
    if let Some(bracketed) = address.strip_prefix('[') {
        let Some(close) = bracketed.find(']') else {
            return Err("IPv6 addresses must be enclosed in matching brackets".to_string());
        };
        let host = &bracketed[..close];
        let suffix = &bracketed[close + 1..];
        let Some(port) = suffix.strip_prefix(':') else {
            return Err("must include a port after the host".to_string());
        };
        if port.contains(':') {
            return Err("must contain exactly one port".to_string());
        }
        return Ok((host, port, true));
    }

    let Some((host, port)) = address.rsplit_once(':') else {
        return Err("must include a host and port".to_string());
    };
    if host.contains(':') {
        return Err("IPv6 addresses must be enclosed in brackets".to_string());
    }
    Ok((host, port, false))
}

fn validate_endpoint_host(
    host: &str,
    bracketed: bool,
    purpose: EndpointPurpose,
) -> std::result::Result<(), String> {
    if host.is_empty() {
        return Err("host must not be empty".to_string());
    }

    if bracketed {
        let address = host
            .parse::<Ipv6Addr>()
            .map_err(|_| "bracketed host must be a valid IPv6 address".to_string())?;
        return if address.is_unspecified() && purpose == EndpointPurpose::Dial {
            Err(
                "host must be dialable; the wildcard address [::] is only valid for listening"
                    .to_string(),
            )
        } else {
            Ok(())
        };
    }

    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return if address.is_unspecified() && purpose == EndpointPurpose::Dial {
            Err("host must be dialable; 0.0.0.0 is only valid for listening".to_string())
        } else {
            Ok(())
        };
    }
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return Err("host is not a valid IPv4 address".to_string());
    }
    if host == "*" {
        return if purpose == EndpointPurpose::Dial {
            Err("host must be dialable; * is only valid for listening".to_string())
        } else {
            Ok(())
        };
    }
    let hostname = host.strip_suffix('.').unwrap_or(host);
    if host.len() > 253
        || hostname.ends_with('.')
        || hostname.split('.').any(|label| {
            label.is_empty()
                || label.len() > 63
                || label.starts_with('-')
                || label.ends_with('-')
                || !label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        })
    {
        return Err("host must be a valid hostname or IP address".to_string());
    }
    Ok(())
}

/// Settings peppy can honor only while it owns the bundled router process.
/// `#[serde(default)]` fills fields omitted from a partial `managed` block.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ManagedZenohConfig {
    pub local_nodes_topology: LocalNodesTopology,
    #[serde(deserialize_with = "deserialize_subscriber_buffers")]
    pub subscriber_buffers: SubscriberBufferConfig,
    pub federation: FederationConfig,
}

/// The operator-run router peppy dials without managing its process,
/// configuration, topology, or federation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalZenohConfig {
    pub endpoint: String,
}

/// The `zenoh` section holds exactly one transport mode. Managed-only knobs
/// exist only because peppy owns the bundled router. External mode always
/// relays local nodes through the operator's router and leaves its federation
/// to the operator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ZenohConfig {
    Managed(ManagedZenohConfig),
    External(ExternalZenohConfig),
}

impl Default for ZenohConfig {
    fn default() -> Self {
        Self::Managed(ManagedZenohConfig::default())
    }
}

impl<'de> Deserialize<'de> for ZenohConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Preserve a present `null` as a value so block presence, not payload
        // validity, selects the variant and the payload receives the proper
        // variant-specific error.
        fn present_value<'de, D>(
            deserializer: D,
        ) -> std::result::Result<Option<serde_json::Value>, D::Error>
        where
            D: Deserializer<'de>,
        {
            serde_json::Value::deserialize(deserializer).map(Some)
        }

        #[derive(Deserialize)]
        struct Wire {
            #[serde(default, deserialize_with = "present_value")]
            managed: Option<serde_json::Value>,
            #[serde(default, deserialize_with = "present_value")]
            external: Option<serde_json::Value>,
            #[serde(flatten)]
            fields: BTreeMap<String, serde_json::Value>,
        }

        let wire = Wire::deserialize(deserializer)?;
        if let Some(field) = wire.fields.keys().next() {
            return Err(serde::de::Error::custom(format!(
                "unknown field `zenoh.{field}`; zenoh holds exactly one of `managed` or `external`"
            )));
        }

        match (wire.managed, wire.external) {
            (Some(_), Some(_)) => Err(serde::de::Error::custom(
                "zenoh.managed and zenoh.external are mutually exclusive; configure exactly one",
            )),
            (Some(managed), None) => serde_json::from_value(managed)
                .map(Self::Managed)
                .map_err(|error| serde::de::Error::custom(format!("zenoh.managed: {error}"))),
            (None, Some(external)) => parse_external_zenoh(external)
                .map(Self::External)
                .map_err(serde::de::Error::custom),
            (None, None) => Ok(Self::default()),
        }
    }
}

fn parse_external_zenoh(
    value: serde_json::Value,
) -> std::result::Result<ExternalZenohConfig, String> {
    let serde_json::Value::Object(mut fields) = value else {
        return Err(
            "zenoh.external must be a block like { endpoint: \"tcp/<host>:<port>\" }".to_string(),
        );
    };
    let endpoint = fields.remove("endpoint");
    if let Some(field) = fields.keys().next() {
        return Err(format!("unknown field `zenoh.external.{field}`"));
    }
    match endpoint {
        Some(serde_json::Value::String(endpoint)) => Ok(ExternalZenohConfig { endpoint }),
        Some(_) => Err("zenoh.external.endpoint must be a string".to_string()),
        None => Err("zenoh.external.endpoint is required".to_string()),
    }
}

impl ZenohConfig {
    /// Whether local node sessions may discover each other directly.
    pub fn gossip(&self) -> bool {
        match self {
            Self::Managed(config) => config.local_nodes_topology.gossip(),
            Self::External(_) => false,
        }
    }

    /// Subscriber buffers for local sessions under either managed topology.
    /// External mode always uses the built-in defaults because this tuning is
    /// managed-only.
    pub fn subscriber_buffers(&self) -> SubscriberBufferConfig {
        match self {
            Self::Managed(config) => config.subscriber_buffers,
            Self::External(_) => SubscriberBufferConfig::default(),
        }
    }

    /// The full Zenoh locator peppy should dial in external mode.
    pub fn external_endpoint(&self) -> Option<&str> {
        match self {
            Self::Managed(_) => None,
            Self::External(config) => Some(&config.endpoint),
        }
    }

    /// Federation settings when peppy owns the router. External federation is
    /// wholly operator-managed, so no federation task is armed.
    pub fn federation(&self) -> Option<&FederationConfig> {
        match self {
            Self::Managed(config) => Some(&config.federation),
            Self::External(_) => None,
        }
    }

    fn validate(&self) -> Result<()> {
        match self {
            Self::Managed(config) => {
                let buffer_sizes = [
                    (
                        "standard_buffer_size",
                        config.subscriber_buffers.standard_buffer_size,
                    ),
                    (
                        "high_throughput_buffer_size",
                        config.subscriber_buffers.high_throughput_buffer_size,
                    ),
                ];
                for (field, value) in buffer_sizes {
                    if value == 0 {
                        return Err(cannot_parse_config(format!(
                            "invalid zenoh.managed.subscriber_buffers.{field}: must be > 0"
                        )));
                    }
                }
                if config.federation.connect_timeout_secs < MIN_FEDERATION_CONNECT_TIMEOUT_SECS {
                    return Err(cannot_parse_config(format!(
                        "invalid zenoh.managed.federation.connect_timeout_secs: must be >= {MIN_FEDERATION_CONNECT_TIMEOUT_SECS}"
                    )));
                }
                if let Some(endpoint) = &config.federation.listen_endpoint {
                    parse_endpoint(endpoint, "tls", EndpointPurpose::Listen).map_err(|error| {
                        cannot_parse_config(format!(
                            "invalid zenoh.managed.federation.listen_endpoint: {error}"
                        ))
                    })?;
                }
                for (field, path) in [
                    ("cert_path", config.federation.cert_path.as_deref()),
                    ("key_path", config.federation.key_path.as_deref()),
                    ("ca_path", config.federation.ca_path.as_deref()),
                ] {
                    if let Some(path) = path {
                        validate_locator_path(path).map_err(|error| {
                            cannot_parse_config(format!(
                                "invalid zenoh.managed.federation.{field}: {error}"
                            ))
                        })?;
                    }
                }
                Ok(())
            }
            Self::External(config) => {
                parse_endpoint(&config.endpoint, "tcp", EndpointPurpose::Dial).map_err(
                    |error| {
                        cannot_parse_config(format!("invalid zenoh.external.endpoint: {error}"))
                    },
                )?;
                Ok(())
            }
        }
    }
}

/// Rejects paths that cannot be embedded in a Zenoh locator fragment. The
/// single source of the reserved-delimiter rule, shared with the federation
/// identity paths.
pub fn validate_locator_path(path: &Path) -> std::result::Result<(), String> {
    let Some(path) = path.to_str() else {
        return Err("must be valid UTF-8 for use in a Zenoh endpoint fragment".to_string());
    };
    if let Some(delimiter) = path
        .chars()
        .find(|character| ['#', ';', '='].contains(character))
    {
        return Err(format!(
            "must not contain the reserved locator delimiter {delimiter:?}; use a path without \
             `#`, `;`, or `=`"
        ));
    }
    Ok(())
}

/// Deserializes the shared [`SubscriberBufferConfig`] through a strict local wire
/// type so typos in this user-edited file cannot silently fall back to defaults.
fn deserialize_subscriber_buffers<'de, D>(
    deserializer: D,
) -> std::result::Result<SubscriberBufferConfig, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(default, deny_unknown_fields)]
    struct Wire {
        standard_buffer_size: usize,
        high_throughput_buffer_size: usize,
    }

    impl Default for Wire {
        fn default() -> Self {
            let defaults = SubscriberBufferConfig::default();
            Self {
                standard_buffer_size: defaults.standard_buffer_size,
                high_throughput_buffer_size: defaults.high_throughput_buffer_size,
            }
        }
    }

    let wire = Wire::deserialize(deserializer)?;
    Ok(SubscriberBufferConfig {
        standard_buffer_size: wire.standard_buffer_size,
        high_throughput_buffer_size: wire.high_throughput_buffer_size,
    })
}

/// The whole `peppy_config.json5` document. Every field is serde-defaulted so a
/// partial or older file still parses; extra unknown keys are tolerated (this is
/// a user-edited file, forward-compat beats strictness here).
///
/// Every DEFAULTED field must also serialize under `Default` (no
/// `skip_serializing_if`): the schema-coverage pin in [`completion`] enumerates
/// those settings by serializing this struct's default value, and a field it
/// cannot see would escape the guarantee that older files gain every new
/// default on upgrade. Required fields that exist only in a non-default tagged
/// variant (currently `zenoh.external.endpoint`) are pinned separately and
/// deliberately are not invented by completion.
///
/// Not `Copy`: `resource_servers` owns heap strings. The daemon reads this once
/// and moves it into the core node, and the CLI clones it field-by-field, so the
/// lost `Copy` costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PeppyConfig {
    /// Fixed core-node name for this daemon; `None` (the template's explicit
    /// `null`) derives the machine-specific default. Checked by [`validate`]
    /// (`Name` charset + [`MAX_CORE_NODE_NAME_LEN`]) so an invalid name fails
    /// at load time instead of panicking when the core node boots.
    ///
    /// [`validate`]: PeppyConfig::validate
    #[serde(default)]
    pub core_node_name: Option<String>,
    #[serde(default)]
    pub zenoh: ZenohConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    #[serde(default)]
    pub resource_servers: ResourceServers,
}

impl PeppyConfig {
    /// Rejects user-tunable fields that serde cannot constrain.
    ///
    /// Buffer sizes feed bounded channel constructors downstream: a 0 capacity
    /// panics `tokio::sync::mpsc::channel` and degrades `flume::bounded` into a
    /// rendezvous channel that stalls every send. A hand-edited 0 must fail loud
    /// at load time rather than crash or wedge a running mesh.
    fn validate(&self) -> Result<()> {
        // An explicit core-node name ends up in `Name::new(...)` when the core
        // node boots, so a value serde accepted as a plain string must be
        // checked here to fail at load time instead of at boot.
        if let Some(name) = &self.core_node_name
            && (Name::new(name.as_str()).is_err() || name.len() > MAX_CORE_NODE_NAME_LEN)
        {
            return Err(Error::Parsing(ParsingError::CannotParseConfig(format!(
                "{PEPPY_CONFIG_FILE}: invalid core_node_name {name:?}: must be \
                 non-empty, at most {MAX_CORE_NODE_NAME_LEN} characters, and use \
                 only characters from \"{ALLOWED_CONFIG_CHARS}\""
            ))));
        }

        self.zenoh.validate()?;

        // The grace period must comfortably exceed the heartbeat interval and a
        // router restart, or a brief daemon blip would trip every node's
        // watchdog. Reject a hand-edited too-small value loud at load time.
        if self.lifecycle.daemon_grace_secs < MIN_DAEMON_GRACE_SECS {
            return Err(Error::Parsing(ParsingError::CannotParseConfig(format!(
                "invalid lifecycle.daemon_grace_secs: must be >= {MIN_DAEMON_GRACE_SECS}"
            ))));
        }
        if self.lifecycle.shutdown_grace_secs < MIN_SHUTDOWN_GRACE_SECS {
            return Err(Error::Parsing(ParsingError::CannotParseConfig(format!(
                "invalid lifecycle.shutdown_grace_secs: must be >= {MIN_SHUTDOWN_GRACE_SECS}"
            ))));
        }
        Ok(())
    }
}

/// Reads the global config from `~/.peppy/conf/peppy_config.json5`, creating it
/// from the bundled default template (verbatim, so comments survive) when it
/// does not exist, and appending defaults for settings an existing file omits.
///
/// A non-empty [`PEPPY_CONFIG_ENV`] value bypasses the normal file completely.
/// The override is loaded read-only and is never created, completed, rewritten,
/// or used as a reason to touch the normal on-disk config. Invalid, incomplete,
/// or unreadable overrides return `Err`.
///
/// Read ONCE by the daemon at startup. A malformed existing file returns `Err`
/// (fail loud) rather than defaulting, since mode and buffer sizes are
/// load-bearing for the whole mesh. This intentionally differs from
/// `ensure_default_repos`, which only warns on a bad repos file.
pub fn load_or_create(peppy_dirs: &PeppyDirs) -> Result<PeppyConfig> {
    load_or_create_with_override(peppy_dirs, std::env::var_os(PEPPY_CONFIG_ENV))
}

fn load_or_create_with_override(
    peppy_dirs: &PeppyDirs,
    env_value: Option<OsString>,
) -> Result<PeppyConfig> {
    if let Some(value) = env_override_source(env_value)? {
        return load_override_config(&value);
    }

    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let path = conf_dir.join(PEPPY_CONFIG_FILE);

    if !path.exists() {
        // Plain write: there is no user-authored content to protect yet, and
        // it leaves the new file with normal umask-derived permissions.
        std::fs::write(&path, DEFAULT_PEPPY_CONFIG_TEMPLATE)?;
        tracing::info!("created {PEPPY_CONFIG_FILE} with the bundled defaults");
        // The bundled template is a compile-time invariant; a parse failure here
        // means the shipped asset is broken, not the user's file.
        let config: PeppyConfig =
            serde_json5::from_str(DEFAULT_PEPPY_CONFIG_TEMPLATE).map_err(|e| {
                Error::Serialize(format!("bundled default peppy_config is invalid: {e}"))
            })?;
        config.validate()?;
        return Ok(config);
    }

    let content = std::fs::read_to_string(&path)?;
    let config: PeppyConfig = serde_json5::from_str(&content).map_err(|e| {
        Error::Parsing(ParsingError::CannotParseConfig(format!(
            "{PEPPY_CONFIG_FILE}: {e}"
        )))
    })?;
    // serde parses any numeric field, so a hand-edited 0 buffer size survives
    // the parse above; reject it before it reaches a bounded channel downstream.
    config.validate()?;
    // Only a fully successful load may touch the user's file: a malformed or
    // invalid config errors out above with the file left byte-for-byte intact.
    complete_file_with_defaults(&path, &content, &config);
    Ok(config)
}

fn env_override_source(value: Option<OsString>) -> Result<Option<String>> {
    let Some(value) = value else {
        return Ok(None);
    };
    if value.is_empty() {
        return Ok(None);
    }
    value.into_string().map(Some).map_err(|_| {
        cannot_parse_config(format!(
            "{PEPPY_CONFIG_ENV} is set but its value is not valid UTF-8; use a UTF-8 file path or inline JSON5 document"
        ))
    })
}

fn load_override_config(value: &str) -> Result<PeppyConfig> {
    let file_attempt = match expand_tilde(value) {
        Ok(path) => fs::read_to_string(&path)
            .map(|content| (path.clone(), content))
            .map_err(|error| format!("{}: {error}", path.display())),
        Err(error) => Err(format!("{value}: {error}")),
    };

    let (content, origin, config) = match file_attempt {
        Ok((path, content)) => {
            let origin = format!("file {}", path.display());
            tracing::info!(
                "using {PEPPY_CONFIG_ENV} ({origin}); bypassing on-disk {PEPPY_CONFIG_FILE}"
            );
            let config = serde_json5::from_str::<PeppyConfig>(&content).map_err(|error| {
                cannot_parse_config(format!("{PEPPY_CONFIG_ENV} ({origin}): {error}"))
            })?;
            (content, origin, config)
        }
        Err(file_error) => match serde_json5::from_str::<PeppyConfig>(value) {
            Ok(config) => {
                let origin = "inline document".to_string();
                tracing::info!(
                    "using {PEPPY_CONFIG_ENV} ({origin}); bypassing on-disk {PEPPY_CONFIG_FILE}"
                );
                (value.to_string(), origin, config)
            }
            Err(inline_error) => {
                return Err(cannot_parse_config(format!(
                    "{PEPPY_CONFIG_ENV} is neither a readable config file ({file_error}) nor an inline JSON5 config ({inline_error})"
                )));
            }
        },
    };

    config
        .validate()
        .map_err(|error| prefix_override_error(error, &origin))?;
    ensure_override_spells_out_every_setting(&content, &origin)?;
    Ok(config)
}

fn cannot_parse_config(message: String) -> Error {
    Error::Parsing(ParsingError::CannotParseConfig(message))
}

fn prefix_override_error(error: Error, origin: &str) -> Error {
    match error {
        Error::Parsing(ParsingError::CannotParseConfig(message)) => {
            cannot_parse_config(format!("{PEPPY_CONFIG_ENV} ({origin}): {message}"))
        }
        other => other,
    }
}

/// Enforces the read-only override contract using the same variant-aware
/// completion table as on-disk completion. Managed overrides must spell every
/// `zenoh.managed` leaf; external overrides need only their required external
/// block and the settings outside `zenoh`.
fn ensure_override_spells_out_every_setting(content: &str, origin: &str) -> Result<()> {
    let Some(completion) = completion::complete_config_content(content) else {
        return Ok(());
    };
    let missing_paths = completion::missing_paths_in_template_order(&completion);
    Err(cannot_parse_config(format!(
        "{PEPPY_CONFIG_ENV} ({origin}): missing settings this peppy release defines: {}; {PEPPY_CONFIG_ENV} sources are never completed with defaults, so every setting must be spelled out",
        missing_paths.join(", ")
    )))
}

fn expand_tilde(path: &str) -> std::result::Result<PathBuf, String> {
    if path == "~" {
        return dirs::home_dir()
            .ok_or_else(|| "cannot resolve ~ because the home directory is unavailable".into());
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir()
            .map(|home| home.join(rest))
            .ok_or_else(|| "cannot resolve ~/ because the home directory is unavailable".into());
    }
    if path.starts_with('~') {
        return Err("~user paths are not supported; use an absolute path or ~/...".into());
    }
    Ok(PathBuf::from(path))
}

/// Appends template defaults for every omitted setting whose insertion point
/// can be safely located, warns about any omission it cannot splice, and logs
/// which settings were added so a release upgrade leaves a visible trace.
///
/// Best effort by design: `config` (parsed from `content`) is already complete
/// in memory via the serde defaults, so this only improves the FILE. The result
/// must pass [`completion::verify_completion`] before anything is written; a
/// failure means a splicing bug, in which case the user's file is left
/// untouched and a warning is logged instead of taking the daemon down over a
/// cosmetic rewrite.
fn complete_file_with_defaults(path: &Path, content: &str, config: &PeppyConfig) {
    let Some(completion) = completion::complete_config_content(content) else {
        return;
    };
    if !completion.unspliceable_paths.is_empty() {
        tracing::warn!(
            "{PEPPY_CONFIG_FILE}: could not add missing settings to the file: {}; \
             continuing with the in-memory defaults",
            completion.unspliceable_paths.join(", ")
        );
    }
    if completion.added_paths.is_empty() {
        return;
    }
    if !completion::verify_completion(content, &completion.content, config) {
        tracing::warn!(
            "adding missing defaults to {PEPPY_CONFIG_FILE} produced inconsistent \
             content, leaving the file untouched"
        );
        return;
    }
    // Write through a symlink, not over it: a dotfiles-managed config stays a
    // symlink and its real target receives the completed content. (The atomic
    // rename below replaces the path entry itself, so it must point at the
    // resolved file.)
    let target = match std::fs::canonicalize(path) {
        Ok(target) => target,
        Err(e) => {
            tracing::warn!("could not resolve {PEPPY_CONFIG_FILE} for completion: {e}");
            return;
        }
    };
    if let Err(e) = write_config_file(&target, &completion.content) {
        tracing::warn!(
            "could not add missing defaults to {PEPPY_CONFIG_FILE}, \
             continuing with the in-memory defaults: {e}"
        );
        return;
    }
    tracing::info!(
        "{PEPPY_CONFIG_FILE}: added settings new to this peppy release: {}",
        completion.added_paths.join(", ")
    );
}

/// Replaces an existing config through a staged sibling tmp file and an atomic
/// rename, so a crash mid-write can never truncate a user's hand-edited
/// `peppy_config.json5`. The destination's permissions are carried onto the
/// staged file first: `NamedTempFile` creates it as 0600 on unix, and the
/// rename would otherwise silently tighten the user's file.
fn write_config_file(path: &Path, content: &str) -> Result<()> {
    let permissions = std::fs::metadata(path)?.permissions();
    publish_atomic(path, |tmp| {
        std::fs::write(tmp, content)?;
        std::fs::set_permissions(tmp, permissions)
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Writes `content` as the config file in a fresh `~/.peppy`-style tempdir.
    /// The tempdir guard is returned so callers keep it alive for the test.
    fn dirs_with_config(content: &str) -> (tempfile::TempDir, PeppyDirs, PathBuf) {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        let path = conf_dir.join(PEPPY_CONFIG_FILE);
        std::fs::write(&path, content).unwrap();
        (tmp, peppy_dirs, path)
    }

    fn error_message(error: Error) -> String {
        match error {
            Error::Parsing(ParsingError::CannotParseConfig(message)) => message,
            other => panic!("expected CannotParseConfig, got {other:?}"),
        }
    }

    fn managed(config: &PeppyConfig) -> &ManagedZenohConfig {
        match &config.zenoh {
            ZenohConfig::Managed(managed) => managed,
            ZenohConfig::External(_) => panic!("expected managed zenoh config"),
        }
    }

    #[test]
    fn env_override_source_unset_and_empty_are_none() {
        assert_eq!(env_override_source(None).unwrap(), None);
        assert_eq!(env_override_source(Some(OsString::new())).unwrap(), None);
    }

    #[cfg(unix)]
    #[test]
    fn env_override_source_non_utf8_fails_loud() {
        use std::os::unix::ffi::OsStringExt;

        let error = env_override_source(Some(OsString::from_vec(vec![0xff]))).unwrap_err();
        let message = error_message(error);
        assert!(message.contains(PEPPY_CONFIG_ENV));
        assert!(message.contains("not valid UTF-8"));
    }

    #[test]
    fn inline_override_accepts_the_full_template_with_comments() {
        let tmp = tempdir().unwrap();
        let root = tmp.path().join("never-created");
        let peppy_dirs = PeppyDirs::new(&root);

        let config =
            load_or_create_with_override(&peppy_dirs, Some(DEFAULT_PEPPY_CONFIG_TEMPLATE.into()))
                .unwrap();

        assert_eq!(config, PeppyConfig::default());
        assert!(!root.exists());
    }

    #[test]
    fn override_managed_document_must_spell_every_managed_leaf() {
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE.replacen(STANDARD_BUFFER_FIELD_SNIPPET, "", 1);

        let message = error_message(load_override_config(&content).unwrap_err());

        assert!(message.contains("zenoh.managed.subscriber_buffers.standard_buffer_size"));
        assert!(message.contains("never completed with defaults"));
    }

    #[test]
    fn override_external_document_is_complete_without_managed_knobs() {
        let expected = PeppyConfig {
            zenoh: ZenohConfig::External(ExternalZenohConfig {
                endpoint: "tcp/router.internal:7448".to_string(),
            }),
            ..PeppyConfig::default()
        };
        let content = serde_json5::to_string(&expected).unwrap();

        assert_eq!(load_override_config(&content).unwrap(), expected);
    }

    #[test]
    fn override_with_empty_zenoh_lists_the_managed_block() {
        let mut document = serde_json::to_value(PeppyConfig::default()).unwrap();
        document["zenoh"] = serde_json::json!({});
        let content = serde_json5::to_string(&document).unwrap();

        let message = error_message(load_override_config(&content).unwrap_err());

        assert!(message.contains("zenoh.managed"));
        assert!(message.contains("never completed with defaults"));
    }

    #[test]
    fn file_override_loads_a_complete_config_file() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("override.json5");
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE.replacen(
            "local_nodes_topology: \"peer\"",
            "local_nodes_topology: \"router\"",
            1,
        );
        std::fs::write(&path, &content).unwrap();
        let before = std::fs::read(&path).unwrap();

        let config = load_override_config(path.to_str().unwrap()).unwrap();

        assert_eq!(
            managed(&config).local_nodes_topology,
            LocalNodesTopology::Router
        );
        assert_eq!(std::fs::read(&path).unwrap(), before);
    }

    #[test]
    fn file_form_errors_do_not_fall_back_to_inline() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("malformed.json5");
        std::fs::write(&path, "{ not json5").unwrap();

        let message = error_message(load_override_config(path.to_str().unwrap()).unwrap_err());

        assert!(message.contains(PEPPY_CONFIG_ENV));
        assert!(message.contains(&format!("file {}", path.display())));
        assert!(!message.contains("neither a readable config file"));
    }

    #[test]
    fn neither_file_nor_inline_lists_both_reasons() {
        let value = "definitely/missing.json5";

        let message = error_message(load_override_config(value).unwrap_err());

        assert!(message.contains(PEPPY_CONFIG_ENV));
        assert!(message.contains(value));
        assert!(message.contains("readable config file"));
        assert!(message.contains("inline JSON5 config"));
    }

    #[test]
    fn whitespace_only_value_fails_with_both_reasons() {
        let message = error_message(load_override_config("   ").unwrap_err());

        assert!(message.contains(PEPPY_CONFIG_ENV));
        assert!(message.contains("readable config file"));
        assert!(message.contains("inline JSON5 config"));
    }

    #[test]
    fn file_override_expands_tilde() {
        let relative = format!(
            "definitely-missing-peppy-config-env-{}-{}",
            std::process::id(),
            line!()
        );
        let value = format!("~/{relative}/x.json5");
        let expanded = dirs::home_dir().unwrap().join(relative).join("x.json5");

        let message = error_message(load_override_config(&value).unwrap_err());

        assert!(message.contains(&expanded.display().to_string()));
        assert!(message.contains("inline JSON5 config"));
    }

    #[test]
    fn inline_override_failing_validation_names_the_env_var() {
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE.replacen(
            &format!("standard_buffer_size: {DEFAULT_STANDARD_BUFFER_SIZE}"),
            "standard_buffer_size: 0",
            1,
        );

        let message = error_message(load_override_config(&content).unwrap_err());

        assert!(message.contains("PEPPY_CONFIG (inline document)"));
        assert!(message.contains("standard_buffer_size"));
    }

    #[test]
    fn override_outdated_lists_missing_paths() {
        let message =
            error_message(load_override_config(r#"{ future_setting: true }"#).unwrap_err());

        assert!(message.contains("PEPPY_CONFIG (inline document)"));
        assert!(message.contains("core_node_name"));
        assert!(message.contains("zenoh"));
        assert!(message.contains("lifecycle"));
        assert!(message.contains("resource_servers"));
        assert!(message.contains("never completed with defaults"));
        assert!(message.contains("every setting must be spelled out"));
    }

    #[test]
    fn outdated_check_runs_after_validation() {
        let content =
            r#"{ zenoh: { managed: { subscriber_buffers: { standard_buffer_size: 0 } } } }"#;

        let message = error_message(load_override_config(content).unwrap_err());

        assert!(message.contains("PEPPY_CONFIG (inline document)"));
        assert!(message.contains("standard_buffer_size"));
        assert!(!message.contains("missing settings this peppy release defines"));
    }

    #[test]
    fn file_override_outdated_stays_untouched() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("outdated.json5");
        let content = b"{ future_setting: true }\n";
        std::fs::write(&path, content).unwrap();

        let message = error_message(load_override_config(path.to_str().unwrap()).unwrap_err());

        assert!(message.contains(&format!("file {}", path.display())));
        assert!(message.contains("core_node_name"));
        assert_eq!(std::fs::read(&path).unwrap(), content);
    }

    #[test]
    fn override_takes_precedence_over_dirs_without_touching_disk() {
        let (_tmp, peppy_dirs, path) = dirs_with_config("{ not json5");
        let before = std::fs::read(&path).unwrap();

        let config =
            load_or_create_with_override(&peppy_dirs, Some(DEFAULT_PEPPY_CONFIG_TEMPLATE.into()))
                .unwrap();

        assert_eq!(config, PeppyConfig::default());
        assert_eq!(std::fs::read(&path).unwrap(), before);

        let tmp = tempdir().unwrap();
        let root = tmp.path().join("never-created");
        let peppy_dirs = PeppyDirs::new(&root);
        load_or_create_with_override(&peppy_dirs, Some(DEFAULT_PEPPY_CONFIG_TEMPLATE.into()))
            .unwrap();
        assert!(!root.exists());
    }

    #[test]
    fn no_override_falls_through_to_disk_flow() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let config = load_or_create_with_override(&peppy_dirs, None).unwrap();
        let path = peppy_dirs.conf_dir().join(PEPPY_CONFIG_FILE);

        assert_eq!(config, PeppyConfig::default());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DEFAULT_PEPPY_CONFIG_TEMPLATE
        );

        let (_tmp, peppy_dirs, path) = dirs_with_config("{}");
        load_or_create_with_override(&peppy_dirs, None).unwrap();
        let completed = std::fs::read_to_string(path).unwrap();
        assert_eq!(
            serde_json5::from_str::<PeppyConfig>(&completed).unwrap(),
            PeppyConfig::default()
        );
        assert!(completion::complete_config_content(&completed).is_none());
    }

    #[test]
    fn partial_splice_survives_unspliceable_remainder() {
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE
            .replacen("  lifecycle: {", r#"  "life\u0063ycle": {"#, 1)
            .replacen(SHUTDOWN_GRACE_FIELD_SNIPPET, "", 1)
            .replacen(API_FIELD_SNIPPET, "", 1);
        let (_tmp, peppy_dirs, path) = dirs_with_config(&content);

        let config = load_or_create(&peppy_dirs).unwrap();
        let completed = std::fs::read_to_string(&path).unwrap();

        assert_eq!(config, PeppyConfig::default());
        assert_ne!(completed, content);
        assert!(completed.contains(API_FIELD_SNIPPET));
        assert!(!completed.contains(SHUTDOWN_GRACE_FIELD_SNIPPET));
        assert!(completed.contains(r#""life\u0063ycle""#));
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), config);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);
    }

    #[test]
    fn override_with_escaped_key_outdated_doc_crashes() {
        let content = DEFAULT_PEPPY_CONFIG_TEMPLATE
            .replacen("  lifecycle: {", r#"  "life\u0063ycle": {"#, 1)
            .replacen(SHUTDOWN_GRACE_FIELD_SNIPPET, "", 1);

        let message = error_message(load_override_config(&content).unwrap_err());

        assert!(message.contains("PEPPY_CONFIG (inline document)"));
        assert!(message.contains("lifecycle.shutdown_grace_secs"));
        assert!(message.contains("never completed with defaults"));
    }

    #[test]
    fn default_topology_is_peer_and_buffers_match_constants() {
        let cfg = PeppyConfig::default();
        let managed = managed(&cfg);
        assert_eq!(cfg.core_node_name, None);
        assert_eq!(managed.local_nodes_topology, LocalNodesTopology::Peer);
        assert!(managed.local_nodes_topology.is_peer());
        assert!(cfg.zenoh.gossip());
        assert!(!LocalNodesTopology::Router.gossip());
        assert_eq!(
            cfg.zenoh.subscriber_buffers().standard_buffer_size,
            DEFAULT_STANDARD_BUFFER_SIZE
        );
        assert_eq!(
            cfg.zenoh.subscriber_buffers().high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
        assert_eq!(cfg.lifecycle.daemon_grace_secs, DEFAULT_DAEMON_GRACE_SECS);
        assert_eq!(
            cfg.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        assert_eq!(cfg.resource_servers.api, DEFAULT_API_URL);
        assert_eq!(
            cfg.zenoh.federation().unwrap().connect_timeout_secs,
            DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS
        );
        assert_eq!(
            cfg.zenoh.federation().unwrap(),
            &FederationConfig::default()
        );
        assert_eq!(
            cfg.zenoh,
            ZenohConfig::Managed(ManagedZenohConfig::default())
        );
        assert_eq!(cfg.zenoh.external_endpoint(), None);
    }

    #[test]
    fn external_accessors_disable_managed_only_behavior() {
        let zenoh = ZenohConfig::External(ExternalZenohConfig {
            endpoint: "tcp/router.internal:7448".to_string(),
        });

        assert!(!zenoh.gossip());
        assert_eq!(
            zenoh.subscriber_buffers(),
            SubscriberBufferConfig::default()
        );
        assert_eq!(zenoh.external_endpoint(), Some("tcp/router.internal:7448"));
        assert_eq!(zenoh.federation(), None);
    }

    #[test]
    fn federation_section_defaults_and_completes() {
        // A managed block with no `federation` block parses with the default
        // and is completed in place with the section.
        let (_tmp, peppy_dirs, path) =
            dirs_with_config(r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            cfg.zenoh.federation().unwrap().connect_timeout_secs,
            DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS
        );
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains("federation: {"));
        assert!(completed.contains(&format!(
            "connect_timeout_secs: {DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS},"
        )));
        // Idempotent: a second load parses to the same config and stops rewriting.
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);

        // An explicit value is honored.
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ zenoh: { managed: { federation: { connect_timeout_secs: 5 } } } }"#,
        );
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.zenoh.federation().unwrap().connect_timeout_secs, 5);
    }

    #[test]
    fn full_federation_block_parses() {
        let content = r#"{
            zenoh: {
                managed: {
                    federation: {
                        connect_timeout_secs: 5,
                        listen_endpoint: "tls/0.0.0.0:7449",
                        cert_path: "/etc/peppy/cert.pem",
                        key_path: "/etc/peppy/key.pem",
                        ca_path: "/etc/peppy/ca.pem",
                    },
                },
            },
        }"#;
        let (_tmp, peppy_dirs, _) = dirs_with_config(content);

        let config = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            config.zenoh.federation(),
            Some(&FederationConfig {
                connect_timeout_secs: 5,
                listen_endpoint: Some("tls/0.0.0.0:7449".into()),
                cert_path: Some("/etc/peppy/cert.pem".into()),
                key_path: Some("/etc/peppy/key.pem".into()),
                ca_path: Some("/etc/peppy/ca.pem".into()),
            })
        );
    }

    #[test]
    fn listener_endpoint_accepts_tls_hosts_and_wildcards() {
        for endpoint in [
            "tls/0.0.0.0:7449",
            "tls/*:7449",
            "tls/[::]:7449",
            "tls/router.example:7449",
            "tls/192.0.2.1:7449",
        ] {
            let content = format!(
                r#"{{ zenoh: {{ managed: {{ federation: {{ listen_endpoint: "{endpoint}" }} }} }} }}"#
            );
            let (_tmp, peppy_dirs, _) = dirs_with_config(&content);
            let config = load_or_create(&peppy_dirs).unwrap();
            assert_eq!(
                config
                    .zenoh
                    .federation()
                    .unwrap()
                    .listen_endpoint
                    .as_deref(),
                Some(endpoint)
            );
        }
    }

    #[test]
    fn invalid_listener_endpoints_fail_loud_and_leave_files_untouched() {
        for (endpoint, expected_message) in [
            ("tcp/0.0.0.0:7449", "tls/<host>:<port>"),
            ("tls/0.0.0.0", "host and port"),
            ("tls/0.0.0.0:0", "integer from 1 through 65535"),
            (
                "tls/0.0.0.0:7449#enable_mtls=true",
                "metadata and endpoint configuration",
            ),
            (
                "tls/0.0.0.0:7449;foo=bar",
                "metadata and endpoint configuration",
            ),
        ] {
            let content = format!(
                r#"{{ zenoh: {{ managed: {{ federation: {{ listen_endpoint: "{endpoint}" }} }} }} }}"#
            );
            let (_tmp, peppy_dirs, path) = dirs_with_config(&content);

            let error = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(error, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains("zenoh.managed.federation.listen_endpoint")
                        && message.contains(expected_message)),
                "expected a listener error for {endpoint:?}, got {error:?}"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
    }

    #[test]
    fn federation_identity_paths_reject_fragment_delimiters() {
        for (field, path) in [
            ("cert_path", "/identity/bad#cert.pem"),
            ("key_path", "/identity/bad;key.pem"),
            ("ca_path", "/identity/bad=ca.pem"),
        ] {
            let content =
                format!(r#"{{ zenoh: {{ managed: {{ federation: {{ {field}: "{path}" }} }} }} }}"#);
            let (_tmp, peppy_dirs, config_path) = dirs_with_config(&content);

            let error = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(error, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains(field) && message.contains("reserved locator delimiter")),
                "expected a path error for {field}, got {error:?}"
            );
            assert_eq!(std::fs::read_to_string(config_path).unwrap(), content);
        }
    }

    #[test]
    fn zenoh_defaults_to_managed_when_absent_or_empty() {
        for content in ["{}", "{ zenoh: {} }"] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);
            let cfg = load_or_create(&peppy_dirs).unwrap();
            assert_eq!(
                cfg.zenoh,
                ZenohConfig::Managed(ManagedZenohConfig::default())
            );
            let completed = std::fs::read_to_string(&path).unwrap();
            assert!(completed.contains("managed: {"));
            assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);
        }

        // An external block carries the exact dial endpoint downstream.
        let endpoint = "tcp/router.internal:7448";
        let (_tmp, peppy_dirs, _) = dirs_with_config(&format!(
            r#"{{ zenoh: {{ external: {{ endpoint: "{endpoint}" }} }} }}"#
        ));
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            cfg.zenoh,
            ZenohConfig::External(ExternalZenohConfig {
                endpoint: endpoint.to_string()
            })
        );
        assert_eq!(cfg.zenoh.external_endpoint(), Some(endpoint));
    }

    #[test]
    fn external_zenoh_accepts_tcp_dial_locators() {
        for endpoint in [
            "tcp/127.0.0.1:7448",
            "tcp/localhost:1",
            "tcp/router-1.internal.example:7448",
            "tcp/[::1]:65535",
            "tcp/[2001:db8::1]:7448",
        ] {
            let content = format!(r#"{{ zenoh: {{ external: {{ endpoint: "{endpoint}" }} }} }}"#);
            let (_tmp, peppy_dirs, _) = dirs_with_config(&content);
            let config = load_or_create(&peppy_dirs).unwrap();
            assert_eq!(config.zenoh.external_endpoint(), Some(endpoint));
        }
    }

    #[test]
    fn endpoint_parser_returns_normalized_host_and_port() {
        assert_eq!(
            parse_endpoint("tls/router.example:7449", "tls", EndpointPurpose::Dial).unwrap(),
            ParsedEndpoint {
                host: "router.example",
                port: 7449,
            }
        );
        assert_eq!(
            parse_endpoint("tls/[2001:db8::1]:7449", "tls", EndpointPurpose::Dial).unwrap(),
            ParsedEndpoint {
                host: "2001:db8::1",
                port: 7449,
            }
        );
    }

    #[test]
    fn invalid_external_zenoh_endpoints_fail_loud_and_leave_files_untouched() {
        for (endpoint, expected_message) in [
            ("", "must not be empty"),
            (
                " tcp/127.0.0.1:7448",
                "must not contain leading or trailing whitespace",
            ),
            (
                "tcp/127.0.0.1:7448 ",
                "must not contain leading or trailing whitespace",
            ),
            ("udp/127.0.0.1:7448", "tcp/<host>:<port>"),
            ("127.0.0.1:7448", "tcp/<host>:<port>"),
            ("tcp/127.0.0.1", "must include a host and port"),
            ("tcp/:7448", "host must not be empty"),
            ("tcp/127.0.0.1:", "port must be an integer"),
            ("tcp/127.0.0.1:+1", "port must be an integer"),
            ("tcp/127.0.0.1:0", "port must be an integer"),
            ("tcp/127.0.0.1:65536", "port must be an integer"),
            ("tcp/0.0.0.0:7448", "0.0.0.0 is only valid for listening"),
            ("tcp/*:7448", "* is only valid for listening"),
            ("tcp/[::]:7448", "[::] is only valid for listening"),
            (
                "tcp/::1:7448",
                "IPv6 addresses must be enclosed in brackets",
            ),
            ("tcp/[::1]7448", "must include a port after the host"),
            ("tcp/[not-ip]:7448", "must be a valid IPv6 address"),
            ("tcp/999.0.0.1:7448", "not a valid IPv4 address"),
            (
                "tcp/bad_host:7448",
                "must be a valid hostname or IP address",
            ),
            (
                "tcp/router.example..:7448",
                "must be a valid hostname or IP address",
            ),
            (
                "tcp/127.0.0.1:7448?prio=1",
                "metadata and endpoint configuration are not supported",
            ),
        ] {
            let content = format!(r#"{{ zenoh: {{ external: {{ endpoint: "{endpoint}" }} }} }}"#);
            let (_tmp, peppy_dirs, path) = dirs_with_config(&content);

            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains("zenoh.external.endpoint")
                        && message.contains(expected_message)),
                "expected an external endpoint error for {endpoint:?}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn exactly_one_zenoh_variant_is_enforced() {
        for (content, expected) in [
            (
                r#"{ zenoh: { managed: {}, external: { endpoint: "tcp/127.0.0.1:7448" } } }"#,
                "zenoh.managed and zenoh.external are mutually exclusive; configure exactly one",
            ),
            (
                r#"{ zenoh: { external: {} } }"#,
                "zenoh.external.endpoint is required",
            ),
            (
                r#"{ zenoh: { external: "tcp/127.0.0.1:7448" } }"#,
                "zenoh.external must be a block like { endpoint: \"tcp/<host>:<port>\" }",
            ),
            (
                r#"{ zenoh: { external: { endpoint: 7448 } } }"#,
                "zenoh.external.endpoint must be a string",
            ),
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);
            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains(expected)),
                "expected {expected:?} for {content}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn external_unknown_fields_fail_loud() {
        for content in [
            r#"{ zenoh: { external: { future_router_option: { enabled: true } } } }"#,
            r#"{
  zenoh: {
    external: {
      endpoint: "tcp/router.internal:7448",
      future_router_option: { enabled: true },
    },
  },
}"#,
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);
            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains("unknown field `zenoh.external.future_router_option`")),
                "expected an unknown external field error for {content}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
    }

    #[test]
    fn zenoh_unknown_fields_fail_loud_and_leave_files_untouched() {
        for (content, expected_message) in [
            (
                r#"{ zenoh: { future_transport_option: true } }"#,
                "unknown field `zenoh.future_transport_option`; zenoh holds exactly one of `managed` or `external`",
            ),
            (
                r#"{ zenoh: { managed: { subscriber_buffers: { standard_buffer_sze: 64 } } } }"#,
                "zenoh.managed: unknown field `standard_buffer_sze`",
            ),
            (
                r#"{ zenoh: { managed: { federation: { connect_timeout_sec: 5 } } } }"#,
                "zenoh.managed: unknown field `connect_timeout_sec`",
            ),
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);

            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains(expected_message)),
                "expected an unknown-field error containing {expected_message:?}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
    }

    #[test]
    fn core_node_name_completes_to_explicit_null_idempotently() {
        // An older file without the knob gains the explicit `null` line
        // (null = derive the default), and a second load parses to the same
        // config without rewriting the file again.
        let (_tmp, peppy_dirs, path) =
            dirs_with_config(r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.core_node_name, None);
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains("core_node_name: null,"));
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);

        // A file that already spells the knob as `null` parses to `None` and
        // does not gain a second line.
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ core_node_name: null }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.core_node_name, None);
        let completed = std::fs::read_to_string(&path).unwrap();
        assert_eq!(completed.matches("core_node_name:").count(), 1);
    }

    #[test]
    fn explicit_core_node_name_round_trips_to_some() {
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ core_node_name: "robot-7" }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.core_node_name.as_deref(), Some("robot-7"));

        // Completion appends the other knobs but never touches the user's
        // value, and a second load is idempotent.
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains(r#"core_node_name: "robot-7""#));
        assert!(!completed.contains("core_node_name: null"));
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);
    }

    #[test]
    fn invalid_core_node_name_fails_loud_and_leaves_file_untouched() {
        for bad in [
            r#"{ core_node_name: "" }"#,
            r#"{ core_node_name: "has space" }"#,
            r#"{ core_node_name: "robot/7" }"#,
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(bad);
            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m))
                    if m.contains("core_node_name")
                        && m.contains(PEPPY_CONFIG_FILE)
                        && m.contains(ALLOWED_CONFIG_CHARS)),
                "expected a core-node-name validation error for {bad}, got: {err:?}"
            );
            // Invalid names fail BEFORE completion: the file keeps omitting
            // knobs and is not rewritten, same as the malformed case.
            assert_eq!(std::fs::read_to_string(&path).unwrap(), bad);
        }
    }

    #[test]
    fn core_node_name_length_cap_is_enforced() {
        // Exactly at the cap is accepted...
        let max = "x".repeat(MAX_CORE_NODE_NAME_LEN);
        let (_tmp, peppy_dirs, _) = dirs_with_config(&format!(r#"{{ core_node_name: "{max}" }}"#));
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.core_node_name.as_deref(), Some(max.as_str()));

        // ...one past it fails loud with the file untouched.
        let over = "x".repeat(MAX_CORE_NODE_NAME_LEN + 1);
        let content = format!(r#"{{ core_node_name: "{over}" }}"#);
        let (_tmp, peppy_dirs, path) = dirs_with_config(&content);
        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m))
                if m.contains("core_node_name")),
            "expected a core-node-name validation error, got: {err:?}"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
    }

    #[test]
    fn zero_federation_timeout_fails_loud() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ zenoh: { managed: { federation: { connect_timeout_secs: 0 } } } }"#,
        );

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("connect_timeout_secs")),
            "expected a federation-timeout validation error, got: {err:?}"
        );
    }

    #[test]
    fn resource_servers_api_is_read_and_defaults() {
        // An explicit api is honored.
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ resource_servers: { api: "http://localhost:9000" } }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.resource_servers.api, "http://localhost:9000");
        assert_eq!(managed(&cfg).local_nodes_topology, LocalNodesTopology::Peer);

        // An empty block falls back to the build's default backend URL.
        let (_tmp, peppy_dirs, _) = dirs_with_config(r#"{ resource_servers: {} }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.resource_servers.api, DEFAULT_API_URL);
    }

    #[test]
    fn parses_partial_lifecycle_block() {
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ lifecycle: { daemon_grace_secs: 600 } }"#);

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.lifecycle.daemon_grace_secs, 600);
        // A field omitted from a partial lifecycle block falls back to its default.
        assert_eq!(
            cfg.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        // Omitted blocks still fall back to their defaults.
        assert_eq!(managed(&cfg).local_nodes_topology, LocalNodesTopology::Peer);
        assert_eq!(
            cfg.zenoh.subscriber_buffers(),
            SubscriberBufferConfig::default()
        );
    }

    #[test]
    fn sub_minimum_shutdown_grace_fails_loud() {
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ lifecycle: { shutdown_grace_secs: 0 } }"#);

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("shutdown_grace_secs")),
            "expected a shutdown-grace validation error, got: {err:?}"
        );
    }

    #[test]
    fn sub_minimum_grace_fails_loud_and_leaves_file_untouched() {
        let invalid = r#"{ lifecycle: { daemon_grace_secs: 5 } }"#;
        let (_tmp, peppy_dirs, path) = dirs_with_config(invalid);

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("daemon_grace_secs")),
            "expected a grace-period validation error, got: {err:?}"
        );
        // Out-of-range values fail BEFORE completion: the file keeps omitting
        // knobs and is not rewritten, same as the malformed case.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), invalid);
    }

    #[test]
    fn creates_file_verbatim_on_first_run() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let path = peppy_dirs.conf_dir().join(PEPPY_CONFIG_FILE);
        assert!(!path.exists());

        let cfg = load_or_create(&peppy_dirs).unwrap();

        assert!(path.exists());
        // Verbatim write preserves the template's comments byte-for-byte.
        let written = std::fs::read_to_string(&path).unwrap();
        assert_eq!(written, DEFAULT_PEPPY_CONFIG_TEMPLATE);
        assert_eq!(cfg, PeppyConfig::default());
    }

    #[test]
    fn load_is_idempotent_and_reads_existing_file() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let first = load_or_create(&peppy_dirs).unwrap();
        let second = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(first, second);
        // A file that already spells out every knob is not rewritten.
        let path = peppy_dirs.conf_dir().join(PEPPY_CONFIG_FILE);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DEFAULT_PEPPY_CONFIG_TEMPLATE
        );
    }

    #[test]
    fn completes_missing_fields_while_preserving_user_values() {
        let (_tmp, peppy_dirs, path) = dirs_with_config(
            r#"{ zenoh: { managed: { local_nodes_topology: "router" } }, lifecycle: { daemon_grace_secs: 45 } }"#,
        );

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            managed(&cfg).local_nodes_topology,
            LocalNodesTopology::Router
        );
        assert_eq!(cfg.lifecycle.daemon_grace_secs, 45);
        assert_eq!(
            cfg.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        assert_eq!(
            cfg.zenoh.subscriber_buffers(),
            SubscriberBufferConfig::default()
        );

        // The user's values survive in the file and the omitted knobs now
        // appear in it with their defaults.
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains(r#"local_nodes_topology: "router""#));
        assert!(completed.contains("daemon_grace_secs: 45"));
        assert!(completed.contains(&format!(
            "standard_buffer_size: {DEFAULT_STANDARD_BUFFER_SIZE},"
        )));
        assert!(completed.contains(&format!(
            "shutdown_grace_secs: {DEFAULT_SHUTDOWN_GRACE_SECS},"
        )));

        // A second load parses the completed file to the same config and no
        // longer rewrites it.
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);
    }

    /// The cross-release guarantee end to end: one load brings a file written
    /// by an older release (settings missing) up to the full current schema on
    /// disk, with the user's values preserved. The expected settings are
    /// derived from the struct itself, so this test never needs editing when a
    /// field is added.
    #[test]
    fn old_release_file_gains_every_current_schema_leaf() {
        let (_tmp, peppy_dirs, path) = dirs_with_config(
            r#"{ zenoh: { managed: { local_nodes_topology: "router" } }, lifecycle: { daemon_grace_secs: 45 } }"#,
        );

        load_or_create(&peppy_dirs).unwrap();

        let on_disk = std::fs::read_to_string(&path).unwrap();
        let doc: serde_json::Value = serde_json5::from_str(&on_disk).unwrap();
        let schema = serde_json::to_value(PeppyConfig::default()).unwrap();
        for leaf in completion::leaf_paths(&schema) {
            // Presence is what matters: `core_node_name` is spelled as an
            // explicit null, so its key must exist while its value stays null.
            let mut value = Some(&doc);
            for segment in leaf.split('.') {
                value = value.and_then(|nested| nested.get(segment));
            }
            assert!(
                value.is_some(),
                "setting {leaf} missing from completed file:\n{on_disk}"
            );
        }

        // The old release's values survived the completion.
        assert_eq!(
            doc["zenoh"]["managed"]["local_nodes_topology"],
            serde_json::json!("router")
        );
        assert_eq!(doc["lifecycle"]["daemon_grace_secs"], serde_json::json!(45));

        // A second load has nothing left to add and leaves the bytes alone.
        load_or_create(&peppy_dirs).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), on_disk);
    }

    #[test]
    fn parses_partial_file_filling_defaults() {
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#);

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            managed(&cfg).local_nodes_topology,
            LocalNodesTopology::Router
        );
        assert!(!cfg.zenoh.gossip());
        // A missing subscriber-buffers block falls back to the built-in defaults.
        assert_eq!(
            cfg.zenoh.subscriber_buffers().standard_buffer_size,
            DEFAULT_STANDARD_BUFFER_SIZE
        );
        assert_eq!(
            cfg.zenoh.subscriber_buffers().high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
    }

    #[test]
    fn parses_empty_object_as_all_defaults() {
        let (_tmp, peppy_dirs, _) = dirs_with_config("{}");

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg, PeppyConfig::default());
    }

    #[test]
    fn round_trips_custom_config() {
        let custom = PeppyConfig {
            core_node_name: Some("lab-bench-1".to_string()),
            zenoh: ZenohConfig::Managed(ManagedZenohConfig {
                local_nodes_topology: LocalNodesTopology::Router,
                subscriber_buffers: SubscriberBufferConfig {
                    standard_buffer_size: 64,
                    high_throughput_buffer_size: 4096,
                },
                federation: FederationConfig {
                    connect_timeout_secs: 45,
                    listen_endpoint: Some("tls/0.0.0.0:7449".to_string()),
                    cert_path: Some("/etc/peppy/federation/cert.pem".into()),
                    key_path: Some("/etc/peppy/federation/key.pem".into()),
                    ca_path: Some("/etc/peppy/federation/ca.pem".into()),
                },
            }),
            lifecycle: LifecycleConfig {
                daemon_grace_secs: 240,
                shutdown_grace_secs: 5,
            },
            resource_servers: ResourceServers {
                api: "http://localhost:9000".to_string(),
            },
        };
        let serialized = serde_json5::to_string(&custom).unwrap();
        let reparsed: PeppyConfig = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed, custom);
    }

    #[test]
    fn topology_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(LocalNodesTopology::Router).unwrap(),
            serde_json::json!("router")
        );
        assert_eq!(
            serde_json::to_value(LocalNodesTopology::Peer).unwrap(),
            serde_json::json!("peer")
        );
    }

    #[test]
    fn malformed_file_fails_loud_and_is_left_untouched() {
        let malformed = r#"{ zenoh: { managed: { local_nodes_topology: "router", subscriber_buffers: { standard_buffer_size: "not a number" } } } }"#;
        let (_tmp, peppy_dirs, path) = dirs_with_config(malformed);

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(_))),
            "expected a parse error, got: {err:?}"
        );
        // A failed load never modifies the file, even though it omits knobs.
        assert_eq!(std::fs::read_to_string(&path).unwrap(), malformed);
    }

    #[test]
    fn zero_standard_buffer_size_fails_loud() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ zenoh: { managed: { subscriber_buffers: { standard_buffer_size: 0 } } } }"#,
        );

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("standard_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn zero_high_throughput_buffer_size_fails_loud() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ zenoh: { managed: { subscriber_buffers: { high_throughput_buffer_size: 0 } } } }"#,
        );

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("high_throughput_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn accepts_minimal_nonzero_buffer_sizes() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ zenoh: { managed: { subscriber_buffers: { standard_buffer_size: 1, high_throughput_buffer_size: 1 } } } }"#,
        );

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.zenoh.subscriber_buffers().standard_buffer_size, 1);
        assert_eq!(
            cfg.zenoh.subscriber_buffers().high_throughput_buffer_size,
            1
        );
    }

    #[cfg(unix)]
    #[test]
    fn completion_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, peppy_dirs, path) =
            dirs_with_config(r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();

        load_or_create(&peppy_dirs).unwrap();

        // The staged tmp file is born 0600; the completed file must come out
        // with the user's permissions, not the tmp file's.
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("lifecycle"),
            "completion did not run"
        );
        assert_eq!(mode, 0o640);
    }

    #[cfg(unix)]
    #[test]
    fn completion_writes_through_a_symlinked_config() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        // A dotfiles-style setup: the file under conf/ is a symlink to a
        // config managed elsewhere.
        let real = tmp.path().join("dotfiles_peppy.json5");
        std::fs::write(
            &real,
            r#"{ zenoh: { managed: { local_nodes_topology: "router" } } }"#,
        )
        .unwrap();
        let link = conf_dir.join(PEPPY_CONFIG_FILE);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            managed(&cfg).local_nodes_topology,
            LocalNodesTopology::Router
        );

        // The symlink survives and the completed content landed in its target.
        assert!(
            std::fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );
        assert!(
            std::fs::read_to_string(&real)
                .unwrap()
                .contains("lifecycle")
        );
    }
}
