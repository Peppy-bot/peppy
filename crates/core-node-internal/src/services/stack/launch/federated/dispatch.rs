//! Driving one peer daemon's share of a launch over the wire.
//!
//! # Why the node actions and not a "run this launcher" call
//!
//! A participant is NOT told to launch. It is told to add these nodes, build
//! them, and start these instances, one goal at a time, by a coordinator that
//! already computed the whole plan. Handing a peer the launcher document would
//! give it a second opinion about placement, ordering, and validation, and two
//! daemons with opinions about one graph is exactly the distributed agreement
//! problem this design refuses to have. The coordinator is the only planner;
//! participants execute.
//!
//! That is why dispatch reuses `node_add` / `node_build` / `node_run` rather
//! than introducing a launch-shaped peer endpoint: they are already the
//! narrowest "do this one thing to your stack" verbs, they already stream
//! feedback, and the daemon that receives them still owns every runtime
//! decision about the node it spawns.
//!
//! # Feedback
//!
//! A peer's sub-goal feedback is relayed into the coordinator's own launch
//! stream, prefixed with the core node it came from. The operator typed one
//! command, so they get one stream; the prefix is what keeps it readable when
//! two machines are building at once.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;

use core_node_api::ActionGoal;
use core_node_api::encoding::{
    LaunchFeedbackStep, NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult,
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult, NodeRunFeedback,
    NodeRunGoal, NodeRunGoalResponse, NodeRunResult, ParticipantSliceBeginRequest,
};
use peppylib::ActionMessenger;
use peppylib::core_node::transport::{poll, send_goal};
use peppylib::messaging::ActionGoalHandle;

use super::super::feedback::{publish_stderr, publish_stdout};
use super::super::watchers::{LifecycleWatchers, set_local_watchers};
use crate::services::stack::action::StackChangeContext;

/// Bound on a peer accepting a dispatched goal. Accepting is a cheap
/// admission check on the peer, so a healthy one answers well inside this; the
/// budget exists so a peer that stops answering fails the launch rather than
/// hanging it.
const GOAL_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the destructive commit. It tears down the peer's running stack, so
/// it is allowed to take as long as a cooperative shutdown of that stack takes.
const SLICE_BEGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// Bound on telling a peer to cancel a goal, and again on its answer for the
/// cancelled work: both fit inside the CLI's grace past the change deadline,
/// leaving the rest of it to the rollback.
const CANCEL_SETTLE_TIMEOUT: Duration = Duration::from_secs(15);

/// One of the three node actions, viewed as something a coordinator dispatches.
///
/// The three differ only in their codecs and in which launch step their output
/// belongs to, so the driver below is written once against this.
pub(in crate::services::stack) trait RemoteGoal: ActionGoal {
    /// The launch step a peer's output for this action is attributed to, so
    /// remote lines land in the same place in the UI as local ones.
    const STEP: LaunchFeedbackStep;
    /// What the action's own result carries beyond success or failure.
    type Outcome;

    fn label() -> &'static str;
    /// Decodes the peer's goal response: the log file the peer created for
    /// this goal, on its own filesystem, when it accepted, and the rejection
    /// reason when it refused.
    fn decode_acceptance(payload: &[u8]) -> std::result::Result<PathBuf, String>;
    fn decode_feedback_line(payload: &[u8]) -> Option<String>;
    fn decode_outcome(payload: &[u8]) -> std::result::Result<Self::Outcome, String>;
}

