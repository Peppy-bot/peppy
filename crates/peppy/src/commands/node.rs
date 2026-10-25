mod add;
mod builder;
mod call;
mod describe;
mod echo;
mod env;
mod info;
mod init;
mod interfaces;
mod json_lines;
mod remove;
pub mod request;
mod run;
pub(crate) mod source;
mod stop;
mod sync;
#[cfg(test)]
mod test_fixtures;
mod types;

use config::node::RestartPolicy;
use config::runtime::Name;
use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{ArgGroup, Subcommand};
use config::node::Toolchain;
use daemon_config::launcher::{LinkTargets, LinkValue, Selection, VacantReason};
use tracing::info;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};

pub use add::{AddNodeParams, add_node};
pub use builder::{BuildOptions, build_node_async};
pub use call::{CallKind, CallParams, call_writing};
pub use describe::{DescribeParams, describe_rendered};
pub use echo::EchoParams;
#[cfg(feature = "test-support")]
pub use echo::echo_messages;
#[cfg(feature = "test-support")]
pub use info::node_info_rendered;
pub use init::NodeInitBuilder;
#[cfg(feature = "test-support")]
pub use run::run_instance_async;
pub use types::NodeName;
// `stack launch` and `stack join` reach it through the `node::` path; nothing
// outside the crate uses it.
pub(crate) use env::caller_env_overrides;

use core_node_api::encoding::DEFAULT_IDLE_TIMEOUT_SECS;
use stack_goal::DEFAULT_BUILD_IDLE_TIMEOUT_SECS;
/// Default absolute max timeout in seconds (safety net).
pub(crate) const DEFAULT_MAX_TIMEOUT_SECS: u64 = 3600;
/// Messaging headroom the CLI waits beyond a daemon's own bound on a stop or
/// a removal, so it does not report a timeout for one that succeeded.
const DAEMON_MESSAGING_MARGIN: std::time::Duration = std::time::Duration::from_secs(5);

/// How long the CLI waits for a daemon request whose own bound is `bound`.
fn daemon_wait(bound: std::time::Duration) -> std::time::Duration {
    bound + DAEMON_MESSAGING_MARGIN
}

/// Warns that `instance_id` ignored its cooperative shutdown within the grace
/// period and was force-killed, so it is not mistaken for a graceful exit.
fn warn_force_killed(instance_id: &str) {
    tracing::warn!(
        "Node instance '{instance_id}' did not shut down gracefully within the grace period and \
         was force-killed"
    );
}

/// The instances `node_name:node_tag` tracks on the daemon, each with its
/// state; none when the daemon has no such node.
async fn fetch_tracked_instances(
    conn: &crate::context::DaemonConnection<'_>,
    node_name: &str,
    node_tag: &str,
    timeout: std::time::Duration,
) -> peppylib::PeppyResult<Vec<(String, core_node_api::InstanceState)>> {
    let response = peppylib::core_node::transport::poll(
        &core_node_api::encoding::NodeInfoRequest::new(node_name, node_tag),
        conn.messenger,
        &conn.core_node_name,
        super::CALLER_INSTANCE_ID,
        &conn.target_core_node,
        timeout,
    )
    .await?;
    Ok(match response {
        core_node_api::encoding::NodeInfoResponse::NotInStack => Vec::new(),
        core_node_api::encoding::NodeInfoResponse::Found(info) => {
            tracked_instances(&info.instances)
        }
    })
}

/// Each of `instances` with its state, whatever the state.
fn tracked_instances(
    instances: &[core_node_api::encoding::NodeInstanceInfo],
) -> Vec<(String, core_node_api::InstanceState)> {
    instances
        .iter()
        .map(|inst| (inst.instance_id.clone(), inst.state))
        .collect()
}

/// Idle + absolute-max timeout pair of the watchdog of a command that follows
/// a daemon goal.
pub struct TimeoutConfig {
    pub idle_secs: u64,
    pub max_secs: u64,
    /// Whether the command takes `--idle-timeout` and `--max-timeout`, which
    /// a timeout then names as the way to raise its budget.
    pub flags: bool,
}

/// Parses a node_name:tag argument string into a tuple
fn parse_node_ref(s: &str) -> Result<(String, String), String> {
    let (node_name, tag) = s.split_once(':').ok_or_else(|| {
        format!(
            "invalid node reference '{}': expected node_name:tag format",
            s
        )
    })?;
    let node_name = node_name.trim().to_string();
    let tag = tag.trim().to_string();
    if node_name.is_empty() {
        return Err(format!(
            "invalid node reference '{}': node_name cannot be empty",
            s
        ));
    }
    if tag.is_empty() {
        return Err(format!(
            "invalid node reference '{}': tag cannot be empty",
            s
        ));
    }
    Ok((node_name, tag))
}

/// Parses a key=value argument string into a tuple
fn parse_key_value_arg(s: &str) -> Result<(String, String), String> {
    let (key, value) = s
        .split_once('=')
        .ok_or_else(|| format!("invalid argument format '{}': expected key=value", s))?;
    let key = key.trim().to_string();
    let value = value.trim().to_string();
    if key.is_empty() {
        return Err(format!("invalid argument '{}': key cannot be empty", s));
    }
    Ok((key, value))
}

/// Parses a `KEY@TARGET` `--link` argument, the unified flag for every kind of
/// launcher link. KEY is a `link_id` from this node's `depends_on` (a producer
/// slot under `{nodes,contracts}`, or a participant/observer slot under
/// `pairings`); TARGET is `<instance>` (a producer instance to bind, or a peer
/// / source instance to pair with / observe) with an optional `/<link_id>`
/// suffix that pins a specific slot on the target (used to disambiguate a
/// pairing peer or an observed source that plays the role through more than one
/// slot). Splits on the first `@`; both halves must be non-empty. KEY and the
/// optional target `/<link_id>` are validated as wire segments via the strict
/// `pmi::Segment::try_from`, which rejects empty, `/`, `@`, `*`, `**`, and the
/// reserved sentinel `_`. The instance part is left free-form for the
/// daemon-side validators to resolve against the stack. The slot KIND is not
/// known here (it needs the manifest), so classification is deferred to
/// validation.
fn parse_link_kv(raw: &str) -> Result<(String, String), String> {
    let (key, value) = parse_key_at_target(raw, "--link", "KEY@TARGET")?;
    let (key, value) = (key.as_str(), value.as_str());
    pmi::Segment::try_from(key).map_err(|e| format!("invalid --link KEY '{key}': {e}"))?;
    let (instance, link) = daemon_config::launcher::split_link_target(value);
    if instance.is_empty() {
        return Err(format!(
            "invalid --link value '{raw}': target instance cannot be empty"
        ));
    }
    if let Some(link) = link {
        pmi::Segment::try_from(link)
            .map_err(|e| format!("invalid --link target slot '{link}': {e}"))?;
    }
    Ok((key.to_string(), value.to_string()))
}

