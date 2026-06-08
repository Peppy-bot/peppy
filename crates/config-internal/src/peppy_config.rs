//! Global daemon configuration read from `~/.peppy/conf/peppy_config.json5`.
//!
//! This is the single user-facing switch for the messaging topology. The daemon
//! reads it ONCE at startup (see `peppy serve`), creating the file from the
//! bundled default template if it is missing, and applies the result to its own
//! core-node session and to every node it spawns. Editing the file takes effect
//! after a daemon restart.
//!
//! Unlike `repositories.json5`, a malformed `peppy_config.json5` fails loud at
//! startup ([`load_or_create`] returns `Err`) instead of falling back to
//! defaults: the mode and buffer sizes determine the whole mesh's routing model
//! and backpressure, so a hand-edited typo must surface immediately rather than
//! silently reverting to peer mode.

use crate::consts::PeppyDirs;
use crate::error::{Error, ParsingError, Result};
use serde::{Deserialize, Serialize};

/// File name of the global daemon config under `~/.peppy/conf`.
pub const PEPPY_CONFIG_FILE: &str = "peppy_config.json5";

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

/// The bundled default config, written verbatim on first create so its comments
/// survive. Kept inline (not `include_str!` from an asset file) because
/// `config-internal` is vendored into every generated node as `src/` only, with
/// no sibling `assets/` directory, so an external include would fail to compile
/// inside a node build.
///
/// The two buffer-size values are spliced in from [`DEFAULT_STANDARD_BUFFER_SIZE`]
/// and [`DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE`] at compile time via `concatcp!`, so
/// the template can never drift from the [`PeerConfig::default`] the parser falls
/// back to when the `peer` block is absent.
const DEFAULT_PEPPY_CONFIG_TEMPLATE: &str = const_format::concatcp!(
    r#"// Read once when the peppy daemon starts, so any edit below (mode or buffer
// sizes) takes effect only after you restart the daemon.
{
  //   "peer"   - Zenoh peer sessions with gossip: nodes form direct
  //              peer-to-peer links and data stops relaying through the router.
  //   "router" - gossip off: all traffic relays through the central zenohd
  //              router.
  // Container nodes in a separate network namespace (Lima on macOS) always use 
  // the router path regardless of this setting.
  mode: "peer",

  // Subscriber channel buffer sizes (number of in-flight messages) per QoS
  // tier, used in peer mode where there is no router relay to buffer between a
  // publisher and a subscriber. Defaults match peppy's built-in behavior; only
  // edit to tune backpressure.
  peer: {
    standard_buffer_size: "#,
    DEFAULT_STANDARD_BUFFER_SIZE,
    r#",
    high_throughput_buffer_size: "#,
    DEFAULT_HIGH_THROUGHPUT_BUFFER_SIZE,
    r#",
  },

  // Node lifecycle safety net. Each spawned node listens for a periodic
  // heartbeat from the daemon. A clean ctrl+C or `systemctl stop` kills the
  // nodes immediately and does NOT wait for this timer. This grace period only
  // governs an *uncatchable* daemon death (crash / OOM / SIGKILL): if the
  // daemon does not return within this many seconds, each node shuts itself
  // down so it does not linger as an orphan. A briefer outage (shorter than
  // this) leaves peer-mode nodes running so they survive a daemon restart.
  lifecycle: {
    daemon_grace_secs: "#,
    DEFAULT_DAEMON_GRACE_SECS,
    r#",
  },
}
"#
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
}

impl Default for LifecycleConfig {
    fn default() -> Self {
        Self {
            daemon_grace_secs: DEFAULT_DAEMON_GRACE_SECS,
        }
    }
}

/// The whole `peppy_config.json5` document. Every field is serde-defaulted so a
/// partial or older file still parses; extra unknown keys are tolerated (this is
/// a user-edited file, forward-compat beats strictness here).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct PeppyConfig {
    #[serde(default)]
    pub mode: Mode,
    #[serde(default)]
    pub peer: PeerConfig,
    #[serde(default)]
    pub lifecycle: LifecycleConfig,
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
        Ok(())
    }
}