macro_rules! impl_remote_goal {
    ($goal:ty, $step:expr, $label:literal, $response:ty, $feedback:ty, $result:ty, $outcome:ty, $take:expr) => {
        impl RemoteGoal for $goal {
            const STEP: LaunchFeedbackStep = $step;
            type Outcome = $outcome;

            fn label() -> &'static str {
                $label
            }

            fn decode_acceptance(payload: &[u8]) -> std::result::Result<PathBuf, String> {
                let response = <$response>::decode(payload)
                    .map_err(|e| format!("undecodable {} goal response: {e}", $label))?;
                if response.accepted {
                    return Ok(response.log_path);
                }
                Err(response
                    .rejection_reason
                    .unwrap_or_else(|| format!("{} was rejected with no reason given", $label)))
            }

            fn decode_feedback_line(payload: &[u8]) -> Option<String> {
                <$feedback>::decode(payload).ok().map(|f| f.line)
            }

            fn decode_outcome(payload: &[u8]) -> std::result::Result<Self::Outcome, String> {
                let result = <$result>::decode(payload)
                    .map_err(|e| format!("undecodable {} result: {e}", $label))?;
                if !result.success {
                    return Err(result
                        .error_message
                        .unwrap_or_else(|| format!("{} failed with no error message", $label)));
                }
                Ok($take(result))
            }
        }
    };
}

impl_remote_goal!(
    NodeAddGoal,
    LaunchFeedbackStep::AddingNode,
    "node_add",
    NodeAddGoalResponse,
    NodeAddFeedback,
    NodeAddResult,
    NodeAddResult,
    std::convert::identity
);
impl_remote_goal!(
    NodeBuildGoal,
    LaunchFeedbackStep::BuildingNode,
    "node_build",
    NodeBuildGoalResponse,
    NodeBuildFeedback,
    NodeBuildResult,
    (),
    drop
);
impl_remote_goal!(
    NodeRunGoal,
    LaunchFeedbackStep::RunningNode,
    "node_run",
    NodeRunGoalResponse,
    NodeRunFeedback,
    NodeRunResult,
    (),
    drop
);

/// What one accepted goal produced, as the coordinator records it.
///
/// `log_path` names the log file the peer created for the goal, on its own
/// filesystem, known from the moment the peer accepted. It is carried
/// separately from `outcome` so a failed phase still names the file that
/// explains it.
pub(in crate::services::stack) struct RemoteGoalRun<T> {
    pub(in crate::services::stack) log_path: PathBuf,
    pub(in crate::services::stack) outcome: std::result::Result<T, RemoteGoalFailure>,
}

/// Why an accepted goal produced no outcome.
#[derive(Debug)]
pub(in crate::services::stack) enum RemoteGoalFailure {
    /// The peer answered: the goal failed there.
    Reported(String),
    /// This coordinator's budget ended first. The peer was told to cancel
    /// the goal, and what it holds for the node is known only by asking it.
    Unresolved(String),
}

impl std::fmt::Display for RemoteGoalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Reported(reason) | Self::Unresolved(reason) => f.write_str(reason),
        }
    }
}

