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

use std::time::Duration;

use core_node_api::ActionGoal;
use core_node_api::encoding::{
    LaunchFeedbackStep, NodeAddFeedback, NodeAddGoal, NodeAddGoalResponse, NodeAddResult,
    NodeBuildFeedback, NodeBuildGoal, NodeBuildGoalResponse, NodeBuildResult, NodeRunFeedback,
    NodeRunGoal, NodeRunGoalResponse, NodeRunResult, ParticipantSliceBeginRequest,
};
use peppylib::core_node::transport::{poll, send_goal, take_goal_result};

use super::super::ProcessLaunchContext;
use super::super::feedback::{publish_stderr, publish_stdout};

/// Bound on a peer accepting a dispatched goal. Accepting is a cheap
/// admission check on the peer, so a healthy one answers well inside this; the
/// budget exists so a peer that stops answering fails the launch rather than
/// hanging it.
const GOAL_ACCEPT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bound on the destructive commit. It tears down the peer's running stack, so
/// it is allowed to take as long as a cooperative shutdown of that stack takes.
const SLICE_BEGIN_TIMEOUT: Duration = Duration::from_secs(120);

/// How long each feedback drain slice waits before re-checking the idle and
/// deadline budgets. Small enough that a blown budget is noticed promptly,
/// large enough not to spin.
const FEEDBACK_DRAIN_SLICE: Duration = Duration::from_millis(50);

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
    fn decode_acceptance(payload: &[u8]) -> std::result::Result<(), String>;
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

            fn decode_acceptance(payload: &[u8]) -> std::result::Result<(), String> {
                let response = <$response>::decode(payload)
                    .map_err(|e| format!("undecodable {} goal response: {e}", $label))?;
                if response.accepted {
                    return Ok(());
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
                #[allow(clippy::redundant_closure_call)]
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
    |result| result
);
impl_remote_goal!(
    NodeBuildGoal,
    LaunchFeedbackStep::BuildingNode,
    "node_build",
    NodeBuildGoalResponse,
    NodeBuildFeedback,
    NodeBuildResult,
    (),
    |_result| ()
);
impl_remote_goal!(
    NodeRunGoal,
    LaunchFeedbackStep::RunningNode,
    "node_run",
    NodeRunGoalResponse,
    NodeRunFeedback,
    NodeRunResult,
    (),
    |_result| ()
);

/// Sends one goal to `core_node` and drives it to completion, relaying its
/// feedback into this launch's stream.
///
/// `idle_timeout` is the same per-phase budget the in-process path uses, but
/// measured against RELAYED feedback rather than local subprocess output: a
/// remote phase has no local activity signal, and the peer's own stream is the
/// only evidence this coordinator has that work is still happening. The launch
/// deadline applies unchanged, because it bounds the whole operation the
/// operator started, wherever the work is running.
pub(in crate::services::stack) async fn run_remote_goal<G: RemoteGoal>(
    ctx: &ProcessLaunchContext,
    core_node: &str,
    goal: &G,
    idle_timeout: Duration,
) -> std::result::Result<G::Outcome, String> {
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

    G::decode_acceptance(handle.goal_reply().body.as_ref())
        .map_err(|reason| format!("`{core_node}` refused the {}: {reason}", G::label()))?;

    let mut last_activity = tokio::time::Instant::now();
    loop {
        let now = tokio::time::Instant::now();
        if ctx.launch_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(format!(
                "max launch timeout exceeded while `{core_node}` was running {}",
                G::label()
            ));
        }
        if now.duration_since(last_activity) >= idle_timeout {
            return Err(format!(
                "`{core_node}` produced no {} output for {}s",
                G::label(),
                idle_timeout.as_secs()
            ));
        }

        match tokio::time::timeout(FEEDBACK_DRAIN_SLICE, handle.on_next_feedback()).await {
            Ok(Ok(message)) => {
                last_activity = tokio::time::Instant::now();
                if let Some(line) = G::decode_feedback_line(message.payload().as_ref()) {
                    publish_stdout(ctx, format!("[{core_node}] {line}"), G::STEP).await;
                }
            }
            // End of stream: the peer completed the goal.
            Ok(Err(_)) => break,
            // Drain slice elapsed with nothing to read; re-check the budgets.
            Err(_) => {}
        }
    }

    let result_timeout = ctx
        .launch_deadline
        .map(|deadline| {
            deadline
                .saturating_duration_since(tokio::time::Instant::now())
                .max(Duration::from_secs(1))
        })
        .unwrap_or(GOAL_ACCEPT_TIMEOUT);
    let payload = take_goal_result(&ctx.messenger, &handle, result_timeout)
        .await
        .map_err(|reason| format!("`{core_node}` {}: {reason}", G::label()))?;

    G::decode_outcome(payload.as_ref()).map_err(|reason| format!("`{core_node}`: {reason}"))
}

/// Tells every participant to replace its stack slice, in parallel.
///
/// This is the first destructive step of a federated launch on any machine, and
/// it happens only once every participant is reserved. A refusal here is
/// returned rather than retried: the reservation makes it nearly impossible
/// (this coordinator holds each machine), so a refusal means the peer's lease
/// lapsed or the network dropped, and continuing would build half a topology.
pub(in crate::services::stack) async fn begin_participant_slices(
    ctx: &ProcessLaunchContext,
    launch_id: &str,
    participants: &[String],
) -> std::result::Result<(), String> {
    let request = ParticipantSliceBeginRequest::new(launch_id);
    let outcomes = futures::future::join_all(participants.iter().map(|core_node| {
        let request = &request;
        async move {
            let outcome = poll(
                request,
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

    let mut refusals = Vec::new();
    for (core_node, outcome) in outcomes {
        match outcome {
            Ok(response) if response.began => {}
            Ok(response) => refusals.push(format!(
                "`{core_node}` refused: {}",
                response
                    .rejection_reason
                    .unwrap_or_else(|| "no reason given".to_owned())
            )),
            Err(e) => refusals.push(format!("`{core_node}` did not answer: {e}")),
        }
    }

    if refusals.is_empty() {
        return Ok(());
    }
    Err(format!(
        "could not take over every participant's stack:\n  {}",
        refusals.join("\n  ")
    ))
}

/// Clears every participant's slice after a failure, naming each one.
///
/// There is no rollback: a launch REPLACES the previous stack, so by the time
/// anything can fail there is nothing to roll back to. The honest end state is
/// an empty slice on every machine the launch touched, which is what this
/// leaves behind, and the operator is told which machines those were.
pub(in crate::services::stack) async fn clear_participant_slices(
    ctx: &ProcessLaunchContext,
    participants: &[String],
) {
    if participants.is_empty() {
        return;
    }
    publish_stderr(
        ctx,
        format!(
            "Clearing the slice this launch started on: {}",
            participants
                .iter()
                .map(|core_node| format!("`{core_node}`"))
                .collect::<Vec<_>>()
                .join(", ")
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