/// Splits the `KEY@TARGET` grammar shared by `node run --link` and
/// `stack launch --place`.
///
/// Only the GRAMMAR is shared. The two flags mean different things (`--link`
/// wires a slot to a producer, `--place` wires a placeholder to a machine) and
/// each validates its own halves; sharing the split keeps them from drifting on
/// whitespace, empty halves, or which `@` separates them.
pub(crate) fn parse_key_at_target(
    raw: &str,
    flag: &str,
    shape: &str,
) -> Result<(String, String), String> {
    let (key, value) = raw
        .split_once('@')
        .ok_or_else(|| format!("invalid {flag} value '{raw}': expected {shape}"))?;
    let key = key.trim();
    let value = value.trim();
    // Name each half with the caller's own vocabulary (`KEY`/`TARGET` for
    // `--link`, `NAME`/`CORE_NODE` for `--place`) so a shared parser
    // never makes one flag speak in the other's terms.
    let (key_label, target_label) = shape.split_once('@').unwrap_or((shape, shape));
    if key.is_empty() {
        return Err(format!(
            "invalid {flag} value '{raw}': {key_label} cannot be empty"
        ));
    }
    if value.is_empty() {
        return Err(format!(
            "invalid {flag} value '{raw}': {target_label} cannot be empty"
        ));
    }
    Ok((key.to_owned(), value.to_owned()))
}

/// Parses a `--vacant-link SLOT=<why>` argument: a slot the node's manifest
/// declares `cardinality: "zero_or_one"`, left unresolved at launch on purpose,
/// plus the reason the operator gives for it. Splits on the first `=` via
/// [`parse_key_value_arg`], so a reason may itself contain `=`; SLOT is
/// validated as a wire segment and the reason is parsed into a
/// [`VacantReason`] here, once, at the flag boundary.
fn parse_vacant_link(raw: &str) -> Result<(String, VacantReason), String> {
    let (link_id, reason) = parse_key_value_arg(raw)
        .map_err(|_| format!("invalid --vacant-link value '{raw}': expected SLOT=<why>"))?;
    pmi::Segment::try_from(link_id.as_str())
        .map_err(|e| format!("invalid --vacant-link SLOT '{link_id}': {e}"))?;
    let reason =
        VacantReason::new(&reason).map_err(|e| format!("invalid --vacant-link '{raw}': {e}"))?;
    Ok((link_id, reason))
}

/// The `--link` and `--vacant-link` flags of one run, merged into the single
/// `links` map every validator downstream reads: the same map a launch file's
/// `links:` parses into, built here at the flag boundary so nothing below has
/// to join two containers to learn a slot's fate.
///
/// Repeating a `--link KEY` mirrors the launcher's array form: it accumulates
/// the slot's bound set, and `validate_bindings` checks the count against the
/// slot's declared cardinality (more than one target on a `cardinality: "one"`
/// slot fails there). Repeating the exact same `KEY@TARGET` pair is a hard
/// error, rejected by `LinkTargets` like every other path that builds a bound
/// set, and a slot given both flags is rejected here: one key, one value.
fn merge_link_flags(
    links: &[(String, String)],
    vacant_links: &[(String, VacantReason)],
) -> Result<BTreeMap<String, LinkValue>, String> {
    let mut accumulated: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (key, target) in links {
        accumulated
            .entry(key.clone())
            .or_default()
            .push(target.clone());
    }
    let mut merged: BTreeMap<String, LinkValue> = BTreeMap::new();
    for (key, targets) in accumulated {
        let targets = LinkTargets::new(targets)
            .map_err(|err| format!("`--link {key}@{target}`: {err}", target = err.target))?;
        merged.insert(key, LinkValue::Bound(Selection::Flags(targets)));
    }
    for (link_id, reason) in vacant_links {
        let clash = match merged.get(link_id) {
            None => None,
            Some(LinkValue::Bound(_)) => Some(
                "is both linked and declared vacant: a slot is wired to a target or \
                 deliberately left empty, not both",
            ),
            Some(LinkValue::Vacant(_)) => {
                Some("is declared vacant twice: one slot takes one reason")
            }
        };
        if let Some(clash) = clash {
            return Err(format!("slot `{link_id}` {clash}"));
        }
        merged.insert(link_id.clone(), LinkValue::Vacant(reason.clone()));
    }
    Ok(merged)
}

