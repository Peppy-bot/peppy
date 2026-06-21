//! Global daemon configuration read from `~/.peppy/conf/peppy_config.json5`.
//!
//! This is the single user-facing switch for the messaging topology. The daemon
//! reads it ONCE at startup (see `peppy serve`), creating the file from the
//! bundled default template if it is missing, and applies the result to its own
//! core-node session and to every node it spawns. Editing the file takes effect
//! after a daemon restart.
//!
//! A well-formed file that omits settings (typically one written by an older
//! peppy before a new knob existed) is completed in place: the missing entries
//! are appended with their default values and explanatory comments, so the file
//! on disk always lists every available knob. The user's own values, comments,
//! and unknown keys are preserved byte-for-byte (see [`completion`]).
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
use serde::{Deserialize, Serialize};
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

/// Default subscriber channel buffer for the `Standard` QoS tier (number of
/// in-flight messages). Mirrors the historical hardcoded value.
pub const DEFAULT_STANDARD_BUFFER_SIZE: usize = 128;
/// Default subscriber channel buffer for the `HighThroughput` QoS tier (e.g.
/// sensor-data streams). Mirrors the historical hardcoded value.
pub const DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE: usize = 1024;

/// Default daemon-liveness grace period, in seconds (180 = 3 minutes). A
/// spawned node that sees no daemon heartbeat for this long shuts itself down
/// to avoid lingering as an orphan after an uncatchable daemon death.
pub const DEFAULT_DAEMON_GRACE_SECS: u64 = 180;
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

/// Default cooperative-shutdown grace period, in seconds. How long the daemon
/// (on a clean ctrl+C / `systemctl stop`) and `peppy node stop` wait for a node
/// to run its cleanup hooks before force-killing its process group. 5s gives a
/// robot node room to park actuators and release hardware before it is killed.
pub const DEFAULT_SHUTDOWN_GRACE_SECS: u64 = 5;
/// Minimum accepted cooperative-shutdown grace period, in seconds. At least 1 so
/// the cooperative shutdown signal is actually given a chance to land before the
/// force-kill (a 0 would cancel the in-flight send and amount to an immediate
/// SIGKILL).
pub const MIN_SHUTDOWN_GRACE_SECS: u64 = 1;

/// Worst-case time a node runtime needs to tear down its asyncio event-loop
/// thread after its shutdown hooks finish, before the OS process can exit. A
/// background task may be executing native code (pycapnp serialization, a pyo3
/// future) that must be joined rather than killed mid-call, so this is a real
/// floor the daemon must allow for. Read by `peppylib-py` to bound the loop-join
/// and by the daemon to size its force-kill deadline above the node's real exit
/// cost. Nodes with no asyncio loop (sync-setup Python, Rust) simply finish well
/// inside it.
pub const EVENT_LOOP_JOIN_BUDGET_SECS: u64 = 5;
/// Slack for interpreter finalize / `Drop` after the loop thread joins, before
/// the OS process actually disappears. Added on top of the grace and join
/// windows when the daemon computes how long to wait before force-killing.
pub const RUNTIME_FINALIZE_MARGIN_SECS: u64 = 2;

// The bundled default config, written verbatim on first create so its comments
// survive. Kept inline (not `include_str!` from an asset file) because
// `config-internal` is vendored into every generated node as `src/` only, with
// no sibling `assets/` directory, so an external include would fail to compile
// inside a node build.
//
// The template is split into one snippet per entry so `completion` can splice a
// missing section or field (comments included) into a user's existing file. The
// numeric values are spliced in from the `DEFAULT_*` constants at compile time
// via `concatcp!`, so neither the template nor a spliced snippet can drift from
// the serde `Default` impls the parser falls back to when an entry is absent.

/// Comment block at the top of the bundled config file.
const TEMPLATE_HEADER: &str = r#"// Read once when the peppy daemon starts, so any edit below (mode or buffer
// sizes) takes effect only after you restart the daemon.
"#;

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

/// The full bundled default config, composed from the snippets above.
const DEFAULT_PEPPY_CONFIG_TEMPLATE: &str = const_format::concatcp!(
    TEMPLATE_HEADER,
    "{\n",
    MODE_SECTION_SNIPPET,
    "\n",
    PEER_SECTION_SNIPPET,
    "\n",
    LIFECYCLE_SECTION_SNIPPET,
    "\n",
    RESOURCE_SERVERS_SECTION_SNIPPET,
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

/// Peer-mode tuning knobs. Buffer sizes are the per-QoS subscriber channel
/// capacities used when nodes peer directly (no router relay to absorb bursts).
///
/// `#[serde(default)]` fills any field a partial `peer` block omits from
/// [`PeerConfig::default`], so every per-field default flows from the single
/// `Default` impl below rather than parallel `default = "fn"` helpers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PeerConfig {
    pub standard_buffer_size: usize,
    pub high_throughput_buffer_size: usize,
}

impl Default for PeerConfig {
    fn default() -> Self {
        Self {
            standard_buffer_size: DEFAULT_STANDARD_BUFFER_SIZE,
            high_throughput_buffer_size: DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
        }
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
/// paths (`/cli-config`, `/me`, `/logout`) are appended by the caller; `api`
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

/// The whole `peppy_config.json5` document. Every field is serde-defaulted so a
/// partial or older file still parses; extra unknown keys are tolerated (this is
/// a user-edited file, forward-compat beats strictness here).
///
/// Not `Copy`: `resource_servers` owns heap strings. The daemon reads this once
/// and moves it into the core node, and the CLI clones it field-by-field, so the
/// lost `Copy` costs nothing.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PeppyConfig {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub peer: PeerConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
    #[serde(default)]
    pub resource_servers: ResourceServers,
}

impl PeppyConfig {
    /// Rejects user-tunable numeric fields that serde cannot constrain.
    ///
    /// Buffer sizes feed bounded channel constructors downstream: a 0 capacity
    /// panics `tokio::sync::mpsc::channel` and degrades `flume::bounded` into a
    /// rendezvous channel that stalls every send. A hand-edited 0 must fail loud
    /// at load time rather than crash or wedge a running mesh.
    fn validate(&self) -> Result<()> {
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
/// on-disk file spells out all available knobs.
///
/// Best effort by design: `config` (parsed from `content`) is already complete
/// in memory via the serde defaults, so this only improves the FILE. The result
/// must pass [`completion::verify_completion`] before anything is written; a
/// failure means a splicing bug, in which case the user's file is left
/// untouched and a warning is logged instead of taking the daemon down over a
/// cosmetic rewrite.
fn complete_file_with_defaults(path: &Path, content: &str, config: &PeppyConfig) {
    let Some(completed) = completion::complete_config_content(content) else {
        return;
    };
    if !completion::verify_completion(content, &completed, config) {
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
    if let Err(e) = write_config_file(&target, &completed) {
        tracing::warn!(
            "could not add missing defaults to {PEPPY_CONFIG_FILE}, \
             continuing with the in-memory defaults: {e}"
        );
    }
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