/// Sends one goal to `core_node` and drives it to completion, relaying its
/// feedback into this launch's stream.
///
/// Returns `Err` only when the peer never accepted the goal (the send failed
/// or the peer refused), which is the one case with no log file to name.
///
/// `idle_timeout` is the same per-phase budget the in-process path uses, but
/// measured against RELAYED feedback rather than local subprocess output: a
/// remote phase has no local activity signal, and the peer's own stream is the
/// only evidence this coordinator has that work is still happening. The launch
/// deadline applies unchanged, because it bounds the whole operation the
/// operator started, wherever the work is running.
pub(in crate::services::stack) async fn run_remote_goal<G: RemoteGoal>(
    ctx: &StackChangeContext,
    core_node: &str,
    goal: &G,
    idle_timeout: Duration,
) -> std::result::Result<RemoteGoalRun<G::Outcome>, String> {
    let mut handle = send_goal(
        goal,
        &ctx.messenger,
        ctx.bound_core_node.as_str(),
        ctx.core_instance_id.as_str(),
        Some(core_node),
        GOAL_ACCEPT_TIMEOUT,
    )
    .await
    .map_err(|e| format!("`{core_node}` did not accept the {} goal: {e}", G::label()))?;

    let log_path = G::decode_acceptance(handle.goal_reply().body.as_ref())
        .map_err(|reason| format!("`{core_node}` refused the {}: {reason}", G::label()))?;

    let outcome = async {
        let mut last_activity = tokio::time::Instant::now();
        loop {
            let now = tokio::time::Instant::now();
            if ctx.change_deadline.is_some_and(|deadline| now >= deadline) {
                return Err(RemoteGoalFailure::Unresolved(format!(
                    "max timeout exceeded while `{core_node}` was running {}",
                    G::label()
                )));
            }
            if now.duration_since(last_activity) >= idle_timeout {
                return Err(RemoteGoalFailure::Unresolved(format!(
                    "`{core_node}` produced no {} output for {}s",
                    G::label(),
                    idle_timeout.as_secs()
                )));
            }

            // Wait exactly until the nearer of the two budgets would be blown,
            // rather than ticking. A remote build can be silent for minutes, and
            // both budgets are re-checked at the top of the loop anyway, so waking
            // any earlier than the deadline that would end the wait is pure spin,
            // multiplied by every peer running a goal concurrently.
            let idle_expiry = last_activity + idle_timeout;
            let wake_at = match ctx.change_deadline {
                Some(deadline) => idle_expiry.min(deadline),
                None => idle_expiry,
            };
            match tokio::time::timeout_at(wake_at, handle.on_next_feedback()).await {
                Ok(Ok(message)) => {
                    last_activity = tokio::time::Instant::now();
                    if let Some(line) = G::decode_feedback_line(message.payload_bytes().as_ref()) {
                        publish_stdout(ctx, format!("[{core_node}] {line}"), G::STEP).await;
                    }
                }
                // End of stream: the peer completed the goal.
                Ok(Err(_)) => break,
                // A budget elapsed with nothing to read; the checks above name it.
                Err(_) => {}
            }
        }

        let result_timeout = ctx
            .change_deadline
            .map(|deadline| {
                deadline
                    .saturating_duration_since(tokio::time::Instant::now())
                    .max(Duration::from_secs(1))
            })
            .unwrap_or(GOAL_ACCEPT_TIMEOUT);
        let payload = ActionMessenger::request_result_body(&ctx.messenger, &handle, result_timeout)
            .await
            .map_err(|reason| {
                RemoteGoalFailure::Unresolved(format!("`{core_node}` {}: {reason}", G::label()))
            })?;

        G::decode_outcome(payload.as_ref())
            .map_err(|reason| RemoteGoalFailure::Reported(format!("`{core_node}`: {reason}")))
    }
    .await;
    if matches!(outcome, Err(RemoteGoalFailure::Unresolved(_))) {
        cancel_remote_goal::<G>(ctx, core_node, &handle).await;
    }

    Ok(RemoteGoalRun { log_path, outcome })
}

/// Tells `core_node` to cancel the goal `handle` drives, then gives it
/// [`CANCEL_SETTLE_TIMEOUT`] to answer for the work, so the peer's own
/// cancel path has run before a rollback asks what it holds. The answer,
/// when one comes, says whether the work was cancelled or finished on its
/// own past the budget.
async fn cancel_remote_goal<G: RemoteGoal>(
    ctx: &StackChangeContext,
    core_node: &str,
    handle: &ActionGoalHandle,
) {
    let label = G::label();
    let line =
        match ActionMessenger::cancel_goal(&ctx.messenger, handle, CANCEL_SETTLE_TIMEOUT).await {
            Ok(_) => {
                match ActionMessenger::request_result_body(
                    &ctx.messenger,
                    handle,
                    CANCEL_SETTLE_TIMEOUT,
                )
                .await
                {
                    Ok(payload) => match G::decode_outcome(payload.as_ref()) {
                        Ok(_) => format!("`{core_node}` finished the {label} after the budget"),
                        Err(reason) => format!("`{core_node}` ended the {label}: {reason}"),
                    },
                    Err(error) => format!(
                        "`{core_node}` was told to cancel the {label} and has not answered for it: \
                     {error}"
                    ),
                }
            }
            Err(error) => format!("`{core_node}` could not be told to cancel the {label}: {error}"),
        };
    publish_stderr(ctx, line, LaunchFeedbackStep::LauncherStep).await;
}