#[derive(Subcommand)]
pub enum NodeCommands {
    /// Create a new peppy node
    #[command(visible_alias = "create")]
    Init {
        /// Name of the node directory to create
        node_name: NodeName,
        /// Build toolchain to use
        #[arg(long, default_value_t)]
        toolchain: Toolchain,
        /// Optional: target directory (defaults to current directory)
        #[arg(long)]
        to_dir: Option<PathBuf>,
        /// Generate container build support (Apptainer definition file)
        #[arg(long = "container")]
        with_container: bool,
    },
    /// Add a node to the node stack based on its peppy.json5 file
    Add {
        /// Source location of a node (directory containing peppy.json5).
        /// Defaults to the current directory if not specified.
        ///
        /// Supported formats:
        /// - Local path: `/path/to/node` or `./relative/path`
        /// - Git URL: `https://github.com/org/repo.git/subpath`
        /// - Git URL with ref: `https://github.com/org/repo.git/subpath --ref tag-or-branch`
        /// - HTTP archive: `https://example.com/node.tar.zst`
        source: Option<String>,
        /// Git ref (tag/branch/commit) to checkout before reading `subpath` (git sources only).
        #[arg(long = "ref")]
        git_ref: Option<String>,
        /// If set, runs `peppy node sync` on the source *before* adding. Forces
        /// peppygen interface code to be regenerated from the current
        /// `peppy.json5`, so the snapshot taken by `node add` is up-to-date.
        ///
        /// Only valid for local filesystem sources; remote (git/http)
        /// sources are synced server-side when the daemon fetches them.
        ///
        /// This flag is NOT implied by `--build` or `--run`; it's a
        /// prerequisite step, not a post-step. Combines with them via short
        /// flag bundling: `-sb` = sync + build, `-sr` = sync + build + run.
        #[arg(short = 's', long)]
        sync: bool,
        /// If set, will trigger a `node build` immediately after adding the node
        #[arg(short = 'b', long)]
        build: bool,
        /// If set, will attempt to spawn an instance directly after adding the
        /// node to the node stack. Implies `--build`.
        ///
        /// When the node requires runtime arguments, pass them as trailing
        /// key=value pairs after the source:
        /// `peppy node add ./my-camera --run resolution=1280x720 frequency=30`
        ///
        /// Combines with `--sync` via `-sr` (sync + build + run).
        #[arg(short = 'r', long)]
        run: bool,
        /// Runtime arguments as key=value pairs (e.g., resolution=1280x720 frequency=30).
        /// These are passed to the node via PEPPY_RUNTIME_CONFIG and only make
        /// sense when `--run` is set; `requires = "run"` so a bare
        /// `peppy node add . frequency=30` errors at parse time instead of
        /// silently ignoring the argument.
        #[arg(value_parser = parse_key_value_arg, requires = "run")]
        args: Vec<(String, String)>,
        /// Optional: specify a deterministic instance ID for the spawn.
        /// Only meaningful with `--run`; gated with `requires = "run"`.
        #[arg(long, hide = true, requires = "run")]
        instance_id: Option<String>,
        /// The clock domain this instance reads: `wall` (the default), a
        /// domain name, or `name@core_node` when one name runs on several
        /// machines. `peppy clock list` shows what is running. Omitted, the
        /// instance reads wall time whatever else is running.
        #[arg(long, value_name = "DOMAIN", value_parser = run::parse_clock_reference,
              conflicts_with = "publish_clock", requires = "run")]
        clock: Option<run::ClockReference>,
        /// Declare a simulated clock domain and make this instance its
        /// publisher. The declaration assigns the instance its domain, so it
        /// takes no `--clock` of its own.
        #[arg(long, value_name = "DOMAIN", value_parser = run::parse_clock_domain_name, requires = "run")]
        publish_clock: Option<Name>,
        /// What the daemon does when the process exits on its own or stops
        /// answering health checks: `never` leaves an exited instance
        /// `finished` or `failed` and a silent one `running`; `on_failure`
        /// starts it again after a failed exit, and kills and restarts it
        /// once it has answered no health check for a minute, except while
        /// two or more instances other than the core node's own run on the
        /// daemon and none is healthy; `always` also starts it again after a
        /// clean exit. Omitted, the node's `framework.restart` decides, then
        /// `on_failure`.
        #[arg(long, value_name = "POLICY", value_parser = run::parse_restart_policy, requires = "run")]
        restart: Option<RestartPolicy>,
        /// Restart attempts in a row, failed starts included, before the next
        /// exit or failed start leaves the instance failed. An exit after a run
        /// of two minutes or more starts the count over; a run the health
        /// monitor ended counts only up to its last answered health check.
        /// Omitted, no limit.
        #[arg(long, value_name = "N", requires = "restart", allow_negative_numbers = true, value_parser = run::parse_max_restarts)]
        max_restarts: Option<NonZeroU32>,
        /// Link a `link_id` from this node's `depends_on` to a target:
        /// `KEY@TARGET`. One flag for every link kind: a producer binding
        /// (TARGET is a producer `instance_id`), a pairing (TARGET is a peer
        /// `instance_id[/peer_link_id]`), or an observer source (TARGET is a
        /// source `instance_id[/source_link_id]`). Repeatable across slots
        /// (`--link a@p1 --link b@p2`) or comma-separated (`--link a@p1,b@p2`);
        /// repeating a KEY accumulates a multi-cardinality slot's set in flag
        /// order, on every link kind, and is rejected on a scalar `one` /
        /// `zero_or_one` slot. Only valid alongside `--run`: without a
        /// chained run there is no instance to apply the links to, so
        /// `requires = "run"` rejects the combination at parse time. Validation
        /// is shared with `peppy node run`; see `validate_and_run_instance`.
        #[arg(
            long = "link",
            value_delimiter = ',',
            value_parser = parse_link_kv,
            action = clap::ArgAction::Append,
            requires = "run",
        )]
        links: Vec<(String, String)>,
        /// Explicitly start with a slot unresolved, saying why: `SLOT=<why>`.
        /// Valid on a slot the node's manifest declares
        /// `cardinality: "zero_or_one"`. Repeatable. The reason is free prose (no
        /// comma splitting) and travels with the instance, so an operator
        /// reading the running stack sees the same sentence. Only valid
        /// alongside `--run`.
        #[arg(
            long = "vacant-link",
            value_name = "SLOT=<why>",
            value_parser = parse_vacant_link,
            action = clap::ArgAction::Append,
            requires = "run",
        )]
        vacant_links: Vec<(String, VacantReason)>,
        /// Idle timeout in seconds; resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// When set, skip the confirmation prompt, and stop and clear the node's tracked instances before overwriting it. While one of them is starting, the add is refused and stops nothing.
        ///
        /// When chaining a build (`--build` / `--run`), the flag is also forwarded
        /// to the build step: any in-progress build of this node is cancelled and
        /// superseded, instead of the chained build being rejected with
        /// "action already in progress".
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Build a node previously added to the node stack
    Build {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: (String, String),
        /// Idle timeout in seconds; resets on build output, bytes written to disk, or CPU time the build burns
        #[arg(long, default_value_t = DEFAULT_BUILD_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// Cancel any in-progress build for this node and start a new one
        #[arg(short = 'f', long)]
        force: bool,
        /// Build from the staged sources even when a cached artifact built
        /// from byte-identical sources exists. Independent of `--force`,
        /// which only cancels an in-flight build of this node.
        #[arg(long)]
        rebuild: bool,
    },
    /// Regenerate the node's interface code (peppygen) based on peppy.json5
    Sync {
        /// Optional path to the node directory. Defaults to the current directory.
        path: Option<PathBuf>,
        /// Resolve dependencies missing from the node stack by fetching them
        /// from the configured repositories (`~/.peppy/cache/nodes.json5`).
        /// Stack lookups still win; the repo cache is consulted only as a
        /// fallback. Output also reports which deps came from the stack vs.
        /// the repositories.
        #[arg(short = 'r', long = "include-repositories")]
        include_repositories: bool,
    },
    /// Runs an instance from a node added to the node stack
    ///
    /// Usage: `peppy node run <node_name>:<tag>` or `peppy node run --node-name <name> --tag <tag>`
    #[command(group(ArgGroup::new("node_source").required(true).args(["node_ref", "node_name"])))]
    Run {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: Option<(String, String)>,
        /// Name of the node to spawn
        #[arg(long, requires = "tag")]
        node_name: Option<String>, // Finds the `NodeConfig` in the node stack that matches this name
        /// Tag of the node
        #[arg(long, requires = "node_name")]
        tag: Option<String>,
        /// Runtime arguments as key=value pairs (e.g., resolution=1280x720 frequency=30)
        /// These are passed to the node via PEPPY_RUNTIME_CONFIG
        #[arg(value_parser = parse_key_value_arg)]
        args: Vec<(String, String)>,
        /// Optional: specify a deterministic instance ID
        #[arg(short = 'i', long)]
        instance_id: Option<String>,
        /// The clock domain this instance reads: `wall` (the default), a
        /// domain name, or `name@core_node` when one name runs on several
        /// machines. `peppy clock list` shows what is running. Omitted, the
        /// instance reads wall time whatever else is running.
        #[arg(long, value_name = "DOMAIN", value_parser = run::parse_clock_reference,
              conflicts_with = "publish_clock")]
        clock: Option<run::ClockReference>,
        /// Declare a simulated clock domain and make this instance its
        /// publisher. The declaration assigns the instance its domain, so it
        /// takes no `--clock` of its own.
        #[arg(long, value_name = "DOMAIN", value_parser = run::parse_clock_domain_name)]
        publish_clock: Option<Name>,
        /// What the daemon does when the process exits on its own or stops
        /// answering health checks: `never` leaves an exited instance
        /// `finished` or `failed` and a silent one `running`; `on_failure`
        /// starts it again after a failed exit, and kills and restarts it
        /// once it has answered no health check for a minute, except while
        /// two or more instances other than the core node's own run on the
        /// daemon and none is healthy; `always` also starts it again after a
        /// clean exit. Omitted, the node's `framework.restart` decides, then
        /// `on_failure`.
        #[arg(long, value_name = "POLICY", value_parser = run::parse_restart_policy)]
        restart: Option<RestartPolicy>,
        /// Restart attempts in a row, failed starts included, before the next
        /// exit or failed start leaves the instance failed. An exit after a run
        /// of two minutes or more starts the count over; a run the health
        /// monitor ended counts only up to its last answered health check.
        /// Omitted, no limit.
        #[arg(long, value_name = "N", requires = "restart", allow_negative_numbers = true, value_parser = run::parse_max_restarts)]
        max_restarts: Option<NonZeroU32>,
        /// Link a `link_id` from this node's `depends_on` to a target:
        /// `KEY@TARGET`. One flag for every link kind. For a producer binding,
        /// TARGET is the producer's `instance_id` (see `peppy stack list --json`),
        /// repeatable across slots (`--link a@p1 --link b@p2`) or
        /// comma-separated (`--link a@p1,b@p2`); repeating a KEY accumulates a
        /// multi-cardinality slot's set in flag order and is rejected on a
        /// scalar `one` / `zero_or_one` slot. For a pairing or observer link, TARGET is a peer/source
        /// `instance_id[/link_id]`, where the `/link_id` suffix disambiguates
        /// when the target plays the role through several slots. A slot left
        /// unlinked aborts the run unless it is a `zero_or_more` producer or
        /// observer slot.
        #[arg(
            long = "link",
            value_delimiter = ',',
            value_parser = parse_link_kv,
            action = clap::ArgAction::Append,
        )]
        links: Vec<(String, String)>,
        /// Explicitly start with a slot unresolved, saying why: `SLOT=<why>`.
        /// Valid on a slot the node's manifest declares
        /// `cardinality: "zero_or_one"`. Repeatable. The reason is free prose (no
        /// comma splitting) and travels with the instance, so an operator
        /// reading the running stack sees the same sentence. The instance boots
        /// with the slot silent (no wire traffic) until a later start resolves
        /// it: for a pairing, via `--link <peer_slot>@<this_instance>/<SLOT>`
        /// from the peer.
        #[arg(
            long = "vacant-link",
            value_name = "SLOT=<why>",
            value_parser = parse_vacant_link,
            action = clap::ArgAction::Append,
        )]
        vacant_links: Vec<(String, VacantReason)>,
        /// Idle timeout in seconds; resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// If set, build the node first if it has not already been built.
        /// No-op (with a message) when the node is already built.
        #[arg(short = 'b', long)]
        build: bool,
    },
    /// Stop a node instance, or clear one that is restarting, finished or
    /// failed, freeing its id
    Stop {
        /// Instance ID of the node to stop
        instance_id: String,
    },
    /// Remove a node from the node stack
    Remove {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: (String, String),
        /// Stop the node's running instances and clear its restarting, finished and failed ones before removing it. Without this flag the command asks first when the node tracks instances. An instance starting when the removal reaches it, or running a process that reached `running` after the removal began, keeps the node; once none is starting, run the removal again. `peppy node stop <instance-id> --core-node <daemon>` ends a restart's start, and a first start ends on its own
        #[arg(long)]
        stop_instances: bool,
        /// Skip the question and remove as `--stop-instances` does
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Return the information about a node currently in the node stack
    ///
    /// Takes a reference to a node that has already been added via
    /// `peppy node add`. If the node is not in the stack, the command exits
    /// with an error; inspecting sources on disk is the job of `node add`,
    /// not `node info`.
    Info {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: (String, String),
    },
    /// Call a service or an action of a running instance, and print how the
    /// call ends as JSON lines
    ///
    /// The command reads the pinned documents of the instance from the
    /// daemon and sends the call itself. It prints a `{"feedback": ...}` line
    /// for each feedback message of a goal, then one line with the `outcome`
    /// of the call. It exits with 0 for a response or a completed goal, 3
    /// for a refusal, 4 for a failure, 5 for a cancel, 6 for an abandon, 7
    /// for an expiry, and 1 for an error of the command. A Ctrl-C during a
    /// goal sends a cancel and waits for the end of the goal; a second Ctrl-C
    /// exits at once with 130.
    Call {
        /// Id of the instance, on the daemon that `--core-node` targets
        instance_id: String,
        /// The member: `<slot>.<member>` for a member of a contract that the
        /// node implements through the slot `<slot>` of its manifest, the
        /// name alone for a member of the node's own, or for the one service
        /// or action of the instance that has it
        member: String,
        /// The kind of the member, for a service and an action of one name
        #[arg(long, value_enum)]
        kind: Option<call::CallKind>,
        /// A JSON5 file that holds the whole request or goal
        #[arg(long, value_name = "FILE")]
        args: Option<PathBuf>,
        /// One field, by its dotted path: `<field>=<value>`. A field whose
        /// JSON value is a string (a string, bytes in base64, a time in RFC
        /// 3339, a 64-bit integer) takes the value as written; any other
        /// field takes it as JSON5
        #[arg(long = "set", value_name = "FIELD=VALUE")]
        settings: Vec<request::FieldSetting>,
        /// One field from a file: `<field>=@<path>`. A string field takes the
        /// text of the file and a bytes field its bytes. A directory gives a
        /// list of `{ path, content }`, one for each file, with paths relative
        /// to the directory
        #[arg(long = "file", value_name = "FIELD=@PATH")]
        files: Vec<request::FieldFile>,
    },
    /// Print the messages of a topic of a running instance, one JSON object
    /// per line, until Ctrl-C
    Echo {
        /// Id of the instance, on the daemon that `--core-node` targets
        instance_id: String,
        /// The topic: `<slot>.<topic>` for a topic that the node emits
        /// through the slot `<slot>` of its manifest (a contract that it
        /// implements, or a pairing that it plays a role of), the name alone
        /// for a topic of the node's own, or for the one topic of the
        /// instance that has it
        topic: String,
    },
    /// Print the interfaces of a running instance with the message formats
    /// of its pinned manifest
    Describe {
        /// Id of the instance, on the daemon that `--core-node` targets
        instance_id: String,
        /// Print the interfaces as one JSON document
        #[arg(long)]
        json: bool,
    },
}

