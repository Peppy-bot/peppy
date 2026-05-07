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
pub use builder::{BuildNodeParams, build_node, build_node_async};
pub use env::caller_env_overrides;
pub use init::NodeInitBuilder;
pub use types::NodeName;

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
    let pos = s.find(':').ok_or_else(|| {
        format!(
            "invalid node reference '{}': expected node_name:tag format",
            s
        )
    })?;
    let node_name = s[..pos].trim().to_string();
    let tag = s[pos + 1..].trim().to_string();
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
    let pos = s
        .find('=')
        .ok_or_else(|| format!("invalid argument format '{}': expected key=value", s))?;
    let key = s[..pos].trim().to_string();
    let value = s[pos + 1..].trim().to_string();
    if key.is_empty() {
        return Err(format!("invalid argument '{}': key cannot be empty", s));
    }
    Ok((key, value))
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
        /// Variant selection. Repeatable.
        ///
        /// Three shapes are accepted:
        /// - Variant name for the *root* node: `mock-python`.
        /// - Git or HTTP URL used directly as the root variant:
        ///   `https://github.com/org/repo.git/path`,
        ///   `https://example.com/variant.tar.zst`.
        /// - `<dep_name>:<dep_tag>@<variant_name>` — override the
        ///   variant used when resolving a transitive dependency of
        ///   the target node (only meaningful when the source is
        ///   `<name>:<tag>`). Repeatable.
        ///
        /// At most one *root* shape may appear per invocation; the
        /// dep-override shape can appear any number of times with
        /// distinct `name:tag` values.
        #[arg(long = "variant")]
        variant: Vec<String>,
        /// If set, runs `peppy node sync` on the source *before* adding. Forces
        /// peppygen interface code to be regenerated from the current
        /// `peppy.json5`, so the snapshot taken by `node add` is up-to-date.
        ///
        /// Only valid for local filesystem sources — remote (git/http)
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
        /// Runtime arguments as key=value pairs (e.g., resolution=1280x720 frequency=30)
        /// These are passed to the node via PEPPY_RUNTIME_CONFIG when run is true
        #[arg(value_parser = parse_key_value_arg)]
        args: Vec<(String, String)>,
        /// Optional: specify a deterministic instance ID
        #[arg(long, hide = true)]
        instance_id: Option<String>,
        /// Idle timeout in seconds — resets whenever output is received
        #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
        idle_timeout: u64,
        /// Absolute max timeout in seconds (safety net)
        #[arg(long, default_value_t = DEFAULT_MAX_TIMEOUT_SECS)]
        max_timeout: u64,
        /// When set, bypass the confirmation prompt and stop running instances before overwriting
        #[arg(short = 'f', long)]
        force: bool,
    },
    /// Build a node previously added to the node stack
    Build {
        /// Node reference in the format node_name:tag (e.g., my_node:v1)
        #[arg(value_parser = parse_node_ref)]
        node_ref: (String, String),
        /// Idle timeout in seconds — resets whenever output is received
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
        /// Idle timeout in seconds — resets whenever output is received
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
    /// with an error — inspecting sources on disk is the job of `node add`,
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
                variant,
                sync,
                build,
                run,
                args,
                instance_id,
                idle_timeout,
                max_timeout,
                force,
            } => {
                let run_options = if run {
                    Some(add::RunAfterAddOptions { args, instance_id })
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
                        variant,
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
                run::run_node(ctx, node_name, tag, args, instance_id, timeouts, build)
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
        assert!(
            parse_run(&["foo:0.1.0", "-b"]),
            "-b should set build on run"
        );
    }

    #[test]
    fn run_long_build_flag_sets_build() {
        assert!(
            parse_run(&["foo:0.1.0", "--build"]),
            "--build should set build on run"
        );
    }

    #[test]
    fn run_without_build_flag_defaults_to_false() {
        assert!(
            !parse_run(&["foo:0.1.0"]),
            "build should default to false on run"
        );
    }

    #[test]
    fn run_i_short_flag_sets_instance_id() {
        assert_eq!(
            parse_run_instance_id(&["foo:0.1.0", "-i", "my-inst"]),
            Some("my-inst".to_string()),
            "-i should set instance_id on run"
        );
    }

    #[test]
    fn run_long_instance_id_flag_sets_instance_id() {
        assert_eq!(
            parse_run_instance_id(&["foo:0.1.0", "--instance-id", "my-inst"]),
            Some("my-inst".to_string()),
            "--instance-id should set instance_id on run"
        );
    }

    #[test]
    fn run_bi_short_bundle_sets_build_and_instance_id() {
        // `-bi <id>` ≡ `-b -i <id>`: clap treats `-bi` as a bundled short-flag
        // run with `i` consuming the next positional as its value.
        let full: Vec<&str> = vec!["peppy", "run", "foo:0.1.0", "-bi", "my-inst"];
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
        assert!(parse_subcommand_force("build", &["foo:0.1.0", "-f"]));
    }

    #[test]
    fn remove_f_short_flag_sets_force() {
        assert!(parse_subcommand_force("remove", &["foo:0.1.0", "-f"]));
    }

    #[test]
    fn add_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("add", &["."]));
    }

    #[test]
    fn build_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("build", &["foo:0.1.0"]));
    }

    #[test]
    fn remove_without_force_flag_defaults_to_false() {
        assert!(!parse_subcommand_force("remove", &["foo:0.1.0"]));
    }
}
