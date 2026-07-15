//! Global daemon configuration read from `~/.peppy/conf/peppy_config.json5`.
//!
//! This is the single user-facing switch for the messaging topology. The daemon
//! reads it ONCE at startup (see `peppy service serve`), creating the file from the
//! bundled default template if it is missing, and applies the result to its own
//! core-node session and to every node it spawns. Editing the file takes effect
//! after a daemon restart.
//!
//! A well-formed file that omits settings (typically one written by an older
//! peppy before a new knob existed) is completed in place: the missing entries
//! are appended with their default values and explanatory comments, so the file
//! on disk always lists every available knob. The user's own values, comments,
//! and unknown keys are otherwise preserved byte-for-byte (see [`completion`]);
//! when appending defaults, completion may add a structural separator comma
//! after the prior final entry. Each setting added this way is logged at info
//! level, so the first start after a peppy upgrade shows exactly which new
//! settings appeared in the file.
//!
//! Unlike `repositories.json5`, a malformed `peppy_config.json5` fails loud at
//! startup ([`load_or_create`] returns `Err`) instead of falling back to
//! defaults: the mode and buffer sizes determine the whole mesh's routing model
//! and backpressure, so a hand-edited typo must surface immediately rather than
//! silently reverting to peer mode. A malformed file is never rewritten.

mod completion;

use crate::atomic_write::publish_atomic;
use crate::consts::PeppyDirs;
use crate::error::{Error, ParsingError, Result};
use config::consts::ALLOWED_CONFIG_CHARS;
use config::peppy_config::{
    DEFAULT_DAEMON_GRACE_SECS, DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE, DEFAULT_SHUTDOWN_GRACE_SECS,
    DEFAULT_STANDARD_BUFFER_SIZE, PeerConfig,
};
use config::runtime::Name;
use serde::{Deserialize, Deserializer, Serialize};
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::path::Path;

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
  // machine-specific default (core-node-...). Names must be unique across all
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

/// The `mode` entry with its explanatory comment.
const MODE_SECTION_SNIPPET: &str = r#"  //   "peer"   - Zenoh peer sessions with gossip: nodes form direct
  //              peer-to-peer links and data stops relaying through the router.
  //   "router" - gossip off: all traffic relays through the central zenohd
  //              router.
  // Container nodes in a separate network namespace (Lima on macOS) always use
  // the router path regardless of this setting.
  mode: "peer",
"#;

/// The `peer.standard_buffer_size` entry, indented for the `peer` block.
const STANDARD_BUFFER_FIELD_SNIPPET: &str = const_format::concatcp!(
    "    standard_buffer_size: ",
    DEFAULT_STANDARD_BUFFER_SIZE,
    ",\n"
);

/// The `peer.high_throughput_buffer_size` entry, indented for the `peer` block.
const HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET: &str = const_format::concatcp!(
    "    high_throughput_buffer_size: ",
    DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
    ",\n"
);

