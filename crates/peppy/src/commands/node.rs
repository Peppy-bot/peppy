mod add;
mod builder;
mod env;
mod info;
mod init;
mod remove;
mod run;
mod runtime_config;
pub(crate) mod source;
mod stop;
mod sync;
mod types;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{ArgGroup, Subcommand};
use config::node::Toolchain;
use tracing::info;

use super::Command;
use crate::{context::AppContext, error::Error as CommandError};

pub use add::{AddNodeParams, add_node};
pub use builder::build_node_async;
pub use init::NodeInitBuilder;
#[cfg(feature = "test-support")]
pub use run::run_instance_async;
pub use types::NodeName;
// Internal helper: `launch` reaches it through the `node::` path; no test or
// `main.rs` consumer, so it stays off the public surface. (`BuildNodeParams`
// and `build_node` are used only here via the `builder::` path, so they need no
// re-export at all.)
pub(crate) use env::caller_env_overrides;

/// Default idle timeout in seconds (resets on output).
pub(crate) const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 600;
/// Default absolute max timeout in seconds (safety net).
pub(crate) const DEFAULT_MAX_TIMEOUT_SECS: u64 = 3600;

/// Idle + absolute-max timeout pair used by `node add` and `node run` polling loops.
pub struct TimeoutConfig {
    pub idle_secs: u64,
    pub max_secs: u64,
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
    let (key, value) = raw
        .split_once('@')
        .ok_or_else(|| format!("invalid --link value '{raw}': expected KEY@TARGET"))?;
    let key = key.trim();
    let value = value.trim();
    if value.is_empty() {
        return Err(format!(
            "invalid --link value '{raw}': TARGET cannot be empty"
        ));
    }
    pmi::Segment::try_from(key).map_err(|e| format!("invalid --link KEY '{key}': {e}"))?;
    let (instance, link) = daemon_config::launcher::split_pair_target(value);
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

/// Parses a `--defer-link` argument: a single pairing/observer slot `link_id`
/// deliberately left unresolved at launch, validated as a wire segment.
fn parse_defer_link(raw: &str) -> Result<String, String> {
    let link_id = raw.trim();
    pmi::Segment::try_from(link_id)
        .map_err(|e| format!("invalid --defer-link LINK_ID '{link_id}': {e}"))?;
    Ok(link_id.to_string())
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
        /// Link a `link_id` from this node's `depends_on` to a target:
        /// `KEY@TARGET`. One flag for every link kind — a producer binding
        /// (TARGET is a producer `instance_id`), a pairing (TARGET is a peer
        /// `instance_id[/peer_link_id]`), or an observer source (TARGET is a
        /// source `instance_id[/source_link_id]`). Repeatable across slots
        /// (`--link a@p1 --link b@p2`) or comma-separated (`--link a@p1,b@p2`);
        /// repeating a KEY accumulates a producer binding's set on a
        /// multi-cardinality slot (and is rejected on a `one` slot or on a
        /// pairing/observer slot). Only valid alongside `--run`: without a
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
        /// Explicitly start with a required pairing/observer slot unresolved:
        /// `LINK_ID`. Repeatable. Only valid alongside `--run`.
        #[arg(
            long = "defer-link",
            value_parser = parse_defer_link,
            action = clap::ArgAction::Append,
            requires = "run",
        )]
        defer_links: Vec<String>,
        /// Idle timeout in seconds; resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// When set, bypass the confirmation prompt and stop running instances before overwriting.
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
        /// Idle timeout in seconds; resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// Cancel any in-progress build for this node and start a new one
        #[arg(short = 'f', long)]
        force: bool,
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
        /// Link a `link_id` from this node's `depends_on` to a target:
        /// `KEY@TARGET`. One flag for every link kind. For a producer binding,
        /// TARGET is the producer's `instance_id` (see `peppy node list`),
        /// repeatable across slots (`--link a@p1 --link b@p2`) or
        /// comma-separated (`--link a@p1,b@p2`); repeating a KEY accumulates a
        /// multi-cardinality slot's set in flag order and is rejected on a
        /// `one` slot. For a pairing or observer link, TARGET is a peer/source
        /// `instance_id[/link_id]`, where the `/link_id` suffix disambiguates
        /// when the target plays the role through several slots. A required
        /// slot left unlinked aborts the run unless it is a `zero_or_more`
        /// producer slot or an `optional` pairing slot.
        #[arg(
            long = "link",
            value_delimiter = ',',
            value_parser = parse_link_kv,
            action = clap::ArgAction::Append,
        )]
        links: Vec<(String, String)>,
        /// Explicitly start with a required pairing/observer slot unresolved:
        /// `LINK_ID`. Repeatable. The instance boots with the slot silent (no
        /// wire traffic) until a later start resolves it — for a pairing, via
        /// `--link <peer_slot>@<this_instance>/<LINK_ID>` from the peer.
        #[arg(
            long = "defer-link",
            value_parser = parse_defer_link,
            action = clap::ArgAction::Append,
        )]
        defer_links: Vec<String>,
        /// Idle timeout in seconds; resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// If set, build the node first if it has not already been built.
        /// No-op (with a message) when the node is already built.
        #[arg(short = 'b', long)]
        build: bool,
    },
    /// Prints out the runtime config of a node instance
    #[command(group(ArgGroup::new("node_source").required(true).args(["node_name", "node_dir"])))]
    RuntimeConfig {
        /// Name of the node
        #[arg(long)]
        node_name: Option<String>,
        /// Directory containing the peppy.json5 configuration file
        node_dir: Option<PathBuf>,
        /// Runtime arguments as key=value pairs (e.g., device.physical=/dev/video0 video.frame_rate=30)
        /// Dot-separated keys create nested objects in the arguments
        #[arg(value_parser = parse_key_value_arg)]
        args: Vec<(String, String)>,
    },
    /// Stop a running node instance
    Stop {
        /// Instance ID of the node to stop
        instance_id: String,
    },
    /// Remove a node from the node stack
    Remove {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: (String, String),
        /// When set, stop all instances running on this node before removing the node itself. Without this flag, the command prompts if instances are still running
        #[arg(long)]
        stop_instances: bool,
        /// When set, bypass the confirmation prompt and stop running instances before removal
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
                    node_init_builder = node_init_builder.to_dir(dir);
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
                links,
                defer_links,
                idle_timeout,
                max_timeout,
                force,
            } => {
                // `requires = "run"` on `args`, `instance_id`, `links`, and
                // `defer_links` means we can only land here with
                // `run == false` when all of them are empty; the run-only
                // fields therefore have a single legal home: inside
                // `Some(RunAfterAddOptions)`.
                let run_options = if run {
                    Some(add::RunAfterAddOptions {
                        args,
                        instance_id,
                        links,
                        defer_links,
                    })
                } else {
                    None
                };
                let timeouts = TimeoutConfig {
                    idle_secs: idle_timeout,
                    max_secs: max_timeout,
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
            } => {
                let timeouts = TimeoutConfig {
                    idle_secs: idle_timeout,
                    max_secs: max_timeout,
                };
                builder::build_node(
                    ctx,
                    builder::BuildNodeParams {
                        node_name,
                        node_tag,
                        timeouts,
                        force,
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
                links,
                defer_links,
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
                };
                run::run_node(
                    ctx,
                    node_name,
                    tag,
                    args,
                    instance_id,
                    links,
                    defer_links,
                    timeouts,
                    build,
                )
            }
            NodeCommands::RuntimeConfig {
                node_name,
                node_dir,
                args,
            } => runtime_config::print_runtime_config(ctx, node_name, node_dir, args),
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
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    /// Tiny clap harness that wraps `NodeCommands` so we can exercise argument
    /// parsing for `peppy node add` in isolation.
    #[derive(Parser)]
    #[command(name = "peppy")]
    struct TestCli {
        #[command(subcommand)]
        command: NodeCommands,
    }

    fn parse_add(args: &[&str]) -> (bool, bool, bool) {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("add"))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
            NodeCommands::Add {
                sync, build, run, ..
            } => (sync, build, run),
            _ => panic!("expected Add variant"),
        }
    }

    fn parse_run(args: &[&str]) -> bool {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("run"))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
            NodeCommands::Run { build, .. } => build,
            _ => panic!("expected Run variant"),
        }
    }

    fn parse_run_instance_id(args: &[&str]) -> Option<String> {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("run"))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
            NodeCommands::Run { instance_id, .. } => instance_id,
            _ => panic!("expected Run variant"),
        }
    }

    fn parse_run_links(args: &[&str]) -> Vec<(String, String)> {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("run"))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
            NodeCommands::Run { links, .. } => links,
            _ => panic!("expected Run variant"),
        }
    }

    fn try_parse_run(args: &[&str]) -> Result<TestCli, clap::Error> {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("run"))
            .chain(args.iter().copied())
            .collect();
        TestCli::try_parse_from(full)
    }

    fn try_parse_add(args: &[&str]) -> Result<TestCli, clap::Error> {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("add"))
            .chain(args.iter().copied())
            .collect();
        TestCli::try_parse_from(full)
    }

    fn parse_add_links(args: &[&str]) -> Vec<(String, String)> {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once("add"))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
            NodeCommands::Add { links, .. } => links,
            _ => panic!("expected Add variant"),
        }
    }

    fn parse_subcommand_force(subcommand: &str, args: &[&str]) -> bool {
        let full: Vec<&str> = std::iter::once("peppy")
            .chain(std::iter::once(subcommand))
            .chain(args.iter().copied())
            .collect();
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
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
        let full: Vec<&str> = vec!["peppy", "run", "foo:v1", "-bi", "my-inst"];
        let cli = TestCli::try_parse_from(full).expect("should parse");
        match cli.command {
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
    fn test_bind_repeated() {
        assert_eq!(
            parse_run_links(&["foo:v1", "--link", "feed@cam_a", "--link", "ctl@cam_b"]),
            vec![
                ("feed".to_string(), "cam_a".to_string()),
                ("ctl".to_string(), "cam_b".to_string())
            ]
        );
    }

    #[test]
    fn test_bind_comma_delimited() {
        assert_eq!(
            parse_run_links(&["foo:v1", "--link", "feed@cam_a,ctl@cam_b"]),
            vec![
                ("feed".to_string(), "cam_a".to_string()),
                ("ctl".to_string(), "cam_b".to_string())
            ]
        );
    }

    #[test]
    fn test_bind_missing_at_separator_rejected() {
        let err = try_parse_run(&["foo:v1", "--link", "noseparator"])
            .err()
            .expect("missing @ should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("KEY@TARGET"), "msg: {msg}");
    }

    #[test]
    fn test_bind_reserved_sentinel_key_rejected() {
        let err = try_parse_run(&["foo:v1", "--link", "_@cam_a"])
            .err()
            .expect("`_` should be rejected");
        let msg = err.to_string();
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    // ─── `peppy node add` --bind / requires=run parse-time enforcement ───

    /// `--bind` on `node add` requires `--run`; the binds parser is the same
    /// as `node run`'s, so a `KEY@VALUE` pair lands on the `binds` field as
    /// a tuple.
    #[test]
    fn add_with_run_accepts_bind() {
        let binds = parse_add_links(&[".", "-r", "--link", "feed@cam_a"]);
        assert_eq!(binds, vec![("feed".to_string(), "cam_a".to_string())]);
    }

    /// Comma-delimited form and repeated `--bind` both feed the same `binds`
    /// vector on `node add -r`, exactly like on `node run`.
    #[test]
    fn add_with_run_bind_repeated_and_comma_delimited() {
        let repeated = parse_add_links(&[".", "-r", "--link", "feed@cam_a", "--link", "ctl@cam_b"]);
        let comma = parse_add_links(&[".", "-r", "--link", "feed@cam_a,ctl@cam_b"]);
        let expected = vec![
            ("feed".to_string(), "cam_a".to_string()),
            ("ctl".to_string(), "cam_b".to_string()),
        ];
        assert_eq!(repeated, expected);
        assert_eq!(comma, expected);
    }

    /// `-sbr` plus `--bind` is the bug-replication shape from the report:
    /// the previous CLI accepted `add . -sbr` and ran an instance with no
    /// bindings. Now `--bind` must parse on that same invocation so the
    /// user can supply them in one shot.
    #[test]
    fn add_sbr_with_bind_parses() {
        let cli = try_parse_add(&[".", "-sbr", "--link", "feed@cam_a"])
            .expect("`add . -sbr --bind feed@cam_a` should parse");
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

    /// Core fix: `--bind` without `--run` is meaningless (nothing to apply
    /// the bindings to), so clap rejects it at parse time.
    #[test]
    fn add_bind_without_run_rejected_at_parse_time() {
        let err = try_parse_add(&[".", "--link", "feed@cam_a"])
            .err()
            .expect("--bind without --run must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("--run") || msg.contains("<RUN>"),
            "error should name the missing --run flag: {msg}"
        );
    }

    /// Same enforcement when sync+build are present but `--run` isn't:
    /// `-sb` is "stop at built", and `--bind` doesn't belong there either.
    #[test]
    fn add_sb_with_bind_rejected_at_parse_time() {
        let err = try_parse_add(&[".", "-sb", "--link", "feed@cam_a"])
            .err()
            .expect("`-sb --bind` (no `r`) must error");
        let msg = err.to_string();
        assert!(
            msg.contains("--run") || msg.contains("<RUN>"),
            "error should name the missing --run flag: {msg}"
        );
    }

    /// Positional `key=value` arguments are runtime overrides, so they only
    /// make sense when chaining a run. `requires = "run"` rejects bare
    /// `add . frequency=30` instead of silently ignoring the value.
    #[test]
    fn add_positional_args_without_run_rejected_at_parse_time() {
        let err = try_parse_add(&[".", "frequency=30"])
            .err()
            .expect("trailing key=value without --run must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("--run") || msg.contains("<RUN>"),
            "error should name the missing --run flag: {msg}"
        );
    }

    /// `--instance-id` only matters when there's a spawn. Without `--run`
    /// the instance is never created, so the flag is rejected at parse time
    /// rather than being silently dropped on the floor.
    #[test]
    fn add_instance_id_without_run_rejected_at_parse_time() {
        let err = try_parse_add(&[".", "--instance-id", "my-inst"])
            .err()
            .expect("--instance-id without --run must be a parse error");
        let msg = err.to_string();
        assert!(
            msg.contains("--run") || msg.contains("<RUN>"),
            "error should name the missing --run flag: {msg}"
        );
    }

    /// Sanity: a plain `add` with neither run-only flag must keep parsing:
    /// `requires = "run"` only fires when one of the gated fields is
    /// present.
    #[test]
    fn add_without_any_run_only_args_still_parses() {
        let cli = try_parse_add(&["."]).expect("plain `add .` should parse");
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
        let err = try_parse_add(&[".", "-r", "--link", "_@cam_a"])
            .err()
            .expect("`_@…` must be rejected on `add` too");
        let msg = err.to_string();
        assert!(msg.contains("reserved"), "msg: {msg}");
    }

    /// And the missing-`@` rejection.
    #[test]
    fn add_bind_missing_at_separator_rejected() {
        let err = try_parse_add(&[".", "-r", "--link", "noseparator"])
            .err()
            .expect("missing `@` must be rejected on `add` too");
        let msg = err.to_string();
        assert!(msg.contains("KEY@TARGET"), "msg: {msg}");
    }

    // ── --link (pairing/observer form) / --defer-link ────────────────────

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
    fn defer_link_validates_the_link_id_as_a_wire_segment() {
        assert_eq!(parse_defer_link(" arm ").unwrap(), "arm");
        assert!(parse_defer_link("_").is_err());
        assert!(parse_defer_link("a/b").is_err());
    }

    #[test]
    fn run_accepts_repeated_link_and_defer_link_flags() {
        let cli = try_parse_run(&[
            "commander:v1",
            "--link",
            "left@arm_1",
            "--link",
            "right@arm_2/controller",
            "--defer-link",
            "gripper",
        ])
        .expect("link flags should parse");
        match cli.command {
            NodeCommands::Run {
                links, defer_links, ..
            } => {
                assert_eq!(
                    links,
                    vec![
                        ("left".to_string(), "arm_1".to_string()),
                        ("right".to_string(), "arm_2/controller".to_string()),
                    ]
                );
                assert_eq!(defer_links, vec!["gripper".to_string()]);
            }
            _ => panic!("expected Run variant"),
        }
    }

    /// Links are applied at instance start, so `--link` without `--run` on
    /// `node add` is a parse error.
    #[test]
    fn add_link_requires_run() {
        let err = try_parse_add(&[".", "--link", "arm@arm_1"])
            .err()
            .expect("--link without --run must be rejected");
        assert!(err.to_string().contains("--run"), "msg: {err}");
        let err = try_parse_add(&[".", "--defer-link", "arm"])
            .err()
            .expect("--defer-link without --run must be rejected");
        assert!(err.to_string().contains("--run"), "msg: {err}");
    }
}
