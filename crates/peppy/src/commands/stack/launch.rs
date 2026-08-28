use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use core_node::{idle_timeout_flag, slow_connection_hint};
use core_node_api::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    LauncherOrigin, NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry, PlacementSpec,
};
use daemon_config::core_node_name::CoreNodeName;
use daemon_config::launcher::{PeppyLauncher, PeppyLauncherParser, compose};
use peppylib::ActionMessenger;
use peppylib::messaging::ResultStatus;
use tracing::info;

use crate::commands::node::caller_env_overrides;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use peppylib::core_node::transport::send_goal;

/// Mints the identity of one federated launch.
///
/// Random rather than derived from the launcher or the clock: two launches of
/// the same file must be distinguishable (otherwise a reset would target the
/// wrong one), and nothing may depend on clock agreement across machines.
fn new_launch_id() -> String {
    format!("launch-{}", names_generator2::get_random(rand::rng()))
}
// Minimum CLI fallback ceiling when the user opts into `--max-timeout-secs`. Ensures the CLI's
// safety net never fires before the daemon's own per-phase timeout, so users see a precise
// daemon-side error rather than a generic CLI fallback. When the user omits the flag, no CLI
// ceiling is installed; the contract is idle-only (daemon-side `max_timeout_secs = None`).
const CLI_MAX_TIMEOUT_FLOOR: Duration = Duration::from_secs(7200);
// Headroom granted to the daemon to surface its own timeout error before the CLI's fallback
// ceiling fires. Keeps the error the user sees specific ("build idle timeout exceeded...") rather
// than a generic CLI-side "daemon hung" message.
const DAEMON_RESPONSE_GRACE: Duration = Duration::from_secs(60);
const FEEDBACK_DRAIN_TIMEOUT: Duration = Duration::from_millis(50);

// CLI wall-clock fallback ceiling. `None` means idle-only (daemon-side contract honored).
// When the user opts into `--max-timeout-secs`, add DAEMON_RESPONSE_GRACE and enforce
// CLI_MAX_TIMEOUT_FLOOR so the daemon's per-phase error fires first.
fn compute_cli_max_timeout(max_timeout_secs: Option<u64>) -> Option<Duration> {
    max_timeout_secs.map(|n| {
        Duration::from_secs(n)
            .saturating_add(DAEMON_RESPONSE_GRACE)
            .max(CLI_MAX_TIMEOUT_FLOOR)
    })
}

/// One line of the "Node log files" listing:
/// `node_name:tag@core-node: /path/to/log`, with a ` [FAILED]` marker before
/// the colon when the phase failed. The core node names the machine whose
/// filesystem holds the log file.
fn node_log_line(node_label: &str, core_node: &str, failed: bool, log_path: &Path) -> String {
    let marker = if failed { " [FAILED]" } else { "" };
    format!("{node_label}@{core_node}{marker}: {}", log_path.display())
}

fn display_node_log_files(
    add_logs: &[NodeAddLogEntry],
    build_logs: &[NodeBuildLogEntry],
    run_logs: &[NodeRunLogEntry],
) {
    if add_logs.is_empty() && build_logs.is_empty() && run_logs.is_empty() {
        return;
    }
    let line = |node_label: &str, core_node: &str, failed: bool, log_path: &Path| {
        (
            node_log_line(node_label, core_node, failed, log_path),
            failed,
        )
    };
    let sections = [
        (
            "Add",
            add_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect::<Vec<_>>(),
        ),
        (
            "Build",
            build_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect(),
        ),
        (
            "Run",
            run_logs
                .iter()
                .map(|e| line(&e.node_label, &e.core_node, e.failed, &e.log_path))
                .collect(),
        ),
    ];
    info!("Node log files:");
    for (title, lines) in sections {
        if lines.is_empty() {
            continue;
        }
        info!("  {title}:");
        for (text, failed) in lines {
            if failed {
                tracing::error!("    {text}");
            } else {
                info!("    {text}");
            }
        }
    }
}