/// A slice-begin some participants did not take: the machines that refused
/// keep the stack they had, so a rollback leaves them alone; a machine that
/// did not answer may hold the slice, so a rollback clears it.
pub(in crate::services::stack) struct SliceBeginRefusal {
    pub(in crate::services::stack) reason: String,
    refusers: Vec<String>,
}

impl SliceBeginRefusal {
    /// The machines among `participants` that may hold the slice.
    pub(in crate::services::stack) fn holders_among(&self, participants: &[String]) -> Vec<String> {
        participants
            .iter()
            .filter(|core_node| !self.refusers.contains(core_node))
            .cloned()
            .collect()
    }
}

/// Tells every participant to replace its stack slice, in parallel, handing
/// each the bind sources its slice needs.
///
/// This is the first destructive step of a federated launch on any machine, and
/// it happens only once every participant is reserved. A refusal here is
/// returned rather than retried: the reservation makes it nearly impossible
/// (this coordinator holds each machine), so a refusal means the peer's lease
/// lapsed or the network dropped, and continuing would build half a topology.
///
/// A participant that had to create one of those sources says so, and the line
/// is relayed here attributed to its machine: an auto-created bind source is
/// how a misspelled file bind looks, and the operator watching this launch is
/// the one who can tell the two apart.
pub(in crate::services::stack) async fn begin_participant_slices(
    ctx: &StackChangeContext,
    launch_id: &str,
    participants: &[String],
    mount_sources_by_machine: &HashMap<String, Vec<String>>,
    watchers: &LifecycleWatchers,
    placements: &daemon_config::launcher::Placements,
    // A join adds to the slice of this launch each participant holds; a
    // launch replaces whatever slice it holds.
    append: bool,
) -> std::result::Result<(), SliceBeginRefusal> {
    set_local_watchers(ctx, watchers, placements);
    ask_participant_slices(ctx, participants, |core_node| {
        let mut request = ParticipantSliceBeginRequest::new(
            launch_id,
            mount_sources_by_machine
                .get(core_node)
                .cloned()
                .unwrap_or_default(),
        );
        request.append = append;
        request.lifecycle_watchers = watchers_hosted_by(watchers, placements, core_node);
        request
    })
    .await
    .map_err(|refusals| SliceBeginRefusal {
        reason: format!(
            "could not take over every participant's stack:\n  {}",
            refusals.lines.join("\n  ")
        ),
        refusers: refusals.refusers,
    })
}

