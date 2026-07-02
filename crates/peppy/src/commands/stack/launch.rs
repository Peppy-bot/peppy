use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use daemon_config::launcher::PeppyLauncherParser;
use core_node_api::encoding::{
    LaunchFeedback, LaunchFeedbackStep, LaunchGoal, LaunchGoalResponse, LaunchResult,
    LauncherOrigin, NodeAddLogEntry, NodeBuildLogEntry, NodeRunLogEntry,
};
use peppylib::ActionMessenger;
use peppylib::messaging::ResultStatus;
use tracing::info;

use crate::commands::node::caller_env_overrides;
use crate::commands::{CALLER_INSTANCE_ID, GOAL_TIMEOUT, SCROLLING_OUTPUT_LINES};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::ScrollingOutput;

use peppylib::core_node::transport::send_launch;
// Minimum CLI fallback ceiling when the user opts into `--max-timeout-secs`. Ensures the CLI's
// safety net never fires before the daemon's own per-phase timeout, so users see a precise
// daemon-side error rather than a generic CLI fallback. When the user omits the flag, no CLI
// ceiling is installed — the contract is idle-only (daemon-side `max_timeout_secs = None`).
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

fn display_node_log_files(
    add_logs: &[NodeAddLogEntry],
    build_logs: &[NodeBuildLogEntry],
    run_logs: &[NodeRunLogEntry],
) {
    if add_logs.is_empty() && build_logs.is_empty() && run_logs.is_empty() {
        return;
    }
    info!("Node log files:");
    if !add_logs.is_empty() {
        info!("  Add:");
        for e in add_logs {
            info!("    `{}`: {}", e.node_label, e.log_path.display());
        }
    }
    if !build_logs.is_empty() {
        info!("  Build:");
        for e in build_logs {
            let marker = if e.failed { " [FAILED]" } else { "" };
            if e.failed {
                tracing::error!("    `{}`{}: {}", e.node_label, marker, e.log_path.display());
            } else {
                info!("    `{}`: {}", e.node_label, e.log_path.display());
            }
        }
    }
    if !run_logs.is_empty() {
        info!("  Run:");
        for e in run_logs {
            let marker = if e.failed { " [FAILED]" } else { "" };
            if e.failed {
                tracing::error!(
                    "    {} (`{}`){}: {}",
                    e.instance_id,
                    e.node_label,
                    marker,
                    e.log_path.display()
                );
            } else {
                info!(
                    "    {} (`{}`): {}",
                    e.instance_id,
                    e.node_label,
                    e.log_path.display()
                );
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

/// Resolves a user-supplied launcher path, treating a bare name as shorthand for the `.json5`
/// file. If the path does not have a `.json5` extension and a sibling `<path>.json5` exists as
/// a file, that is returned; otherwise the original path is returned unchanged so the caller's
/// existing not-found error path fires.
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

/// Decide whether the user wants a filesystem launcher or a repository launcher.
///
/// `./demo.json5`, `/abs/path`, `foo.json5` → `Fs`, with the path canonicalized so the daemon
/// finds it regardless of its working directory. A bare name like `openarm01_sim_teleop` first
/// tries the filesystem (sibling `.json5` shorthand from `resolve_launcher_path`) and only
/// falls back to `Repository` when no file exists at that name.
fn infer_launcher_origin(input: PathBuf) -> Result<LauncherOrigin> {
    if looks_like_fs_path(&input) {
        let resolved = resolve_launcher_path(input);
        let canonical = resolved.canonicalize().map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to resolve launcher config path '{}': {}",
                resolved.display(),
                e
            ))
        })?;
        return Ok(LauncherOrigin::Fs(canonical));
    }

    let resolved = resolve_launcher_path(input.clone());
    if let Ok(canonical) = resolved.canonicalize() {
        return Ok(LauncherOrigin::Fs(canonical));
    }

    Ok(LauncherOrigin::Repository {
        name: input.to_string_lossy().into_owned(),
    })
}

pub fn launch(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    crate::commands::block_on(launch_async(
        ctx,
        launcher_config_path,
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    ))
}

async fn launch_async(
    ctx: &Arc<AppContext>,
    launcher_config_path: PathBuf,
    node_add_idle_timeout_secs: u64,
    node_build_idle_timeout_secs: u64,
    node_run_idle_timeout_secs: u64,
    max_timeout_secs: Option<u64>,
) -> Result<()> {
    let launcher_origin = infer_launcher_origin(launcher_config_path)?;

    // Pre-validate the launcher config locally for `Fs` so the user gets a fast, precise parse
    // error before the daemon round-trip. `Repository` resolution lives daemon-side, so we
    // skip the local check rather than duplicate the lookup here.
    if let LauncherOrigin::Fs(path) = &launcher_origin {
        PeppyLauncherParser::from_path(path).map_err(Error::DaemonConfig)?;
    }

    let conn = ctx.connect_to_daemon().await?;

    match &launcher_origin {
        LauncherOrigin::Fs(path) => info!(
            "Calling launcher on daemon '{}' with config={}",
            conn.core_node_name,
            path.display()
        ),
        LauncherOrigin::Repository { name } => info!(
            "Calling launcher on daemon '{}' with repository launcher `{}`",
            conn.core_node_name, name
        ),
    }

    let goal = LaunchGoal::new(
        launcher_origin,
        node_add_idle_timeout_secs,
        node_build_idle_timeout_secs,
        node_run_idle_timeout_secs,
        max_timeout_secs,
    )
    .with_env_vars(caller_env_overrides());

    // CLI fallback ceiling: when the user opts into a max we grant the daemon a response-grace
    // window to surface its own error first, but never less than the absolute floor in case the
    // daemon hangs entirely. `None` honors the daemon's idle-only contract — no CLI ceiling.
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

    let mut action_handle = send_launch(
        &goal,
        conn.messenger,
        &conn.core_node_name,
        CALLER_INSTANCE_ID,
        None,
        GOAL_TIMEOUT,
    )
    .await
    .map_err(|e| Error::ExecutionFailed(format!("Failed to send launch goal: {}", e)))?;

    let goal_response = LaunchGoalResponse::decode(&action_handle.goal_response().payload())
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
            return Err(Error::ExecutionFailed(format!(
                "Launch timed out: no output received for {}s. Log file: {}",
                cli_idle_timeout.as_secs(),
                goal_response.log_path.display()
            )));
        }

        match tokio::time::timeout(FEEDBACK_DRAIN_TIMEOUT, action_handle.on_next_feedback()).await {
            Ok(Ok(msg)) => {
                last_activity = tokio::time::Instant::now();
                let payload = msg.payload();
                if let Ok(feedback) = LaunchFeedback::decode(&payload) {
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

    #[test]
    fn infer_launcher_origin_falls_back_to_repository_for_bare_name() {
        // Use a name that is unlikely to collide with any sibling .json5 in the test cwd.
        let bare = PathBuf::from("definitely_not_a_real_launcher_xyz123");
        let origin = infer_launcher_origin(bare.clone()).expect("bare name should not error");
        match origin {
            LauncherOrigin::Repository { name } => {
                assert_eq!(name, "definitely_not_a_real_launcher_xyz123");
            }
            other => panic!("expected Repository, got {other:?}"),
        }
    }
}