fn handle_feedback(
    feedback: &LaunchFeedback,
    scrolling_output: &mut Option<ScrollingOutput>,
    current_scrolling_step: &mut Option<LaunchFeedbackStep>,
) {
    // Check if we're switching between steps
    let step_changed = current_scrolling_step
        .as_ref()
        .map(|s| std::mem::discriminant(s) != std::mem::discriminant(&feedback.step))
        .unwrap_or(true);

    if step_changed {
        // Clear existing scrolling output if we were in a scrolling step
        if let Some(output) = scrolling_output.as_mut() {
            output.clear();
            *scrolling_output = None;
        }
        *current_scrolling_step = Some(feedback.step);
    }

    match &feedback.step {
        LaunchFeedbackStep::LauncherStep => {
            if feedback.is_stdout() {
                info!("{}", feedback.line);
            } else {
                tracing::warn!("{}", feedback.line);
            }
        }
        LaunchFeedbackStep::AddingNode
        | LaunchFeedbackStep::BuildingNode
        | LaunchFeedbackStep::RunningNode => {
            let output = scrolling_output
                .get_or_insert_with(|| ScrollingOutput::new(SCROLLING_OUTPUT_LINES));
            output.add_line(&feedback.line);
        }
    }
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
#[derive(Debug, Clone, Default)]
pub struct PlacementArgs {
    pub places: Vec<(String, String)>,
    pub local: bool,
}

impl PlacementArgs {
    /// Resolves the raw flags into the intent the goal carries, with `self`
    /// replaced by the coordinator's real name.
    ///
    /// The CLI owns only the flag GRAMMAR (no repeats, no `--local` mixed with
    /// `--place`, and `self` resolution). Which links a launcher declares,
    /// whether the wired ones match, and whether each target is live on the
    /// federation are all the coordinator's: it has the resolved document,
    /// which for a `Repository` launcher the CLI may never have seen. That is
    /// why `--local` travels as [`PlacementSpec::Local`] rather than as the
    /// map it expands to — the CLI cannot expand what it cannot read.
    fn resolve(&self, coordinator: &str) -> Result<PlacementSpec> {
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
            let target = if CoreNodeName::is_self_keyword(target) {
                coordinator.to_owned()
            } else {
                CoreNodeName::new(target.as_str())
                    .map_err(|reason| {
                        Error::ExecutionFailed(format!(
                            "invalid --place target `{target}` for core node link `{link}`: \
                             {reason}"
                        ))
                    })?
                    .into_string()
            };
            if resolved.insert(link.clone(), target).is_some() {
                return Err(Error::ExecutionFailed(format!(
                    "--place wires core node link `{link}` more than once; each link takes \
                     exactly one core node"
                )));
            }
        }
        Ok(PlacementSpec::Places(resolved))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn launch(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    placement: PlacementArgs,
    with: Vec<String>,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    crate::commands::block_on(launch_async(
        ctx,
        launcher_config_path,
        placement,
        with,
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    ))
}

#[allow(clippy::too_many_arguments)]
async fn launch_async(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    placement: PlacementArgs,
    with: Vec<String>,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    let launcher_origin = infer_launcher_origin(launcher_config_path)?;

    // Pre-validate the launcher config locally for `Fs` so the user gets a fast, precise parse
    // error before the daemon round-trip. `Repository` resolution lives daemon-side, so we
    // skip the local check rather than duplicate the lookup here. Only the verdict is
    // used: placement and the `--with` selection are resolved against the document the
    // COORDINATOR read, so that a repository launcher and a file one resolve identically.
    if let LauncherOrigin::Fs(path) = &launcher_origin {
        let parsed = parse_launcher_file(path)?;
        compose(&parsed, path, &with).map_err(|e| Error::ExecutionFailed(e.to_string()))?;
    }

    let conn = ctx.connect_to_daemon().await?;

    // An `Fs` origin names a path on THIS machine's filesystem, so a peer
    // daemon would open a different tree or nothing at all. Same guard the
    // other daemon-scoped commands already apply.
    if matches!(launcher_origin, LauncherOrigin::Fs(_)) {
        crate::commands::reject_remote_target_for_local_path(&conn, "peppy stack launch").map_err(
            |_| {
                Error::ExecutionFailed(format!(
                    "`peppy stack launch` with a launcher file path cannot target the remote \
                     daemon `{}`: the path names a tree on this machine. Use a repository \
                     launcher (`peppy stack launch <name>`), or run the command from the \
                     machine that holds the file.",
                    conn.target_core_node
                ))
            },
        )?;
    }

    let placement = placement.resolve(&conn.target_core_node)?;

    // State loudly which remote daemons are about to have their stacks
    // replaced. A launch is destructive on every machine it touches, and the
    // operator typed only one command. `--local` names none by construction,
    // and a `--place` target the launcher does not declare is the
    // coordinator's refusal to make, so this only reports what was asked for.
    let remote: BTreeSet<&str> = match &placement {
        PlacementSpec::Places(places) => places
            .values()
            .map(String::as_str)
            .filter(|core_node| *core_node != conn.target_core_node)
            .collect(),
        PlacementSpec::Local => BTreeSet::new(),
    };
    if !remote.is_empty() {
        println!(
            "This launch will REPLACE the node stack on {} remote daemon(s): {}",
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

    let goal = LaunchGoal::new(
        launcher_origin,
        // The launch id is minted here, by the process that starts the launch,
        // and recorded by every participant alongside its slice. That is what
        // makes the global stack reconstructible by query afterwards.
        new_launch_id(),
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    )
    .with_env_vars(caller_env_overrides())
    .with_placement(placement)
    .with_selections(with);

    // CLI fallback ceiling: when the user opts into a max we grant the daemon a response-grace
    // window to surface its own error first, but never less than the absolute floor in case the
    // daemon hangs entirely. `None` honors the daemon's idle-only contract; no CLI ceiling.
    let cli_max_timeout: Option<Duration> = compute_cli_max_timeout(max_timeout_secs);

    // CLI-side liveness watchdog: trips if no feedback arrives from any phase. Must cover the
    // longest per-phase idle budget (only one phase runs at a time) plus a grace window so the
    // daemon's phase-specific timeout always fires first and surfaces a precise error.
    let cli_idle_timeout = Duration::from_secs(
        node_add_idle_timeout_secs
            .max(node_build_idle_timeout_secs)
            .max(node_run_idle_timeout_secs),
    )
    .saturating_add(DAEMON_RESPONSE_GRACE);

    let mut action_handle = send_goal(
        &goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        Some(&conn.target_core_node),
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send launch goal: {}", e)))?;

    let goal_response = LaunchGoalResponse::decode(&action_handle.goal_reply().body)
        .map_err(|e| Error::ExecutionFailed(format!("Failed to decode goal response: {}", e)))?;

    if !goal_response.accepted {
        let reason = goal_response
            .rejection_reason
            .unwrap_or_else(|| "unknown reason".to_string());
        return Err(Error::ExecutionFailed(format!(
            "Launch goal rejected: {}",
            reason
        )));
    }

    info!(
        "Launch goal accepted, log file: {}",
        goal_response.log_path.display()
    );

    let absolute_deadline: Option<tokio::time::Instant> =
        cli_max_timeout.and_then(|d| tokio::time::Instant::now().checked_add(d));
    let mut last_activity = tokio::time::Instant::now();
    let mut scrolling_output: Option<ScrollingOutput> = None;
    let mut current_scrolling_step: Option<LaunchFeedbackStep> = None;

    // Drain feedback until the server closes the stream on completion,
    // honoring the idle / max-timeout budgets.
    loop {
        let now = tokio::time::Instant::now();
        if let Some(deadline) = absolute_deadline
            && now >= deadline
        {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: max timeout exceeded. Log file: {}",
                goal_response.log_path.display()
            )));
        }
        if now.duration_since(last_activity) >= cli_idle_timeout {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            // Name the phase that went quiet and the flag that raises its
            // budget, so a slow-connection user is pointed at the fix instead
            // of a bare timeout.
            let (phase, hint) = match current_scrolling_step {
                Some(step) => (
                    format!(" during the {} phase", step.phase_label()),
                    idle_timeout_flag(step)
                        .map(|flag| format!("; {}", slow_connection_hint(flag)))
                        .unwrap_or_default(),
                ),
                None => (String::new(), String::new()),
            };
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: no output received for {}s{phase}{hint}. Log file: {}",
                cli_idle_timeout.as_secs(),
                goal_response.log_path.display()
            )));
        }

        match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                let payload = msg.payload_bytes();
                if let Ok(feedback) = LaunchFeedback::decode(payload.as_ref()) {
                    handle_feedback(
                        &feedback,
                        &mut scrolling_output,
                        &mut current_scrolling_step,
                    );
                }
            }
            Ok(Err(_)) => break, // end-of-stream: the goal has completed
            Err(_) => {}         // drain slice elapsed; re-check timeouts and keep draining
        }
    }

    // The goal has completed; fetch its (server-buffered) result once. Give it
    // the remaining max budget so it resolves promptly.
    let now = tokio::time::Instant::now();
    let result_timeout = match absolute_deadline {
        Some(deadline) => deadline
            .saturating_duration_since(now)
            .max(Duration::from_secs(1)),
        None => Duration::from_secs(30),
    };
    match ActionMessenger::request_result(conn.messenger, &action_handle, result_timeout).await {
        Ok(reply) => {
            let body = match reply.status {
                ResultStatus::Completed | ResultStatus::Cancelled => reply.body,
                ResultStatus::Abandoned => {
                    if let Some(output) = scrolling_output.as_mut() {
                        output.clear();
                    }
                    return Err(Error::ExecutionFailed(
                        "the launch goal was abandoned by its worker before producing a result"
                            .to_string(),
                    ));
                }
                ResultStatus::Expired => {
                    if let Some(output) = scrolling_output.as_mut() {
                        output.clear();
                    }
                    return Err(Error::ExecutionFailed(
                        "the launch result expired before it could be fetched".to_string(),
                    ));
                }
            };
            let result = LaunchResult::decode(body.as_ref()).map_err(|err| {
                Error::ExecutionFailed(format!("Failed to decode launch result: {}", err))
            })?;

            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }

            display_node_log_files(
                &result.node_add_logs,
                &result.node_build_logs,
                &result.node_run_logs,
            );

            if !result.success {
                let error_msg = result
                    .error_message
                    .unwrap_or_else(|| "unknown error".to_string());
                return Err(Error::ExecutionFailed(format!(
                    "Launch failed: {}. Log file: {}",
                    error_msg,
                    result.log_path.display()
                )));
            }

            info!("Launch configuration applied successfully");
            Ok(())
        }
        Err(err) => {
            if let Some(output) = scrolling_output.as_mut() {
                output.clear();
            }
            Err(Error::ExecutionFailed(format!(
                "Failed to get launch result: {}",
                err
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_preserves_idle_only_contract() {
        assert_eq!(compute_cli_max_timeout(None), None);
    }

    #[test]
    fn small_value_hits_the_floor() {
        let got = compute_cli_max_timeout(Some(60)).expect("some");
        assert_eq!(got, CLI_MAX_TIMEOUT_FLOOR);
    }

    #[test]
    fn large_value_dominates_the_floor() {
        let n = CLI_MAX_TIMEOUT_FLOOR.as_secs() * 2;
        let got = compute_cli_max_timeout(Some(n)).expect("some");
        assert_eq!(got, Duration::from_secs(n) + DAEMON_RESPONSE_GRACE);
    }

    #[test]
    fn saturating_add_does_not_panic_at_u64_max() {
        let got = compute_cli_max_timeout(Some(u64::MAX)).expect("some");
        assert_eq!(got, Duration::MAX);
    }

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
                .map(|(link, target)| ((*link).to_owned(), (*target).to_owned()))
                .collect(),
            local: false,
        }
    }

    /// The wirings of a `--place` resolution, or a panic naming what came out
    /// instead. Every `--place` case must produce explicit places; `--local`
    /// has its own assertions.
    fn resolved_places(args: PlacementArgs, coordinator: &str) -> BTreeMap<String, String> {
        match args.resolve(coordinator).expect("valid wiring") {
            PlacementSpec::Places(places) => places,
            PlacementSpec::Local => panic!("expected explicit places, got --local"),
        }
    }

    #[test]
    fn place_wires_each_link_to_its_core_node() {
        let resolved = resolved_places(
            places(&[
                ("robot_onboard", "cn-robot-7"),
                ("cloud_inference", "cn-atlas-h100"),
            ]),
            "cn-robot-7",
        );

        assert_eq!(resolved["robot_onboard"], "cn-robot-7");
        assert_eq!(resolved["cloud_inference"], "cn-atlas-h100");
    }

    /// `self` means "the daemon this launch is sent to", and it is resolved
    /// CLI-side so a daemon only ever sees concrete names.
    #[test]
    fn self_resolves_to_the_coordinator() {
        let resolved = resolved_places(places(&[("robot_onboard", "self")]), "cn-robot-7");
        assert_eq!(resolved["robot_onboard"], "cn-robot-7");
    }

    /// Wiring two placeholders to one machine is legitimate: a launcher that
    /// describes a two-machine topology must still be runnable on one box.
    #[test]
    fn two_links_may_share_one_core_node() {
        let resolved = resolved_places(
            places(&[("robot_onboard", "cn-solo"), ("cloud_inference", "cn-solo")]),
            "cn-robot-7",
        );
        assert_eq!(resolved.len(), 2);
        assert!(resolved.values().all(|target| target == "cn-solo"));
    }

    #[test]
    fn wiring_one_link_twice_is_refused() {
        let error = places(&[
            ("robot_onboard", "cn-robot-7"),
            ("robot_onboard", "cn-atlas-h100"),
        ])
        .resolve("cn-robot-7")
        .expect_err("a link takes exactly one core node");
        assert!(error.to_string().contains("more than once"), "got: {error}");
    }

    #[test]
    fn a_malformed_place_target_is_refused_by_the_shared_validator() {
        let error = places(&[("robot_onboard", "has space")])
            .resolve("cn-robot-7")
            .expect_err("a bad core node name must fail");
        assert!(error.to_string().contains("robot_onboard"), "got: {error}");
        assert!(error.to_string().contains("characters"), "got: {error}");
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
        .resolve("cn-robot-7")
        .expect("valid wiring");
        assert_eq!(resolved, PlacementSpec::Local);
    }

    #[test]
    fn local_mixed_with_place_is_refused() {
        let error = PlacementArgs {
            places: vec![("robot_onboard".to_owned(), "cn-robot-7".to_owned())],
            local: true,
        }
        .resolve("cn-robot-7")
        .expect_err("--local and --place are two ways to say the same thing");
        assert!(
            error.to_string().contains("cannot be combined"),
            "got: {error}"
        );
    }

    #[test]
    fn no_placement_flags_wire_nothing() {
        let resolved = resolved_places(PlacementArgs::default(), "cn-robot-7");
        assert!(resolved.is_empty());
    }

    /// Two launches of the same file must be distinguishable, or a reset would
    /// target the wrong one.
    #[test]
    fn launch_ids_are_distinct_per_launch() {
        assert_ne!(new_launch_id(), new_launch_id());
        assert!(new_launch_id().starts_with("launch-"));
    }

    #[test]
    fn node_log_line_names_the_core_node_holding_the_file() {
        let line = node_log_line(
            "deliberative_planner:v1",
            "cn-vibrant-chaplygin",
            false,
            Path::new("/tmp/.peppy/logs/add/deliberative_planner_v1.log"),
        );
        assert_eq!(
            line,
            "deliberative_planner:v1@cn-vibrant-chaplygin: \
             /tmp/.peppy/logs/add/deliberative_planner_v1.log"
        );
    }

    #[test]
    fn node_log_line_marks_a_failed_phase_before_the_colon() {
        let line = node_log_line(
            "reactive_policy:v1",
            "cn-robot-7",
            true,
            Path::new("/tmp/.peppy/logs/build/reactive_policy_v1.log"),
        );
        assert_eq!(
            line,
            "reactive_policy:v1@cn-robot-7 [FAILED]: /tmp/.peppy/logs/build/reactive_policy_v1.log"
        );
    }
}
