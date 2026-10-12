use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use config::runtime::{CoreNodeTarget, Name};
use core_node_api::encoding::{LaunchGoal, LauncherOrigin, PlacementSpec};
use daemon_config::launcher::{PeppyLauncher, PeppyLauncherParser, PreparedLauncher};
use stack_goal::StackGoal;
use tracing::info;

use super::LauncherArgs;
use super::goal::drive_stack_goal;
use crate::commands::node::caller_env_overrides;
use crate::context::AppContext;
use crate::error::{Error, Result};

/// Mints the identity of one federated launch.
///
/// Random rather than derived from the launcher or the clock: two launches of
/// the same file must be distinguishable (otherwise a reset would target the
/// wrong one), and nothing may depend on clock agreement across machines.
fn new_launch_id() -> String {
    format!("launch-{}", names_generator2::get_random(rand::rng()))
}

/// True when `input` syntactically looks like a filesystem path: it carries a path separator
/// or a `.json5` extension. Such inputs never fall back to repository lookup so a typoed file
/// path surfaces a precise file-not-found error instead of a confusing "launcher not in cache".
fn looks_like_fs_path(input: &Path) -> bool {
    let s = input.as_os_str().to_string_lossy();
    if s.contains('/') || s.contains('\\') {
        return true;
    }
    input.extension().is_some_and(|ext| ext == "json5")
}

/// Resolves a user-supplied launcher path, treating a path without the `.json5` extension as
/// shorthand for the sibling file that has it. When no such sibling exists the original path
/// is returned unchanged so the caller's existing not-found error path fires.
///
/// Lives in the CLI (the sole caller) rather than `core-node-api`: it touches the filesystem
/// (`is_file`), which a pure wire-codec crate should not do.
fn resolve_launcher_path(path: PathBuf) -> PathBuf {
    if path.extension().is_some_and(|ext| ext == "json5") {
        return path;
    }
    let mut with_ext = path.clone().into_os_string();
    with_ext.push(".json5");
    let candidate = PathBuf::from(with_ext);
    if candidate.is_file() { candidate } else { path }
}

/// Parses a launcher file, naming the file in the error: the parse error alone locates the
/// offending field within the document, not the document on disk.
pub(super) fn parse_launcher_file(path: &Path) -> Result<PeppyLauncher> {
    PeppyLauncherParser::from_path(path)
        .map_err(|e| Error::ExecutionFailed(format!("launcher file {}: {e}", path.display())))
}

/// Decide whether the user wants a filesystem launcher or a repository launcher.
///
/// `./demo.json5`, `/abs/path`, `dir/foo`, `foo.json5` → `Fs`, with the path canonicalized so
/// the daemon finds it regardless of its working directory. A bare name like
/// `openarm01_sim_teleop` (no separator, no `.json5` extension) → `Repository`, whatever the
/// current directory holds: the decision is made on the argument alone, and a same-named file
/// next to the caller is never read.
pub(super) fn infer_launcher_origin(input: PathBuf) -> Result<LauncherOrigin> {
    if !looks_like_fs_path(&input) {
        return Ok(LauncherOrigin::Repository {
            name: input.to_string_lossy().into_owned(),
        });
    }
    let resolved = resolve_launcher_path(input);
    let canonical = resolved.canonicalize().map_err(|e| {
        Error::ExecutionFailed(format!(
            "Failed to resolve launcher config path '{}': {}",
            resolved.display(),
            e
        ))
    })?;
    Ok(LauncherOrigin::Fs(canonical))
}

