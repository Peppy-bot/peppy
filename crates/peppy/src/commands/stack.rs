mod benchmark;
mod goal;
mod join;
mod launch;
mod list;
mod remove;
mod reset;
mod resolve;

pub use list::{StackListReport, list_nodes_collecting, list_nodes_json_collecting};
pub use resolve::{JoinPreview, resolve_rendered};

use std::path::PathBuf;
use std::sync::Arc;

use clap::Subcommand;
use tracing::info;

use super::Command;
use super::node::{DEFAULT_BUILD_IDLE_TIMEOUT_SECS, DEFAULT_IDLE_TIMEOUT_SECS};
use crate::{context::AppContext, error::Error as CommandError};

#[derive(Subcommand)]
pub enum StackCommands {
    /// Launch a stack from a launcher, replacing the current node stack.
    Launch {
        /// The launcher to run: a repository launcher's name, or a path to a
        /// `launcher/v1` file.
        #[arg(value_name = "LAUNCHER")]
        launcher_config_path: PathBuf,
        /// Wire a placement link to a real federated core node:
        /// `NAME@<core-node>`, with NAME a `core_nodes` placeholder the
        /// launcher declares or the name of a copy it deploys. Repeatable,
        /// once per link; `self` names the daemon this command is sent to.
        #[arg(long = "place", value_name = "NAME@CORE_NODE", value_parser = parse_place)]
        place: Vec<(String, String)>,
        /// Wire every declared core node link to this daemon, so a
        /// multi-machine launcher runs unmodified on one box. How you develop
        /// against a federated topology with no second machine.
        #[arg(long)]
        local: bool,
        #[command(flatten)]
        with: WithWords,
        #[command(flatten)]
        timeouts: StackTimeouts,
        /// Build every node from its staged sources even when a cached
        /// artifact built from byte-identical sources exists. Applies to
        /// each node build of the launch, on this daemon and on every peer.
        #[arg(long)]
        rebuild: bool,
    },
    /// Add a copy of one of the launcher's options to the running stack.
    ///
    /// The copy's instances are minted as NAME_<instance-id>, the way
    /// `node run` adds an instance of a node.
    Join {
        /// The option to copy: one of a `zero_or_more` axis of the running
        /// launcher.
        #[arg(value_name = "OPTION")]
        option: String,
        /// The copy's name: the prefix of every instance id it creates and
        /// its placement link.
        #[arg(short = 'i', long = "instance-id", value_name = "NAME", value_parser = parse_copy_name)]
        name: config::runtime::Name,
        #[command(flatten)]
        with: WithWords,
        /// Override an argument of one of the copy's instances with a JSON5
        /// value; the instance id is the one written in the option's fragment.
        #[arg(long = "set-arguments", value_name = "INSTANCE.ARGUMENT=JSON5")]
        arguments: Vec<core_node_api::encoding::ArgumentOverride>,
        /// Run the whole copy on a machine. Defaults to the coordinator,
        /// which `self` also names.
        #[arg(long = "place", value_name = "CORE_NODE", value_parser = join::parse_placement)]
        place: Option<core_node_api::encoding::JoinPlacement>,
        #[command(flatten)]
        timeouts: StackTimeouts,
    },
    /// Stop and remove every instance of one copy.
    Remove {
        /// The copy's name, as listed by `stack list`.
        #[arg(value_parser = parse_copy_name)]
        name: config::runtime::Name,
    },
    /// List the nodes in the current node stack
    List {
        /// Emit machine-readable JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the flat launcher a composed launch would run, and the report
    /// of what the selection did.
    ///
    /// Needs no running stack and touches nothing: the flattened
    /// `launcher/v1` document goes to stdout, so it doubles as the escape
    /// hatch (flatten, hand-edit, launch the flat file), while the
    /// resolution report goes to stderr.
    Resolve {
        /// The launcher to resolve: a repository launcher's name, or a path
        /// to a `launcher/v1` file.
        #[arg(value_name = "LAUNCHER")]
        launcher_config_path: PathBuf,
        #[command(flatten)]
        with: WithWords,
        #[command(flatten)]
        join: JoinPreview,
    },
    /// Tear the node stack down to an empty state.
    ///
    /// Clears the targeted daemon alone: its stack slice, and any federated
    /// reservation holding the machine (`--core-node` picks a remote daemon;
    /// the default is the local one). When the target holds one slice of a
    /// launch the rest of the system is still running, says so.
    ///
    /// With `--federated`, also tears down every other machine that launch
    /// holds. Participants are REDISCOVERED by query rather than remembered:
    /// keyed on the launch the target's slice names, or on the target's own
    /// coordinator name when its slice is gone, so it works after a daemon
    /// restart and finds machines held by a reservation alone.
    Reset {
        /// Tear down every slice of the launch, from whichever machine this is
        /// run on, rather than just this daemon's.
        #[arg(long)]
        federated: bool,
    },
    /// Benchmark the latency of every interface wiring each node to its direct
    /// dependencies, measured against the already-running stack.
    ///
    /// Service/action numbers are real-payload-sized messaging round-trips (the
    /// user handler is never invoked); topic numbers are real producer→consumer
    /// delivery latency on live traffic (exact on a single host; cross-host needs
    /// PTP/NTP). Benchmarking never triggers a real handler or creates a goal.
    Benchmark {
        /// Timed samples per interface (after warmup).
        #[arg(long, default_value_t = 200)]
        samples: u32,
        /// Warmup samples per interface, discarded before measuring.
        #[arg(long, default_value_t = 20)]
        warmup: u32,
        /// Per-sample probe/observe timeout in milliseconds.
        #[arg(long, default_value_t = 2000)]
        per_sample_timeout_ms: u64,
    },
}

/// A copy's name is its placement link, so it is held to a core node name's
/// grammar before it travels as the [`config::runtime::Name`] the goal
/// carries.
fn parse_copy_name(raw: &str) -> Result<config::runtime::Name, String> {
    use config::runtime::{CoreNodeName, CoreNodeNameError};
    let placement = CoreNodeName::new(raw).map_err(|error| match error {
        CoreNodeNameError::Reserved => format!(
            "`{raw}` names the daemon this command targets, so no copy may be named it; choose \
             another name"
        ),
        CoreNodeNameError::Malformed => {
            format!("a copy's name is its placement link, so it {error}")
        }
    })?;
    Ok(config::runtime::Name::new(placement.into_string()).expect("a core node name is a name"))
}

/// `--place NAME@CORE_NODE` on launch: NAME is a core node link the flat
/// launcher carries, which every copy's name is one of.
fn parse_place(raw: &str) -> Result<(String, String), String> {
    crate::commands::node::parse_key_at_target(raw, "--place", "NAME@CORE_NODE")
}

/// The phase budgets a launch or a join runs under, each idle budget a
/// positive number of seconds.
#[derive(clap::Args, Debug)]
pub struct StackTimeouts {
    /// Idle timeout in seconds for the node add phase (resets on git/http
    /// progress or sub-process output).
    #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
    pub node_add_idle_timeout_secs: u64,
    /// Idle timeout in seconds for the node build phase (resets on build
    /// output or image-download/write progress).
    #[arg(long, default_value_t = DEFAULT_BUILD_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
    pub node_build_idle_timeout_secs: u64,
    /// Idle timeout in seconds for the node run-startup phase (resets on
    /// subprocess output until the node signals ready).
    #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS, value_parser = clap::value_parser!(u64).range(1..))]
    pub node_run_idle_timeout_secs: u64,
    /// Absolute maximum in seconds for the whole operation. Unset, only the
    /// idle timeouts apply.
    #[arg(long)]
    pub max_timeout_secs: Option<u64>,
}

impl StackTimeouts {
    /// The budgets a goal carries, before the caller's environment is added.
    pub fn budgets(&self) -> core_node_api::encoding::StackBudgets {
        core_node_api::encoding::StackBudgets::new(
            self.node_add_idle_timeout_secs,
            self.node_build_idle_timeout_secs,
            self.node_run_idle_timeout_secs,
            self.max_timeout_secs,
        )
    }
}

/// The `--with` words, shared by launch, join, and resolve.
#[derive(clap::Args, Default)]
pub struct WithWords {
    /// Select one option of a `components` axis: `option` or `axis=option`.
    /// Repeatable and comma-separated. At launch the words swap what the
    /// launcher deploys on its `one` axes and turn `zero_or_one` axes on,
    /// reaching the axes of the fragments those selections run, and
    /// `NAME.option` or `NAME.axis=option` selects the own axis of the copy
    /// NAME the file deploys; at join they select the copied option's own
    /// axes.
    ///
    /// The words travel to the coordinator verbatim, like `--local`:
    /// only the daemon holds a repository launcher's document, so only
    /// it knows which axes exist.
    #[arg(
        long = "with",
        value_name = "option|axis=option",
        value_delimiter = ',',
        value_parser = parse_with_word,
        action = clap::ArgAction::Append
    )]
    pub words: Vec<String>,
}