/// The whole `peer` block with its explanatory comment.
const PEER_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"  // Subscriber channel buffer sizes (number of in-flight messages) per QoS
  // tier, used in peer mode where there is no router relay to buffer between a
  // publisher and a subscriber. Defaults match peppy's built-in behavior; only
  // edit to tune backpressure.
  peer: {
"#,
    STANDARD_BUFFER_FIELD_SNIPPET,
    HIGH_THROUGHPUT_BUFFER_FIELD_SNIPPET,
    "  },\n"
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

/// The `federation.connect_timeout_secs` entry with its comment, indented for
/// the `federation` block.
const FEDERATION_TIMEOUT_FIELD_SNIPPET: &str = const_format::concatcp!(
    r#"    // Seconds the daemon spends resolving your per-user cloud router before
    // giving up for this attempt (it retries in the background). Bounds the
    // federation done at startup and on each `peppy auth login`/`logout`;
    // minimum 1. If the backend is unreachable within this window the daemon
    // stays standalone rather than blocking.
    connect_timeout_secs: "#,
    DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
    ",\n"
);

/// The whole `federation` block with its explanatory comment.
const FEDERATION_SECTION_SNIPPET: &str = const_format::concatcp!(
    r#"  // Per-user zenoh-router federation: how the daemon links its local router to
  // your private cloud router. Only tuned to bound a slow/unreachable backend
  // during the federation step.
  federation: {
"#,
    FEDERATION_TIMEOUT_FIELD_SNIPPET,
    "  },\n"
);

/// The `zenohd.mode` entry with its comment, indented for the `zenohd` block.
const ZENOHD_MODE_FIELD_SNIPPET: &str = r#"    // "managed": peppy starts, monitors, and stops its bundled zenohd.
    // "external": peppy connects to a router you run at `endpoint` and never
    //             starts, reconfigures, restarts, or stops it.
    mode: "managed",
"#;

/// The whole `zenohd` block.
const ZENOHD_SECTION_SNIPPET: &str =
    const_format::concatcp!("  zenohd: {\n", ZENOHD_MODE_FIELD_SNIPPET, "  },\n");

/// The full bundled default config, composed from the snippets above.
const DEFAULT_PEPPY_CONFIG_TEMPLATE: &str = const_format::concatcp!(
    TEMPLATE_HEADER,
    "{\n",
    CORE_NODE_NAME_SECTION_SNIPPET,
    "\n",
    MODE_SECTION_SNIPPET,
    "\n",
    PEER_SECTION_SNIPPET,
    "\n",
    LIFECYCLE_SECTION_SNIPPET,
    "\n",
    RESOURCE_SERVERS_SECTION_SNIPPET,
    "\n",
    FEDERATION_SECTION_SNIPPET,
    "\n",
    ZENOHD_SECTION_SNIPPET,
    "}\n"
);

/// The messaging topology the daemon runs in.
///
/// `Peer` keeps gossip on so nodes form direct peer-to-peer links; `Router`
/// turns gossip off so every node routes through the central `zenohd`. The
/// `gossip()` mapping is the single source of truth tying this user-facing
/// choice to the `DiscoveryConfig.gossip` flag the sessions actually read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Peer,
    Router,
}

impl Mode {
    /// Whether this is peer mode (direct peer-to-peer links).
    pub fn is_peer(self) -> bool {
        matches!(self, Mode::Peer)
    }

    /// Mode to gossip mapping: peer enables gossip, router disables it.
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
/// [`LifecycleConfig::default`], matching the `PeerConfig` pattern.
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FederationConfig {
    pub connect_timeout_secs: u64,
}

impl Default for FederationConfig {
    fn default() -> Self {
        Self {
            connect_timeout_secs: DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS,
        }
    }
}

/// Whether peppy owns the local zenoh router or connects to one the operator
/// owns. The internally tagged variants make ownership explicit and ensure an
/// external router carries the network address peppy must dial.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum ZenohdConfig {
    /// Peppy starts, monitors, restarts, and stops its bundled zenohd.
    #[default]
    Managed,
    /// Peppy adopts a responsive router at `endpoint` without managing its
    /// process or configuration.
    External { endpoint: String },
}

impl<'de> Deserialize<'de> for ZenohdConfig {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // Deserialize the tagged shape manually so invalid combinations and
        // unknown fields inside the ownership block fail loud. Unknown
        // top-level config fields remain forward compatible.
        #[derive(Deserialize)]
        #[serde(rename_all = "snake_case")]
        enum Ownership {
            Managed,
            External,
        }
        #[derive(Deserialize)]
        struct Wire {
            mode: Option<Ownership>,
            #[serde(flatten)]
            fields: BTreeMap<String, serde_json::Value>,
        }

        let mut wire = Wire::deserialize(deserializer)?;
        let endpoint = wire.fields.remove("endpoint");
        if let Some(field) = wire.fields.keys().next() {
            return Err(serde::de::Error::custom(format!(
                "unknown field `zenohd.{field}`"
            )));
        }

        match wire.mode.unwrap_or(Ownership::Managed) {
            Ownership::Managed => {
                if endpoint.is_some() {
                    return Err(serde::de::Error::custom(
                        "zenohd.endpoint is only valid in external mode",
                    ));
                }
                Ok(Self::Managed)
            }
            Ownership::External => match endpoint {
                Some(serde_json::Value::String(endpoint)) => Ok(Self::External { endpoint }),
                Some(_) => Err(serde::de::Error::custom("zenohd.endpoint must be a string")),
                None => Err(serde::de::Error::custom(
                    "zenohd.endpoint is required in external mode",
                )),
            },
        }
    }
}