/// The `--place` / `--local` wiring as the user typed it.
///
/// `--local` is a flag rather than a launcher key because the launcher's author
/// declares a TOPOLOGY and cannot know what machines the person running it has.
/// Mixing the two is refused: they are two ways of saying where things go, and
/// a launch that took some placements from one and some from the other would be
/// legible to nobody.
#[derive(clap::Args, Default)]
pub struct PlacementArgs {
    /// Wire a placement link to a real federated core node:
    /// `NAME@<core-node>`, with NAME a `core_nodes` placeholder the
    /// launcher declares or the name of a copy the file lists. A copy
    /// `--join` adds runs where its `--join` names. Repeatable, once per
    /// link; `self` names the daemon this command is sent to.
    #[arg(long = "place", value_name = "NAME@CORE_NODE", value_parser = parse_place)]
    pub places: Vec<(Name, CoreNodeTarget)>,
    /// Wire every declared core node link to this daemon, so a
    /// multi-machine launcher runs unmodified on one box. How you develop
    /// against a federated topology with no second machine. No `--join` and
    /// no `--with` word then names a machine.
    #[arg(long)]
    pub local: bool,
}

/// `--place NAME@CORE_NODE`: NAME is a core node link the flat launcher
/// carries, which every copy's name is one of, and CORE_NODE a machine or
/// `self`.
fn parse_place(raw: &str) -> std::result::Result<(Name, CoreNodeTarget), String> {
    let (link, target) =
        crate::commands::node::parse_key_at_target(raw, "--place", "NAME@CORE_NODE")?;
    let link = Name::new(link.as_str())
        .map_err(|reason| format!("invalid --place link `{link}`: {reason}"))?;
    let target = CoreNodeTarget::parse(&target).map_err(|reason| {
        format!("invalid --place target `{target}` for core node link `{link}`: {reason}")
    })?;
    Ok((link, target))
}

impl PlacementArgs {
    /// The raw flags as the intent the goal carries.
    ///
    /// The CLI owns only the flag GRAMMAR (no repeats, no `--local` mixed with
    /// `--place`). Which links a launcher declares, whether the wired ones
    /// match, and whether each target is live on the federation are all the
    /// coordinator's: it has the resolved document, which for a `Repository`
    /// launcher the CLI may never have seen. That is why `--local` travels as
    /// [`PlacementSpec::Local`] rather than as the map it expands to, and why
    /// `self` travels as the coordinator, which the daemon resolves.
    fn resolve(&self) -> Result<PlacementSpec> {
        if self.local && !self.places.is_empty() {
            return Err(Error::ExecutionFailed(
                "--local and --place cannot be combined: --local puts every core node link on \
                 this machine, so there is nothing left for --place to decide. Drop one."
                    .to_owned(),
            ));
        }

        if self.local {
            return Ok(PlacementSpec::Local);
        }

        let mut resolved = BTreeMap::new();
        for (link, target) in &self.places {
            if resolved.insert(link.clone(), target.clone()).is_some() {
                return Err(Error::ExecutionFailed(format!(
                    "--place wires core node link `{link}` more than once; each link takes \
                     exactly one core node"
                )));
            }
        }
        Ok(PlacementSpec::Places(resolved))
    }
}

/// The goal of a launch of `origin` under `budgets`, with its placement, its
/// words and its joins, refused as the daemon would refuse it: a `--place`
/// key that names a `--join` copy, and a machine on a `--join` or a word
/// under `--local`.
fn launch_goal(
    origin: LauncherOrigin,
    budgets: core_node_api::encoding::StackBudgets,
    placement: &PlacementArgs,
    words: Vec<core_node_api::encoding::LaunchWord>,
    joins: Vec<core_node_api::encoding::CopyReference>,
) -> Result<LaunchGoal> {
    LaunchGoal::new(
        origin,
        // The launch id is minted here, by the process that starts the launch,
        // and recorded by every participant alongside its slice. That is what
        // makes the global stack reconstructible by query afterwards.
        new_launch_id(),
        budgets,
    )
    .with_layout(placement.resolve()?, words, joins)
    .map_err(|error| Error::ExecutionFailed(error.to_string()))
}