/// One `--with` word: `option` or `axis=option`, never blank. Which axes and
/// options exist is the coordinator's to say (it holds the document), so the
/// CLI checks only that the word says something.
fn parse_with_word(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err(format!(
            "a --with entry is `option`, `axis=option` or, at launch, `NAME.option` or \
             `NAME.axis=option`, never blank (check for a stray comma in {raw:?})"
        ));
    }
    Ok(trimmed.to_owned())
}

pub struct StackCommand {
    pub command: StackCommands,
}

impl Command for StackCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<(), CommandError> {
        match self.command {
            StackCommands::Join {
                option,
                name,
                with,
                arguments,
                place,
                timeouts,
            } => join::join(ctx, option, name, with.words, arguments, place, timeouts),
            StackCommands::Remove { name } => remove::remove(ctx, name),
            StackCommands::List { json } => list::list_nodes(ctx, json),
            StackCommands::Reset { federated } => reset::reset_stack(ctx, federated),
            StackCommands::Resolve {
                launcher_config_path,
                with,
                join,
            } => resolve::resolve(ctx, launcher_config_path, with.words, join),
            StackCommands::Launch {
                launcher_config_path,
                place,
                local,
                with,
                timeouts,
                rebuild,
            } => {
                info!("Launching stack...");
                launch::launch(
                    ctx,
                    launcher_config_path,
                    launch::PlacementArgs {
                        places: place,
                        local,
                    },
                    with.words,
                    timeouts.budgets(),
                    rebuild,
                )
            }
            StackCommands::Benchmark {
                samples,
                warmup,
                per_sample_timeout_ms,
            } => {
                info!("Benchmarking stack...");
                benchmark::benchmark(ctx, samples, warmup, per_sample_timeout_ms)
            }
        }
    }
}