impl ZenohdConfig {
    /// The full Zenoh locator peppy should dial when the router is externally
    /// managed, or `None` when peppy should manage its own router.
    pub fn external_endpoint(&self) -> Option<&str> {
        match self {
            Self::Managed => None,
            Self::External { endpoint } => Some(endpoint),
        }
    }

    fn validate(&self) -> std::result::Result<(), String> {
        let Some(endpoint) = self.external_endpoint() else {
            return Ok(());
        };
        validate_tcp_dial_endpoint(endpoint)
    }
}

/// Validates the deliberately narrow locator surface supported for an external
/// router. Peppy currently transports its daemon and node sessions over TCP, so
/// accepting another Zenoh protocol here would create a config the rest of the
/// stack cannot honor. This is syntax-only: hostnames are not resolved while
/// loading the config.
fn validate_tcp_dial_endpoint(endpoint: &str) -> std::result::Result<(), String> {
    if endpoint.is_empty() {
        return Err("must not be empty".to_string());
    }
    if endpoint.trim() != endpoint {
        return Err("must not contain leading or trailing whitespace".to_string());
    }

    let Some(address) = endpoint.strip_prefix("tcp/") else {
        return Err("must use the tcp/<host>:<port> locator form".to_string());
    };
    if address.contains(['?', '#']) {
        return Err("metadata and endpoint configuration are not supported".to_string());
    }

    let (host, port, bracketed) = split_tcp_host_port(address)?;
    validate_dial_host(host, bracketed)?;
    if port.is_empty() || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err("port must be an integer from 1 through 65535".to_string());
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| "port must be an integer from 1 through 65535".to_string())?;
    if port == 0 {
        return Err("port must be an integer from 1 through 65535".to_string());
    }
    Ok(())
}