/// Points each source at the watchers `watchers` lists for it, on this
/// machine and on each participant hosting one: the previous plan's lists
/// when a change is undone, the remaining plan's when a copy leaves.
pub(in crate::services::stack) async fn set_participant_watchers(
    ctx: &StackChangeContext,
    launch_id: &str,
    participants: &[String],
    watchers: &LifecycleWatchers,
    placements: &daemon_config::launcher::Placements,
) {
    set_local_watchers(ctx, watchers, placements);
    let Err(refusals) = ask_participant_slices(ctx, participants, |core_node| {
        let mut request = ParticipantSliceBeginRequest::new(launch_id, Vec::new());
        request.append = true;
        request.lifecycle_watchers = watchers_hosted_by(watchers, placements, core_node);
        request
    })
    .await
    else {
        return;
    };
    publish_stderr(
        ctx,
        format!(
            "could not point the sources on every machine at their watchers; the sources \
             there keep their previous watchers until the next change replaces them:\n  {}",
            refusals.lines.join("\n  ")
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;
}

/// The watcher lists of the sources `core_node` hosts, as that participant is
/// handed them.
fn watchers_hosted_by(
    watchers: &LifecycleWatchers,
    placements: &daemon_config::launcher::Placements,
    core_node: &str,
) -> LifecycleWatchers {
    watchers
        .iter()
        .filter(|(instance, _)| placements.of(instance.as_str()) == core_node)
        .map(|(instance, hosts)| (instance.clone(), hosts.clone()))
        .collect()
}

/// The machines that refused a slice-begin, and every refusal and missing
/// answer as a line.
struct SliceRefusals {
    refusers: Vec<String>,
    lines: Vec<String>,
}

/// Asks each participant to hold this launch's slice, with the request
/// `request` builds for it, and names the machines that refused or went
/// unanswered.
async fn ask_participant_slices(
    ctx: &StackChangeContext,
    participants: &[String],
    request: impl Fn(&str) -> ParticipantSliceBeginRequest,
) -> std::result::Result<(), SliceRefusals> {
    let outcomes = futures::future::join_all(participants.iter().map(|core_node| {
        let request = request(core_node);
        async move {
            let outcome = poll(
                &request,
                &ctx.messenger,
                ctx.bound_core_node.as_str(),
                ctx.core_instance_id.as_str(),
                core_node,
                SLICE_BEGIN_TIMEOUT,
            )
            .await;
            (core_node.clone(), outcome)
        }
    }))
    .await;

    let mut refusals = SliceRefusals {
        refusers: Vec::new(),
        lines: Vec::new(),
    };
    for (core_node, outcome) in outcomes {
        match outcome {
            Ok(response) if response.ok => {
                for src in response.auto_created_mount_sources {
                    publish_stderr(
                        ctx,
                        format!("[{core_node}] {}", containers::auto_created_warning(&src)),
                        LaunchFeedbackStep::LauncherStep,
                    )
                    .await;
                }
            }
            Ok(response) => {
                refusals.lines.push(format!(
                    "`{core_node}` refused: {}",
                    response
                        .rejection_reason
                        .unwrap_or_else(|| "no reason given".to_owned())
                ));
                refusals.refusers.push(core_node);
            }
            Err(e) => refusals
                .lines
                .push(format!("`{core_node}` did not answer: {e}")),
        }
    }

    if refusals.lines.is_empty() {
        return Ok(());
    }
    Err(refusals)
}

/// Clears every named participant's slice after a failure, naming each
/// one: a failed launch leaves an empty slice on every machine that took
/// its slice, and a failed join clears the machines that held nothing
/// before it and took its slice.
pub(in crate::services::stack) async fn clear_participant_slices(
    ctx: &StackChangeContext,
    participants: &[String],
) {
    if participants.is_empty() {
        return;
    }
    publish_stderr(
        ctx,
        format!(
            "Clearing the slice this {} started on: {}",
            ctx.action.label(),
            daemon_config::format_quoted_list(participants)
        ),
        LaunchFeedbackStep::LauncherStep,
    )
    .await;

    let failures = futures::future::join_all(participants.iter().map(|core_node| async move {
        let outcome = poll(
            &core_node_api::encoding::StackResetRequest::new(),
            &ctx.messenger,
            ctx.bound_core_node.as_str(),
            ctx.core_instance_id.as_str(),
            core_node,
            SLICE_BEGIN_TIMEOUT,
        )
        .await;
        match outcome {
            Ok(response) if response.success => None,
            Ok(response) => Some(format!(
                "`{core_node}`: {}",
                response
                    .error_message
                    .unwrap_or_else(|| "reset reported failure".to_owned())
            )),
            Err(e) => Some(format!("`{core_node}`: {e}")),
        }
    }))
    .await;

    for failure in failures.into_iter().flatten() {
        publish_stderr(
            ctx,
            format!(
                "could not clear {failure}. That machine may still be running part of this \
                 launch; clear it with `peppy stack reset` from there."
            ),
            LaunchFeedbackStep::LauncherStep,
        )
        .await;
    }
}