/// The machines other than `coordinator` that `goal` names: its `--place`
/// values, the machines of its `--join` copies and of its words. `self` is
/// the coordinator, never a remote machine.
fn remote_machines<'a>(goal: &'a LaunchGoal, coordinator: &str) -> BTreeSet<&'a str> {
    let places = match goal.placement() {
        PlacementSpec::Places(places) => places.values().collect(),
        PlacementSpec::Local => Vec::new(),
    };
    places
        .into_iter()
        .chain(goal.joins().iter().map(|join| &join.core_node))
        .chain(goal.selections().iter().filter_map(|word| word.core_node()))
        .filter_map(CoreNodeTarget::named)
        .map(|machine| machine.as_str())
        .filter(|machine| *machine != coordinator)
        .collect()
}

/// Sends the launch `args` describes to the daemon as `G` and follows it to
/// its result: a [`LaunchGoal`] runs the stack, a
/// [`core_node_api::encoding::StackBuildGoal`] stops once every node is
/// built.
pub(super) fn launch<G>(ctx: &Arc<AppContext>, args: LauncherArgs) -> Result<()>
where
    G: StackGoal + From<LaunchGoal>,
{
    crate::commands::block_on(launch_async::<G>(ctx, args))
}

async fn launch_async<G>(ctx: &Arc<AppContext>, args: LauncherArgs) -> Result<()>
where
    G: StackGoal + From<LaunchGoal>,
{
    let LauncherArgs {
        launcher_config_path,
        placement,
        with,
        joins,
        timeouts,
        rebuild,
        no_prebuild,
    } = args;
    let words = with.words;
    let joins = joins.joins;
    let subcommand = G::OPERATION.to_ascii_lowercase();
    let launcher_origin = infer_launcher_origin(launcher_config_path)?;

    let goal = launch_goal(
        launcher_origin.clone(),
        timeouts.budgets().with_env_vars(caller_env_overrides()),
        &placement,
        words,
        joins,
    )?
    .with_rebuild(rebuild)
    .with_prebuild(!no_prebuild);
    // Pre-validate the launcher config locally for `Fs` so the user gets a fast, precise parse
    // error before the daemon round-trip. `Repository` resolution lives daemon-side, so we
    // skip the local check rather than duplicate the lookup here. Only the verdict is
    // used: placement and the `--with` selection are resolved against the document the
    // COORDINATOR read, so that a repository launcher and a file one resolve identically.
    if let LauncherOrigin::Fs(path) = &launcher_origin {
        let parsed = parse_launcher_file(path)?;
        PreparedLauncher::load(&parsed, path)
            .and_then(|prepared| prepared.launch(goal.selections(), goal.joins()))
            .map_err(|error| Error::ExecutionFailed(error.to_string()))?;
    }

    let conn = ctx.connect_to_daemon().await?;

    // An `Fs` origin names a path on THIS machine's filesystem, so a peer
    // daemon would open a different tree or nothing at all. Same guard the
    // other daemon-scoped commands already apply.
    if matches!(launcher_origin, LauncherOrigin::Fs(_)) {
        let command = format!("peppy stack {subcommand}");
        crate::commands::reject_remote_target_for_local_path(&conn, &command).map_err(|_| {
            Error::ExecutionFailed(format!(
                "`{command}` with a launcher file path cannot target the remote daemon `{}`: \
                 the path names a tree on this machine. Use a repository launcher \
                 (`{command} <name>`), or run the command from the machine that holds the \
                 file.",
                conn.target_core_node
            ))
        })?;
    }

    // State loudly which remote daemons are about to have their stacks
    // replaced. A launch or a build is destructive on every machine it
    // touches, and the operator typed only one command. `--local` names none
    // by construction, and a `--place` target the launcher does not declare is
    // the coordinator's refusal to make, so this only reports what was asked
    // for: the `--place` values, the `--join` machines and the word machines.
    let remote = remote_machines(&goal, &conn.target_core_node);
    if !remote.is_empty() {
        println!(
            "This {subcommand} will REPLACE the node stack on {} remote daemon(s): {}",
            remote.len(),
            remote.iter().copied().collect::<Vec<_>>().join(", ")
        );
    }

    match &launcher_origin {
        LauncherOrigin::Fs(path) => info!(
            "Calling launcher on daemon '{}' with config={}",
            conn.target_core_node,
            path.display()
        ),
        LauncherOrigin::Repository { name } => info!(
            "Calling launcher on daemon '{}' with repository launcher `{}`",
            conn.target_core_node, name
        ),
    }

    let budgets = goal.budgets.clone();

    drive_stack_goal(&conn, &G::from(goal), &budgets).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_launcher_path_appends_json5_when_sibling_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("openarm01_sim_teleop");
        let with_ext = tmp.path().join("openarm01_sim_teleop.json5");
        std::fs::write(&with_ext, "{}").unwrap();

        assert_eq!(resolve_launcher_path(bare), with_ext);
    }

    #[test]
    fn resolve_launcher_path_keeps_explicit_json5_extension() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("foo.json5");
        std::fs::write(&p, "{}").unwrap();

        assert_eq!(resolve_launcher_path(p.clone()), p);
    }

    #[test]
    fn resolve_launcher_path_returns_original_when_no_sibling_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("does_not_exist");

        assert_eq!(resolve_launcher_path(bare.clone()), bare);
    }

    #[test]
    fn resolve_launcher_path_ignores_directory_at_sibling_path() {
        let tmp = tempfile::tempdir().unwrap();
        let bare = tmp.path().join("name");
        std::fs::create_dir(tmp.path().join("name.json5")).unwrap();

        assert_eq!(resolve_launcher_path(bare.clone()), bare);
    }

    #[test]
    fn looks_like_fs_path_detects_separators() {
        assert!(looks_like_fs_path(Path::new("./foo")));
        assert!(looks_like_fs_path(Path::new("../foo")));
        assert!(looks_like_fs_path(Path::new("/abs/foo")));
        assert!(looks_like_fs_path(Path::new("dir/foo")));
    }

    #[test]
    fn looks_like_fs_path_detects_json5_extension() {
        assert!(looks_like_fs_path(Path::new("foo.json5")));
    }

    #[test]
    fn looks_like_fs_path_treats_bare_name_as_non_path() {
        assert!(!looks_like_fs_path(Path::new("openarm01_sim_teleop")));
        assert!(!looks_like_fs_path(Path::new("foo_bar")));
    }

    #[test]
    fn infer_launcher_origin_canonicalizes_existing_fs_path() {
        let tmp = tempfile::tempdir().unwrap();
        let p = tmp.path().join("launcher.json5");
        std::fs::write(&p, "{}").unwrap();

        let origin = infer_launcher_origin(p.clone()).expect("should resolve fs path");
        match origin {
            LauncherOrigin::Fs(resolved) => {
                assert_eq!(resolved, p.canonicalize().unwrap());
            }
            other => panic!("expected Fs, got {other:?}"),
        }
    }

    #[test]
    fn infer_launcher_origin_errors_when_fs_looking_path_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("missing.json5");

        let err = infer_launcher_origin(missing).expect_err("should fail on missing file");
        match err {
            Error::ExecutionFailed(msg) => assert!(
                msg.contains("Failed to resolve launcher config path"),
                "unexpected error: {msg}"
            ),
            other => panic!("expected ExecutionFailed, got {other:?}"),
        }
    }

    /// The decision is made on the argument alone: a bare name is a
    /// repository launcher without a look at the filesystem. The integration
    /// suite runs the binary next to a same-named file to hold that.
    #[test]
    fn infer_launcher_origin_resolves_a_bare_name_through_the_repository() {
        let origin =
            infer_launcher_origin(PathBuf::from("openarm_v2")).expect("bare name should not error");
        match origin {
            LauncherOrigin::Repository { name } => assert_eq!(name, "openarm_v2"),
            other => panic!("expected Repository, got {other:?}"),
        }
    }

    #[test]
    fn parse_launcher_file_names_the_file_in_its_error() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("openarm_v2.json5");
        std::fs::write(&file, r#"{ peppy_schema: "mcp_exposure/v1" }"#).unwrap();

        let err = parse_launcher_file(&file).expect_err("an exposure is not a launcher");

        let message = err.to_string();
        assert!(message.contains(&file.display().to_string()), "{message}");
        assert!(
            message.contains("expected peppy_schema 'launcher/v1', got 'mcp_exposure/v1'"),
            "{message}"
        );
    }

    fn places(pairs: &[(&str, &str)]) -> PlacementArgs {
        PlacementArgs {
            places: pairs
                .iter()
                .map(|(link, target)| parse_place(&format!("{link}@{target}")).unwrap())
                .collect(),
            local: false,
        }
    }

    /// The wirings of a `--place` resolution, or a panic naming what came out
    /// instead. Every `--place` case must produce explicit places; `--local`
    /// has its own assertions.
    fn resolved_places(args: PlacementArgs) -> BTreeMap<String, String> {
        match args.resolve().expect("valid wiring") {
            PlacementSpec::Places(places) => places
                .into_iter()
                .map(|(link, target)| (link.to_string(), target.to_string()))
                .collect(),
            PlacementSpec::Local => panic!("expected explicit places, got --local"),
        }
    }

    #[test]
    fn place_wires_each_link_to_its_core_node() {
        let resolved = resolved_places(places(&[
            ("robot_onboard", "cn-robot-7"),
            ("cloud_inference", "cn-atlas-h100"),
        ]));

        assert_eq!(resolved["robot_onboard"], "cn-robot-7");
        assert_eq!(resolved["cloud_inference"], "cn-atlas-h100");
    }

    /// `self` means "the daemon this launch is sent to": it travels as the
    /// coordinator, which the daemon resolves.
    #[test]
    fn self_travels_as_the_coordinator() {
        let PlacementSpec::Places(resolved) = places(&[("robot_onboard", "self")])
            .resolve()
            .expect("valid wiring")
        else {
            panic!("expected explicit places");
        };
        assert_eq!(
            resolved[&Name::new("robot_onboard").unwrap()],
            CoreNodeTarget::Coordinator
        );
    }

    /// Wiring two placeholders to one machine is legitimate: a launcher that
    /// describes a two-machine topology must still be runnable on one box.
    #[test]
    fn two_links_may_share_one_core_node() {
        let resolved = resolved_places(places(&[
            ("robot_onboard", "cn-solo"),
            ("cloud_inference", "cn-solo"),
        ]));
        assert_eq!(resolved.len(), 2);
        assert!(resolved.values().all(|target| target == "cn-solo"));
    }

    #[test]
    fn wiring_one_link_twice_is_refused() {
        let error = places(&[
            ("robot_onboard", "cn-robot-7"),
            ("robot_onboard", "cn-atlas-h100"),
        ])
        .resolve()
        .expect_err("a link takes exactly one core node");
        assert!(error.to_string().contains("more than once"), "got: {error}");
    }

    #[test]
    fn a_malformed_place_is_refused_by_the_shared_validator() {
        let error = parse_place("robot_onboard@has space").expect_err("a bad core node name");
        assert!(error.contains("robot_onboard"), "got: {error}");
        assert!(error.contains("characters"), "got: {error}");
        let error = parse_place("bad link@cn-robot-7").expect_err("a bad link name");
        assert!(error.contains("invalid --place link"), "got: {error}");
    }

    /// `--local` travels as INTENT. The CLI never expands it, for either
    /// launcher origin: which links exist is a property of the document, and
    /// for a repository launcher only the coordinator has read it. Expanding
    /// here would make `--local` work for a file path and quietly do nothing
    /// for a name.
    #[test]
    fn local_travels_as_intent_rather_than_an_expansion() {
        let resolved = PlacementArgs {
            places: Vec::new(),
            local: true,
        }
        .resolve()
        .expect("valid wiring");
        assert_eq!(resolved, PlacementSpec::Local);
    }

    #[test]
    fn local_mixed_with_place_is_refused() {
        let error = PlacementArgs {
            places: vec![parse_place("robot_onboard@cn-robot-7").unwrap()],
            local: true,
        }
        .resolve()
        .expect_err("--local and --place are two ways to say the same thing");
        assert!(
            error.to_string().contains("cannot be combined"),
            "got: {error}"
        );
    }

    #[test]
    fn no_placement_flags_wire_nothing() {
        let resolved = resolved_places(PlacementArgs::default());
        assert!(resolved.is_empty());
    }

    /// The notice names every remote machine the launch places something on:
    /// a `--place` value, a `--join` machine and a word machine, each once,
    /// never the coordinator, whether named or written `self`.
    #[test]
    fn the_remote_machines_are_every_machine_the_launch_names() {
        let words = [
            "xiaozhu.ai_brain_vla@cn-gpu-box",
            "bravo.pad@self",
            "mujoco",
        ]
        .iter()
        .map(|word| word.parse().unwrap())
        .collect();
        let joins = [
            "openarm_sim:xiaozhu@cn-jetson-1",
            "openarm_sim:charlie@cn-robot-7",
        ]
        .iter()
        .map(|join| join.parse().unwrap())
        .collect();
        let goal = LaunchGoal::new(
            LauncherOrigin::repository("simulation_mcp"),
            "launch-1",
            core_node_api::encoding::StackBudgets::new(1, 1, 1, None),
        )
        .with_layout(
            places(&[("bravo", "cn-gpu-box"), ("robot_onboard", "self")])
                .resolve()
                .unwrap(),
            words,
            joins,
        )
        .unwrap();
        assert_eq!(
            remote_machines(&goal, "cn-robot-7")
                .into_iter()
                .collect::<Vec<_>>(),
            ["cn-gpu-box", "cn-jetson-1"]
        );
    }

    /// The goal holds the rules that read several flags: a `--place` key
    /// names no `--join` copy, and `--local` leaves every machine unnamed.
    #[test]
    fn the_goal_refuses_a_placed_join_copy_and_a_machine_under_local() {
        let goal = |placement: PlacementArgs, words: &[&str], joins: &[&str]| {
            launch_goal(
                LauncherOrigin::repository("simulation_mcp"),
                core_node_api::encoding::StackBudgets::new(1, 1, 1, None),
                &placement,
                words.iter().map(|word| word.parse().unwrap()).collect(),
                joins.iter().map(|join| join.parse().unwrap()).collect(),
            )
        };
        let error = goal(
            places(&[("xiaozhu", "cn-x")]),
            &[],
            &["openarm_sim:xiaozhu"],
        )
        .expect_err("a joined copy is placed where it is joined");
        assert!(
            error.to_string().contains(
                "place a joined copy where you join it: `--join openarm_sim:xiaozhu@CORE_NODE`"
            ),
            "{error}"
        );
        let local = || PlacementArgs {
            places: Vec::new(),
            local: true,
        };
        for (words, joins) in [
            (
                &["xiaozhu.ai_brain_vla@self"][..],
                &["openarm_sim:xiaozhu"][..],
            ),
            (&[][..], &["openarm_sim:xiaozhu@cn-x"][..]),
        ] {
            let error = goal(local(), words, joins).expect_err("--local names no machine");
            assert!(
                error
                    .to_string()
                    .contains("`--local` runs the whole launch on this machine"),
                "{error}"
            );
        }
        goal(
            local(),
            &["xiaozhu.ai_brain_vla", "mujoco"],
            &["openarm_sim:xiaozhu"],
        )
        .expect("--local with no machine named");
        goal(
            places(&[("bravo", "cn-x")]),
            &["xiaozhu.ai_brain_vla@cn-gpu-box"],
            &["openarm_sim:xiaozhu@cn-y"],
        )
        .expect("a copy of the file is placed with --place, a joined one where it is joined");
    }

    /// Two launches of the same file must be distinguishable, or a reset would
    /// target the wrong one.
    #[test]
    fn launch_ids_are_distinct_per_launch() {
        assert_ne!(new_launch_id(), new_launch_id());
        assert!(new_launch_id().starts_with("launch-"));
    }
}