fn split_tcp_host_port(address: &str) -> std::result::Result<(&str, &str, bool), String> {
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

fn validate_dial_host(host: &str, bracketed: bool) -> std::result::Result<(), String> {
    if host.is_empty() {
        return Err("host must not be empty".to_string());
    }

    if bracketed {
        let address = host
            .parse::<Ipv6Addr>()
            .map_err(|_| "bracketed host must be a valid IPv6 address".to_string())?;
        return if address.is_unspecified() {
            Err(
                "host must be dialable; the wildcard address [::] is only valid for listening"
                    .to_string(),
            )
        } else {
            Ok(())
        };
    }

    if let Ok(address) = host.parse::<Ipv4Addr>() {
        return if address.is_unspecified() {
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
        return Err("host must be dialable; * is only valid for listening".to_string());
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

/// The whole `peppy_config.json5` document. Every field is serde-defaulted so a
/// partial or older file still parses; extra unknown keys are tolerated (this is
/// a user-edited file, forward-compat beats strictness here).
///
/// Every DEFAULTED field must also serialize under `Default` (no
/// `skip_serializing_if`): the schema-coverage pin in [`completion`] enumerates
/// those settings by serializing this struct's default value, and a field it
/// cannot see would escape the guarantee that older files gain every new
/// default on upgrade. Required fields that exist only in a non-default tagged
/// variant (currently `zenohd.endpoint`) are pinned separately and deliberately
/// are not invented by completion.
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
    pub mode: Mode,
    #[serde(default)]
    pub peer: PeerConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    #[serde(default)]
    pub resource_servers: ResourceServers,
    #[serde(default)]
    pub federation: FederationConfig,
    #[serde(default)]
    pub zenohd: ZenohdConfig,
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

        let buffer_sizes = [
            ("standard_buffer_size", self.peer.standard_buffer_size),
            (
                "high_throughput_buffer_size",
                self.peer.high_throughput_buffer_size,
            ),
        ];
        for (field, value) in buffer_sizes {
            if value == 0 {
                return Err(Error::Parsing(ParsingError::CannotParseConfig(format!(
                    "invalid peer buffer size: {field} must be > 0"
                ))));
            }
        }

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
        // A 0 bound would make the federation pull either give up instantly or
        // (to the HTTP client) wait forever; reject a hand-edited too-small value
        // loud at load time, same as the grace periods.
        if self.federation.connect_timeout_secs < MIN_FEDERATION_CONNECT_TIMEOUT_SECS {
            return Err(Error::Parsing(ParsingError::CannotParseConfig(format!(
                "invalid federation.connect_timeout_secs: must be >= {MIN_FEDERATION_CONNECT_TIMEOUT_SECS}"
            ))));
        }
        self.zenohd.validate().map_err(|message| {
            Error::Parsing(ParsingError::CannotParseConfig(format!(
                "{PEPPY_CONFIG_FILE}: invalid zenohd.endpoint: {message}"
            )))
        })?;
        Ok(())
    }
}

/// Reads the global config from `~/.peppy/conf/peppy_config.json5`, creating it
/// from the bundled default template (verbatim, so comments survive) when it
/// does not exist, and appending defaults for any setting an existing file
/// omits so the file on disk always lists every available knob.
///
/// Read ONCE by the daemon at startup. A malformed existing file returns `Err`
/// (fail loud) rather than defaulting, since mode and buffer sizes are
/// load-bearing for the whole mesh. This intentionally differs from
/// `ensure_default_repos`, which only warns on a bad repos file.
pub fn load_or_create(peppy_dirs: &PeppyDirs) -> Result<PeppyConfig> {
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

/// Appends template defaults for every setting the user's file omits, so the
/// on-disk file spells out all available knobs, and logs at info level which
/// settings were added so a release upgrade leaves a visible trace.
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

    #[test]
    fn default_mode_is_peer_and_buffers_match_constants() {
        let cfg = PeppyConfig::default();
        assert_eq!(cfg.core_node_name, None);
        assert_eq!(cfg.mode, Mode::Peer);
        assert!(cfg.mode.is_peer());
        assert!(cfg.mode.gossip());
        assert!(!Mode::Router.gossip());
        assert_eq!(cfg.peer.standard_buffer_size, DEFAULT_STANDARD_BUFFER_SIZE);
        assert_eq!(
            cfg.peer.high_throughput_buffer_size,
            DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE
        );
        assert_eq!(cfg.lifecycle.daemon_grace_secs, DEFAULT_DAEMON_GRACE_SECS);
        assert_eq!(
            cfg.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        assert_eq!(cfg.resource_servers.api, DEFAULT_API_URL);
        assert_eq!(
            cfg.federation.connect_timeout_secs,
            DEFAULT_FEDERATION_CONNECT_TIMEOUT_SECS
        );
        assert_eq!(cfg.zenohd, ZenohdConfig::Managed);
        assert_eq!(cfg.zenohd.external_endpoint(), None);
    }

    #[test]
    fn federation_section_defaults_and_completes() {
        // An existing file with no `federation` block parses with the default
        // and is completed in place with the section (the auto-complete path
        // older files rely on).
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ mode: "router" }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            cfg.federation.connect_timeout_secs,
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
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ federation: { connect_timeout_secs: 5 } }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.federation.connect_timeout_secs, 5);
    }

    #[test]
    fn zenohd_section_defaults_and_completes() {
        // An older file gains the explicit managed default, and loading the
        // completed file again leaves it byte-for-byte unchanged.
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ mode: "router" }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.zenohd, ZenohdConfig::Managed);
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains("zenohd: {"));
        assert!(completed.contains("mode: \"managed\","));
        assert_eq!(load_or_create(&peppy_dirs).unwrap(), cfg);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), completed);

        // A present but empty section receives the missing defaulted field,
        // just like a wholly absent section.
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ zenohd: {} }"#);
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.zenohd, ZenohdConfig::Managed);
        assert!(
            std::fs::read_to_string(path)
                .unwrap()
                .contains("mode: \"managed\",")
        );

        // External mode carries the exact dial endpoint downstream.
        let endpoint = "tcp/router.internal:7448";
        let (_tmp, peppy_dirs, _) = dirs_with_config(&format!(
            r#"{{ zenohd: {{ mode: "external", endpoint: "{endpoint}" }} }}"#
        ));
        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(
            cfg.zenohd,
            ZenohdConfig::External {
                endpoint: endpoint.to_string()
            }
        );
        assert_eq!(cfg.zenohd.external_endpoint(), Some(endpoint));
    }

    #[test]
    fn external_zenohd_accepts_tcp_dial_locators() {
        for endpoint in [
            "tcp/127.0.0.1:7448",
            "tcp/localhost:1",
            "tcp/router-1.internal.example:7448",
            "tcp/[::1]:65535",
            "tcp/[2001:db8::1]:7448",
        ] {
            let content =
                format!(r#"{{ zenohd: {{ mode: "external", endpoint: "{endpoint}" }} }}"#);
            let (_tmp, peppy_dirs, _) = dirs_with_config(&content);
            let config = load_or_create(&peppy_dirs).unwrap();
            assert_eq!(config.zenohd.external_endpoint(), Some(endpoint));
        }
    }

    #[test]
    fn invalid_external_zenohd_endpoints_fail_loud_and_leave_files_untouched() {
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
            let content =
                format!(r#"{{ zenohd: {{ mode: "external", endpoint: "{endpoint}" }} }}"#);
            let (_tmp, peppy_dirs, path) = dirs_with_config(&content);

            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains(PEPPY_CONFIG_FILE)
                        && message.contains("zenohd.endpoint")
                        && message.contains(expected_message)),
                "expected a zenohd endpoint error for {endpoint:?}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn invalid_zenohd_shapes_fail_loud_and_leave_files_untouched() {
        for content in [
            r#"{ zenohd: { mode: "managed", endpoint: "tcp/127.0.0.1:7448" } }"#,
            r#"{ zenohd: { mode: "external" } }"#,
            r#"{ zenohd: { mode: "something_else" } }"#,
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);
            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains(PEPPY_CONFIG_FILE)),
                "expected a zenohd shape error for {content}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content);
        }
    }

    #[test]
    fn zenohd_unknown_fields_fail_loud_in_every_mode() {
        for content in [
            r#"{ zenohd: { future_router_option: { enabled: true } } }"#,
            r#"{
  zenohd: {
    mode: "external",
    endpoint: "tcp/router.internal:7448",
    future_router_option: { enabled: true },
  },
}"#,
        ] {
            let (_tmp, peppy_dirs, path) = dirs_with_config(content);
            let err = load_or_create(&peppy_dirs).unwrap_err();
            assert!(
                matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref message))
                    if message.contains("unknown field `zenohd.future_router_option`")),
                "expected an unknown zenohd field error for {content}, got: {err:?}"
            );
            assert_eq!(std::fs::read_to_string(path).unwrap(), content);
        }
    }

    #[test]
    fn core_node_name_completes_to_explicit_null_idempotently() {
        // An older file without the knob gains the explicit `null` line
        // (null = derive the default), and a second load parses to the same
        // config without rewriting the file again.
        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ mode: "router" }"#);
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
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ federation: { connect_timeout_secs: 0 } }"#);

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
        assert_eq!(cfg.mode, Mode::Peer);

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
        assert_eq!(cfg.mode, Mode::Peer);
        assert_eq!(cfg.peer, PeerConfig::default());
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
        let (_tmp, peppy_dirs, path) =
            dirs_with_config(r#"{ mode: "router", lifecycle: { daemon_grace_secs: 45 } }"#);

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.mode, Mode::Router);
        assert_eq!(cfg.lifecycle.daemon_grace_secs, 45);
        assert_eq!(
            cfg.lifecycle.shutdown_grace_secs,
            DEFAULT_SHUTDOWN_GRACE_SECS
        );
        assert_eq!(cfg.peer, PeerConfig::default());

        // The user's values survive in the file and the omitted knobs now
        // appear in it with their defaults.
        let completed = std::fs::read_to_string(&path).unwrap();
        assert!(completed.contains(r#"mode: "router""#));
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
        let (_tmp, peppy_dirs, path) =
            dirs_with_config(r#"{ mode: "router", lifecycle: { daemon_grace_secs: 45 } }"#);

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
        assert_eq!(doc["mode"], serde_json::json!("router"));
        assert_eq!(doc["lifecycle"]["daemon_grace_secs"], serde_json::json!(45));

        // A second load has nothing left to add and leaves the bytes alone.
        load_or_create(&peppy_dirs).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), on_disk);
    }

    #[test]
    fn parses_partial_file_filling_defaults() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(r#"{ mode: "router" }"#);

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.mode, Mode::Router);
        assert!(!cfg.mode.gossip());
        // Missing peer block falls back to the built-in defaults.
        assert_eq!(cfg.peer.standard_buffer_size, DEFAULT_STANDARD_BUFFER_SIZE);
        assert_eq!(
            cfg.peer.high_throughput_buffer_size,
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
            mode: Mode::Router,
            peer: PeerConfig {
                standard_buffer_size: 64,
                high_throughput_buffer_size: 4096,
            },
            lifecycle: LifecycleConfig {
                daemon_grace_secs: 240,
                shutdown_grace_secs: 5,
            },
            resource_servers: ResourceServers {
                api: "http://localhost:9000".to_string(),
            },
            federation: FederationConfig {
                connect_timeout_secs: 45,
            },
            zenohd: ZenohdConfig::External {
                endpoint: "tcp/router.internal:7448".to_string(),
            },
        };
        let serialized = serde_json5::to_string(&custom).unwrap();
        let reparsed: PeppyConfig = serde_json5::from_str(&serialized).unwrap();
        assert_eq!(reparsed, custom);
    }

    #[test]
    fn mode_serializes_snake_case() {
        assert_eq!(
            serde_json::to_value(Mode::Router).unwrap(),
            serde_json::json!("router")
        );
        assert_eq!(
            serde_json::to_value(Mode::Peer).unwrap(),
            serde_json::json!("peer")
        );
    }

    #[test]
    fn malformed_file_fails_loud_and_is_left_untouched() {
        let malformed = r#"{ mode: "router", peer: { standard_buffer_size: "not a number" } }"#;
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
        let (_tmp, peppy_dirs, _) = dirs_with_config(r#"{ peer: { standard_buffer_size: 0 } }"#);

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("standard_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn zero_high_throughput_buffer_size_fails_loud() {
        let (_tmp, peppy_dirs, _) =
            dirs_with_config(r#"{ peer: { high_throughput_buffer_size: 0 } }"#);

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("high_throughput_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn accepts_minimal_nonzero_buffer_sizes() {
        let (_tmp, peppy_dirs, _) = dirs_with_config(
            r#"{ peer: { standard_buffer_size: 1, high_throughput_buffer_size: 1 } }"#,
        );

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.peer.standard_buffer_size, 1);
        assert_eq!(cfg.peer.high_throughput_buffer_size, 1);
    }

    #[cfg(unix)]
    #[test]
    fn completion_preserves_file_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let (_tmp, peppy_dirs, path) = dirs_with_config(r#"{ mode: "router" }"#);
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
        std::fs::write(&real, r#"{ mode: "router" }"#).unwrap();
        let link = conf_dir.join(PEPPY_CONFIG_FILE);
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.mode, Mode::Router);

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