/// Reads the global config from `~/.peppy/conf/peppy_config.json5`, creating it
/// from the bundled default template (verbatim, so comments survive) when it
/// does not exist.
///
/// Read ONCE by the daemon at startup. A malformed existing file returns `Err`
/// (fail loud) rather than defaulting, since mode and buffer sizes are
/// load-bearing for the whole mesh. This intentionally differs from
/// `ensure_default_repos`, which only warns on a bad repos file.
pub fn load_or_create(peppy_dirs: &PeppyDirs) -> Result<PeppyConfig> {
    let conf_dir = peppy_dirs.conf_dir();
    std::fs::create_dir_all(&conf_dir)?;
    let path = conf_dir.join(PEPPY_CONFIG_FILE);

    let config: PeppyConfig = if !path.exists() {
        std::fs::write(&path, DEFAULT_PEPPY_CONFIG_TEMPLATE)?;
        // The bundled template is a compile-time invariant; a parse failure here
        // means the shipped asset is broken, not the user's file.
        serde_json5::from_str(DEFAULT_PEPPY_CONFIG_TEMPLATE).map_err(|e| {
            Error::Serialize(format!("bundled default peppy_config is invalid: {e}"))
        })?
    } else {
        let content = std::fs::read_to_string(&path)?;
        serde_json5::from_str(&content).map_err(|e| {
            Error::Parsing(ParsingError::CannotParseConfig(format!(
                "{PEPPY_CONFIG_FILE}: {e}"
            )))
        })?
    };

    // serde parses any numeric field, so a hand-edited 0 buffer size survives
    // the steps above; reject it before it reaches a bounded channel downstream.
    config.validate()?;
    Ok(config)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

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
    }

    #[test]
    fn parses_partial_lifecycle_block() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ lifecycle: { daemon_grace_secs: 600 } }"#,
        )
        .unwrap();

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.lifecycle.daemon_grace_secs, 600);
        // Omitted blocks still fall back to their defaults.
        assert_eq!(cfg.mode, Mode::Peer);
        assert_eq!(cfg.peer, PeerConfig::default());
    }

    #[test]
    fn sub_minimum_grace_fails_loud() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ lifecycle: { daemon_grace_secs: 5 } }"#,
        )
        .unwrap();

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("daemon_grace_secs")),
            "expected a grace-period validation error, got: {err:?}"
        );
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
    }

    #[test]
    fn parses_partial_file_filling_defaults() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join(PEPPY_CONFIG_FILE), r#"{ mode: "router" }"#).unwrap();

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
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(conf_dir.join(PEPPY_CONFIG_FILE), "{}").unwrap();

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
    fn malformed_file_fails_loud() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ mode: "router", peer: { standard_buffer_size: "not a number" } }"#,
        )
        .unwrap();

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(_))),
            "expected a parse error, got: {err:?}"
        );
    }

    #[test]
    fn zero_standard_buffer_size_fails_loud() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ peer: { standard_buffer_size: 0 } }"#,
        )
        .unwrap();

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("standard_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn zero_high_throughput_buffer_size_fails_loud() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ peer: { high_throughput_buffer_size: 0 } }"#,
        )
        .unwrap();

        let err = load_or_create(&peppy_dirs).unwrap_err();
        assert!(
            matches!(err, Error::Parsing(ParsingError::CannotParseConfig(ref m)) if m.contains("high_throughput_buffer_size")),
            "expected a buffer-size validation error, got: {err:?}"
        );
    }

    #[test]
    fn accepts_minimal_nonzero_buffer_sizes() {
        let tmp = tempdir().unwrap();
        let peppy_dirs = PeppyDirs::new(tmp.path());
        let conf_dir = peppy_dirs.conf_dir();
        std::fs::create_dir_all(&conf_dir).unwrap();
        std::fs::write(
            conf_dir.join(PEPPY_CONFIG_FILE),
            r#"{ peer: { standard_buffer_size: 1, high_throughput_buffer_size: 1 } }"#,
        )
        .unwrap();

        let cfg = load_or_create(&peppy_dirs).unwrap();
        assert_eq!(cfg.peer.standard_buffer_size, 1);
        assert_eq!(cfg.peer.high_throughput_buffer_size, 1);
    }
}