impl NodeCommands {
    /// Whether the command prints data that a program reads on standard
    /// output, so its log lines go to standard error.
    pub fn prints_data(&self) -> bool {
        matches!(
            self,
            NodeCommands::Call { .. } | NodeCommands::Echo { .. } | NodeCommands::Describe { .. }
        )
    }
}

pub struct NodeCommand {
    pub command: NodeCommands,
}

impl Command for NodeCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            NodeCommands::Init {
                to_dir,
                node_name,
                toolchain,
                with_container,
            } => {
                let mut node_init_builder =
                    NodeInitBuilder::new(ctx, node_name, toolchain, with_container);

                if let Some(dir) = to_dir {
                    node_init_builder = node_init_builder.with_dir(dir);
                }

                node_init_builder.build()
            }
            NodeCommands::Add {
                source,
                git_ref,
                sync,
                build,
                run,
                args,
                instance_id,
                clock,
                publish_clock,
                restart,
                max_restarts,
                links,
                vacant_links,
                idle_timeout,
                max_timeout,
                force,
            } => {
                // `requires = "run"` on `args`, `instance_id`, `links`, and
                // `vacant_links` means we can only land here with
                // `run == false` when all of them are empty; the run-only
                // fields therefore have a single legal home: inside
                // `Some(RunAfterAddOptions)`.
                let run_options = if run {
                    Some(add::RunAfterAddOptions {
                        args,
                        instance_id,
                        clock: run::ClockChoice {
                            clock,
                            publish_clock,
                        },
                        restart: run::restart_request(restart, max_restarts)?,
                        links: merge_link_flags(&links, &vacant_links)
                            .map_err(CommandError::ExecutionFailed)?,
                    })
                } else {
                    None
                };
                let timeouts = TimeoutConfig {
                    idle_secs: idle_timeout,
                    max_secs: max_timeout,
                    flags: true,
                };
                let source = source.unwrap_or_else(|| ".".to_string());
                // `--run` implies `--build`: you can't run an instance of
                // an unbuilt node. `--sync` is independent (prerequisite, not
                // a post-step) and is not implied by either.
                let chain_build = build || run;
                add::add_node(
                    ctx,
                    add::AddNodeParams {
                        source,
                        git_ref,
                        run_options,
                        timeouts,
                        force,
                        confirm_reader: None,
                        sync,
                        chain_build,
                    },
                )
            }
            NodeCommands::Build {
                node_ref: (node_name, node_tag),
                idle_timeout,
                max_timeout,
                force,
                rebuild,
            } => {
                let timeouts = TimeoutConfig {
                    idle_secs: idle_timeout,
                    max_secs: max_timeout,
                    flags: true,
                };
                builder::build_node(
                    ctx,
                    builder::BuildNodeParams {
                        node_name,
                        node_tag,
                        timeouts,
                        options: BuildOptions { force, rebuild },
                    },
                )
            }
            NodeCommands::Sync {
                path,
                include_repositories,
            } => {
                info!("Syncing node interfaces...");
                sync::sync_node(ctx, path, include_repositories)
            }
            NodeCommands::Run {
                node_ref,
                node_name,
                tag,
                args,
                instance_id,
                clock,
                publish_clock,
                restart,
                max_restarts,
                links,
                vacant_links,
                idle_timeout,
                max_timeout,
                build,
            } => {
                let (node_name, tag) = node_ref
                    .or_else(|| node_name.zip(tag))
                    .expect("either node_ref or node_name+tag must be provided");
                info!("Running node {}:{}...", node_name, tag);
                let timeouts = TimeoutConfig {
                    idle_secs: idle_timeout,
                    max_secs: max_timeout,
                    flags: true,
                };
                run::run_node(
                    ctx,
                    run::RunNodeParams {
                        node_name,
                        tag,
                        args,
                        instance_id,
                        clock: run::ClockChoice {
                            clock,
                            publish_clock,
                        },
                        restart: run::restart_request(restart, max_restarts)?,
                        links: merge_link_flags(&links, &vacant_links)
                            .map_err(CommandError::ExecutionFailed)?,
                        timeouts,
                        build,
                    },
                )
            }
            NodeCommands::Stop { instance_id } => {
                info!("Stopping node instance {}...", instance_id);
                stop::stop_node(ctx, instance_id)
            }
            NodeCommands::Remove {
                node_ref: (node_name, tag),
                stop_instances,
                force,
            } => {
                info!("Remove node {}:{}...", node_name, tag);
                remove::remove_node(ctx, node_name, tag, stop_instances, force)
            }
            NodeCommands::Info {
                node_ref: (node_name, node_tag),
            } => {
                info!("Getting node info for {}:{}...", node_name, node_tag);
                info::node_info(ctx, node_name, node_tag)
            }
            NodeCommands::Call {
                instance_id,
                member,
                kind,
                args,
                settings,
                files,
            } => call::call(
                ctx,
                call::CallParams {
                    instance_id,
                    member,
                    kind,
                    message: request::MessageArguments {
                        args,
                        settings,
                        files,
                    },
                },
            ),
            NodeCommands::Echo { instance_id, topic } => {
                echo::echo(ctx, echo::EchoParams { instance_id, topic })
            }
            NodeCommands::Describe { instance_id, json } => {
                describe::describe(ctx, describe::DescribeParams { instance_id, json })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn vacant_reason(reason: &str) -> VacantReason {
        VacantReason::new(reason).expect("test reasons say something")
    }

    /// Every tracked instance keeps its state, a starting one included,
    /// which the overwrite and removal questions name.
    #[test]
    fn tracked_instances_keep_every_state() {
        let instance = |instance_id: &str, state| core_node_api::encoding::NodeInstanceInfo {
            clock: Default::default(),
            instance_id: instance_id.to_owned(),
            state,
            healthy: true,
            slot_bindings: std::collections::BTreeMap::new(),
            pairing_slots: std::collections::BTreeMap::new(),
            endpoints: Vec::new(),
            restart_status: core_node_api::RestartStatus::default(),
        };
        let states = [
            core_node_api::InstanceState::Starting,
            core_node_api::InstanceState::Running,
            core_node_api::InstanceState::Restarting,
            core_node_api::InstanceState::Finished,
            core_node_api::InstanceState::Failed,
        ];
        let instances: Vec<_> = states
            .iter()
            .map(|state| instance(state.as_str(), *state))
            .collect();
        assert_eq!(
            tracked_instances(&instances),
            states
                .iter()
                .map(|state| (state.as_str().to_owned(), *state))
                .collect::<Vec<_>>()
        );
    }

    /// Tiny clap harness that wraps `NodeCommands` so we can exercise argument
    /// parsing for `peppy node add` in isolation.
    #[derive(Parser)]
    #[command(name = "peppy")]
    struct TestCli {
        #[command(subcommand)]
        command: NodeCommands,
    }

    fn try_parse(subcommand: &str, args: &[&str]) -> Result<TestCli, clap::Error> {
        TestCli::try_parse_from(
            ["peppy", subcommand]
                .into_iter()
                .chain(args.iter().copied()),
        )
    }

    fn parse(subcommand: &str, args: &[&str]) -> NodeCommands {
        try_parse(subcommand, args).expect("should parse").command
    }

    fn parse_add(args: &[&str]) -> (bool, bool, bool) {
        match parse("add", args) {
            NodeCommands::Add {
                sync, build, run, ..
            } => (sync, build, run),
            _ => panic!("expected Add variant"),
        }
    }

    fn parse_run(args: &[&str]) -> bool {
        match parse("run", args) {
            NodeCommands::Run { build, .. } => build,
            _ => panic!("expected Run variant"),
        }
    }

    fn parse_run_instance_id(args: &[&str]) -> Option<String> {
        match parse("run", args) {
            NodeCommands::Run { instance_id, .. } => instance_id,
            _ => panic!("expected Run variant"),
        }
    }

    fn parse_run_links(args: &[&str]) -> Vec<(String, String)> {
        match parse("run", args) {
            NodeCommands::Run { links, .. } => links,
            _ => panic!("expected Run variant"),
        }
    }

    fn parse_add_links(args: &[&str]) -> Vec<(String, String)> {
        match parse("add", args) {
            NodeCommands::Add { links, .. } => links,
            _ => panic!("expected Add variant"),
        }
    }

    fn parse_subcommand_force(subcommand: &str, args: &[&str]) -> bool {
        match parse(subcommand, args) {
            NodeCommands::Add { force, .. }
            | NodeCommands::Build { force, .. }
            | NodeCommands::Remove { force, .. } => force,
            _ => panic!("expected Add/Build/Remove variant"),
        }
    }

    #[test]
    fn add_sr_short_bundle_sets_sync_and_run() {
        let (sync, build, run) = parse_add(&[".", "-sr"]);
        assert!(sync, "-sr should set sync");
        assert!(
            !build,
            "-sr should NOT set build directly (run implies it at runtime)"
        );
        assert!(run, "-sr should set run");
    }

    #[test]
    fn add_sb_short_bundle_sets_sync_and_build() {
        let (sync, build, run) = parse_add(&[".", "-sb"]);
        assert!(sync, "-sb should set sync");
        assert!(build, "-sb should set build");
        assert!(!run, "-sb should NOT set run");
    }

    #[test]
    fn add_s_short_sets_sync_only() {
        let (sync, build, run) = parse_add(&[".", "-s"]);
        assert!(sync, "-s should set sync");
        assert!(!build);
        assert!(!run);
    }

    #[test]
    fn add_long_sync_flag_parses() {
        let (sync, build, run) = parse_add(&[".", "--sync", "--build"]);
        assert!(sync);
        assert!(build);
        assert!(!run);
    }

    #[test]
    fn add_without_sync_flag_defaults_to_false() {
        let (sync, build, run) = parse_add(&[".", "--build"]);
        assert!(!sync, "sync should default to false");
        assert!(build);
        assert!(!run);
    }

    #[test]
    fn run_b_short_flag_sets_build() {
        assert!(parse_run(&["foo:v1", "-b"]), "-b should set build on run");
    }

    #[test]
    fn run_long_build_flag_sets_build() {
        assert!(
            parse_run(&["foo:v1", "--build"]),
            "--build should set build on run"
        );
    }

    #[test]
    fn run_without_build_flag_defaults_to_false() {
        assert!(
            !parse_run(&["foo:v1"]),
            "build should default to false on run"
        );
    }

    #[test]
    fn run_i_short_flag_sets_instance_id() {
        assert_eq!(
            parse_run_instance_id(&["foo:v1", "-i", "my-inst"]),
            Some("my-inst".to_string()),
            "-i should set instance_id on run"
        );
    }

    #[test]
    fn run_long_instance_id_flag_sets_instance_id() {
        assert_eq!(
            parse_run_instance_id(&["foo:v1", "--instance-id", "my-inst"]),
            Some("my-inst".to_string()),
            "--instance-id should set instance_id on run"
        );
    }

    #[test]
    fn run_bi_short_bundle_sets_build_and_instance_id() {
        // `-bi <id>` ≡ `-b -i <id>`: clap treats `-bi` as a bundled short-flag
        // run with `i` consuming the next positional as its value.
        match parse("run", &["foo:v1", "-bi", "my-inst"]) {
            NodeCommands::Run {
                build, instance_id, ..
            } => {
                assert!(build, "-bi should set build");
                assert_eq!(
                    instance_id,
                    Some("my-inst".to_string()),
                    "-bi should set instance_id"
                );
            }
            _ => panic!("expected Run variant"),
        }
    }

    #[test]
    fn add_f_short_flag_sets_force() {
        assert!(parse_subcommand_force("add", &[".", "-f"]));
    }

    #[test]
    fn build_f_short_flag_sets_force() {
        assert!(parse_subcommand_force("build", &["foo:v1", "-f"]));
    }

    #[test]
    fn remove_f_short_flag_sets_force() {
        assert!(parse_subcommand_force("remove", &["foo:v1", "-f"]));
    }

    #[test]
    fn add_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("add", &["."]));
    }

    #[test]
    fn build_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("build", &["foo:v1"]));
    }

    #[test]
    fn remove_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("remove", &["foo:v1"]));
    }

    #[test]
    fn link_repeated() {
        assert_eq!(
            parse_run_links(&["foo:v1", "--link", "feed@cam_a", "--link", "ctl@cam_b"]),
            vec![
                ("feed".to_string(), "cam_a".to_string()),
                ("ctl".to_string(), "cam_b".to_string())
            ]
        );
    }

    #[test]
    fn link_comma_delimited() {
        assert_eq!(
            parse_run_links(&["foo:v1", "--link", "feed@cam_a,ctl@cam_b"]),
            vec![
                ("feed".to_string(), "cam_a".to_string()),
                ("ctl".to_string(), "cam_b".to_string())
            ]
        );
    }

    #[test]
    fn link_missing_at_separator_rejected() {
        let err = try_parse("run", &["foo:v1", "--link", "noseparator"])
            .err()
            .expect("missing @ should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("KEY@TARGET"), "msg: {msg}");
    }

    #[test]
    fn link_reserved_sentinel_key_rejected() {
        let err = try_parse("run", &["foo:v1", "--link", "_@cam_a"])
            .err()
            .expect("`_` should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    // ─── `peppy node add` --link / requires=run parse-time enforcement ───

    /// `--link` on `node add` requires `--run`; the links parser is the same
    /// as `node run`'s, so a `KEY@VALUE` pair lands on the `links` field as
    /// a tuple.
    #[test]
    fn add_with_run_accepts_link() {
        let links = parse_add_links(&[".", "-r", "--link", "feed@cam_a"]);
        assert_eq!(links, vec![("feed".to_string(), "cam_a".to_string())]);
    }

    /// Comma-delimited form and repeated `--link` both feed the same `links`
    /// vector on `node add -r`, exactly like on `node run`.
    #[test]
    fn add_with_run_link_repeated_and_comma_delimited() {
        let repeated = parse_add_links(&[".", "-r", "--link", "feed@cam_a", "--link", "ctl@cam_b"]);
        let comma = parse_add_links(&[".", "-r", "--link", "feed@cam_a,ctl@cam_b"]);
        let expected = vec![
            ("feed".to_string(), "cam_a".to_string()),
            ("ctl".to_string(), "cam_b".to_string()),
        ];
        assert_eq!(repeated, expected);
        assert_eq!(comma, expected);
    }

    /// `-sbr` plus `--link` parses in one invocation, so the user supplies the
    /// bindings of the chained run in one shot.
    #[test]
    fn add_sbr_with_link_parses() {
        let cli = try_parse("add", &[".", "-sbr", "--link", "feed@cam_a"])
            .expect("`add . -sbr --link feed@cam_a` should parse");
        match cli.command {
            NodeCommands::Add {
                sync,
                build,
                run,
                links,
                ..
            } => {
                assert!(sync && build && run, "-sbr should set all three");
                assert_eq!(links, vec![("feed".to_string(), "cam_a".to_string())]);
            }
            _ => panic!("expected Add variant"),
        }
    }

    /// The run-only arguments of `node add` need `--run`: `requires = "run"`
    /// rejects them at parse time instead of silently dropping them.
    #[test]
    fn add_run_only_arguments_without_run_are_rejected_at_parse_time() {
        let cases: [&[&str]; 4] = [
            // `--link` has nothing to apply its bindings to without a run.
            &[".", "--link", "feed@cam_a"],
            // `-sb` is "stop at built", and `--link` does not belong there
            // either.
            &[".", "-sb", "--link", "feed@cam_a"],
            // Positional `key=value` arguments are runtime overrides, so they
            // only make sense when chaining a run.
            &[".", "frequency=30"],
            // `--instance-id` only matters when there is a spawn, and without
            // `--run` the instance is never created.
            &[".", "--instance-id", "my-inst"],
        ];
        for args in cases {
            let invocation = args.join(" ");
            let Err(err) = try_parse("add", args) else {
                panic!("`add {invocation}` without --run must be a parse error");
            };
            let msg = err.to_string();
            assert!(
                msg.contains("--run") || msg.contains("<RUN>"),
                "`add {invocation}`: error should name the missing --run flag: {msg}"
            );
        }
    }

    /// Sanity: a plain `add` with neither run-only flag must keep parsing:
    /// `requires = "run"` only fires when one of the gated fields is
    /// present.
    #[test]
    fn add_without_any_run_only_args_still_parses() {
        let cli = try_parse("add", &["."]).expect("plain `add .` should parse");
        match cli.command {
            NodeCommands::Add {
                run,
                args,
                instance_id,
                links,
                ..
            } => {
                assert!(!run);
                assert!(args.is_empty());
                assert!(instance_id.is_none());
                assert!(links.is_empty());
            }
            _ => panic!("expected Add variant"),
        }
    }

    /// The bind value parser is shared between `node add` and `node run`,
    /// so the reserved-sentinel rejection on `_@…` must trigger on `add`
    /// too. Catches a regression where `add` would silently accept invalid
    /// keys that `run` rejects.
    #[test]
    fn add_bind_reserved_sentinel_key_rejected() {
        let err = try_parse("add", &[".", "-r", "--link", "_@cam_a"])
            .err()
            .expect("`_@…` must be rejected on `add` too");
        let msg = err.to_string();
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    /// And the missing-`@` rejection.
    #[test]
    fn add_bind_missing_at_separator_rejected() {
        let err = try_parse("add", &[".", "-r", "--link", "noseparator"])
            .err()
            .expect("missing `@` must be rejected on `add` too");
        let msg = err.to_string();
        assert!(msg.contains("KEY@TARGET"), "msg: {msg}");
    }

    // ── --link (pairing/observer form) / --vacant-link ───────────────────

    #[test]
    fn link_kv_parses_plain_and_pinned_targets() {
        assert_eq!(
            parse_link_kv("arm@arm_1").unwrap(),
            ("arm".to_string(), "arm_1".to_string())
        );
        // The `/link_id` pin rides through as part of the target string; the
        // daemon-side validator splits and resolves it against the manifest.
        assert_eq!(
            parse_link_kv("controller@cmd_1/left_arm").unwrap(),
            ("controller".to_string(), "cmd_1/left_arm".to_string())
        );
    }

    #[test]
    fn link_kv_rejects_bad_shapes() {
        // Missing `@`.
        assert!(parse_link_kv("arm").unwrap_err().contains("KEY@TARGET"));
        // Empty target instance.
        assert!(parse_link_kv("arm@").unwrap_err().contains("TARGET"));
        // KEY must be a wire segment: the reserved sentinel is rejected.
        assert!(parse_link_kv("_@arm_1").unwrap_err().contains("KEY"));
        // So must a wildcard target slot suffix.
        assert!(
            parse_link_kv("arm@arm_1/*")
                .unwrap_err()
                .contains("target slot")
        );
    }

    #[test]
    fn vacant_link_validates_the_slot_and_demands_a_reason() {
        assert_eq!(
            parse_vacant_link(" arm = monitor rig ").unwrap(),
            ("arm".to_string(), vacant_reason("monitor rig"))
        );
        // The reason is free prose, so the first `=` separates and the rest
        // rides through untouched.
        assert_eq!(
            parse_vacant_link("arm=set a=b upstream").unwrap(),
            ("arm".to_string(), vacant_reason("set a=b upstream"))
        );
        // SLOT must be a wire segment.
        assert!(parse_vacant_link("_=why").is_err());
        assert!(parse_vacant_link("a/b=why").is_err());
        // A vacancy without a reason says only "not this one".
        assert!(
            parse_vacant_link("arm=")
                .unwrap_err()
                .contains("needs a reason")
        );
        assert!(
            parse_vacant_link("arm=   ")
                .unwrap_err()
                .contains("needs a reason")
        );
        // No `=` at all.
        assert!(parse_vacant_link("arm").unwrap_err().contains("SLOT=<why>"));
    }

    #[test]
    fn run_accepts_repeated_link_and_vacant_link_flags() {
        let cli = try_parse(
            "run",
            &[
                "commander:v1",
                "--link",
                "left@arm_1",
                "--link",
                "right@arm_2/controller",
                "--vacant-link",
                "gripper=bench rig: no gripper, and a comma, in the reason",
            ],
        )
        .expect("link flags should parse");
        match cli.command {
            NodeCommands::Run {
                links,
                vacant_links,
                ..
            } => {
                assert_eq!(
                    links,
                    vec![
                        ("left".to_string(), "arm_1".to_string()),
                        ("right".to_string(), "arm_2/controller".to_string()),
                    ]
                );
                // A reason is prose: the comma stays in it rather than
                // splitting the flag the way `--link` values split.
                assert_eq!(
                    vacant_links,
                    vec![(
                        "gripper".to_string(),
                        vacant_reason("bench rig: no gripper, and a comma, in the reason")
                    )]
                );
            }
            _ => panic!("expected Run variant"),
        }
    }

    /// Repeated `--link KEY` occurrences accumulate the slot's target set in
    /// flag order (the check against the slot kind and cardinality happens
    /// later in the validators); only the exact same `KEY@TARGET` pair twice
    /// is a hard error here. A repeated key on a pairing/observer slot
    /// survives this stage and is rejected downstream as a multi-target value.
    #[test]
    fn repeated_link_keys_accumulate_and_exact_duplicates_are_rejected() {
        let flags = |targets: &[&str]| {
            LinkValue::Bound(Selection::Flags(
                LinkTargets::new(targets.iter().map(|t| t.to_string()).collect())
                    .expect("test targets are unique"),
            ))
        };
        let link = |key: &str, target: &str| (key.to_string(), target.to_string());

        let map = merge_link_flags(&[link("arm", "arm_1"), link("grip", "grip_1")], &[])
            .expect("distinct keys should collect");
        assert_eq!(map.get("arm"), Some(&flags(&["arm_1"])));
        assert_eq!(map.get("grip"), Some(&flags(&["grip_1"])));

        // Different targets on one key accumulate in flag occurrence order.
        let map = merge_link_flags(
            &[
                link("cameras", "rear_camera"),
                link("cameras", "front_camera"),
            ],
            &[],
        )
        .expect("repeated keys accumulate");
        assert_eq!(
            map.get("cameras"),
            Some(&flags(&["rear_camera", "front_camera"])),
            "flag occurrence order must be preserved"
        );

        // The exact same KEY@TARGET pair twice is a hard error.
        let err = merge_link_flags(&[link("arm", "arm_1"), link("arm", "arm_1")], &[])
            .expect_err("duplicate link rejected");
        assert!(
            err.contains("--link arm@arm_1") && err.contains("once"),
            "unexpected error: {err}"
        );
    }

    /// One key, one value: the flags merge into the same map a launch file
    /// parses into, so a slot given both flags is a collision at the boundary
    /// rather than a contradiction some later validator has to notice.
    #[test]
    fn a_slot_cannot_be_both_linked_and_vacant_on_the_command_line() {
        let err = merge_link_flags(
            &[("arm".to_string(), "arm_1".to_string())],
            &[("arm".to_string(), vacant_reason("no arm here"))],
        )
        .expect_err("one slot cannot take both flags");
        assert!(
            err.contains("`arm`") && err.contains("both linked and declared vacant"),
            "unexpected error: {err}"
        );
    }

    /// Repeating `--vacant-link` on one slot is the same collision, and says
    /// so in its own words rather than claiming the slot was linked.
    #[test]
    fn a_slot_cannot_be_declared_vacant_twice() {
        let err = merge_link_flags(
            &[],
            &[
                ("arm".to_string(), vacant_reason("no arm here")),
                ("arm".to_string(), vacant_reason("also no arm here")),
            ],
        )
        .expect_err("one slot takes one reason");
        assert!(
            err.contains("`arm`") && err.contains("declared vacant twice"),
            "unexpected error: {err}"
        );
    }

    /// A vacancy becomes a `links` entry like any other, so downstream sees
    /// one map holding every slot's fate.
    #[test]
    fn vacant_flags_join_the_same_links_map() {
        let map = merge_link_flags(
            &[("arm".to_string(), "arm_1".to_string())],
            &[(
                "gripper".to_string(),
                vacant_reason("  no gripper on this rig  "),
            )],
        )
        .expect("distinct slots merge");
        assert_eq!(
            map.get("gripper")
                .and_then(LinkValue::vacancy)
                .map(VacantReason::as_str),
            Some("no gripper on this rig"),
            "the reason is trimmed as it becomes a value"
        );
        assert!(map.get("arm").and_then(LinkValue::selection).is_some());
    }

    /// Links are applied at instance start, so `--link` without `--run` on
    /// `node add` is a parse error.
    #[test]
    fn add_link_requires_run() {
        let err = try_parse("add", &[".", "--link", "arm@arm_1"])
            .err()
            .expect("--link without --run must be rejected");
        assert!(err.to_string().contains("--run"), "msg: {err}");
        let err = try_parse("add", &[".", "--vacant-link", "arm=no arm here"])
            .err()
            .expect("--vacant-link without --run must be rejected");
        assert!(err.to_string().contains("--run"), "msg: {err}");
    }
}
