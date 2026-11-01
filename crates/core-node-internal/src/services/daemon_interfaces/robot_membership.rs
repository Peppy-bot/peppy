//! The provider of `robot_membership:v1`: `list`, `join` and `remove` of the
//! copies of the running launch, within the scope of the sender's daemon
//! target of `robot_membership:v1`.
//!
//! - `list` answers:
//!   - `options`: the scope's options with their models, descriptions,
//!     features and kinds;
//!   - `core_nodes`: the machines of the federation now;
//!   - `this_core_node`: this machine;
//!   - `copies`: the copies whose option is in the scope, in name order,
//!     each with its machine, the features its selections hold with the
//!     machine of each, and whether its software runs;
//!   - `max_copies`: the most copies of the scope's options on the stack;
//!   - `change`: the change that runs now when it adds or removes a copy of
//!     a scoped option;
//!   - `launcher`: the launcher of the stack;
//!   - `launch_copies`: the copies of the scope's options that the launch
//!     started, in name order, each as the launch started it, in the form
//!     of `copies` without `running`.
//! - `join` parses the goal into a typed join: the name as a copy name, the
//!   option as a scoped option, each machine as a core node name, and each
//!   feature as a feature of the option, named once (refused `rejected`).
//!   Then it takes the join lock, refuses a name that a copy has and a stack
//!   that holds `max_copies` copies of the scope's options
//!   (`precondition_failed`), and sends the stack join to its own daemon for
//!   the sender, which the leases of the robots then read: the daemon
//!   refuses the join under the name of a robot that another client leases,
//!   and the join of a lease holder during a halt, and leases the new robot
//!   to a lease holder.
//! - `remove` refuses a name that is not a copy of a scoped option, then
//!   sends the removal for the sender: the daemon refuses the removal of a
//!   robot that another client leases.
//!
//! The provider runs one `join` at a time: a `join` takes the lock before
//! it reads the stack and holds it until the daemon's work ends, and a
//! `join` that finds it held is refused with the kind `busy`. A `join` also
//! holds the admission lock until the daemon admitted or refused its stack
//! goal, and `list` waits for that lock before it reads the stack, so `list`
//! reports each addition under `change` from the moment it starts.
//!
//! A goal follows its stack change to its end: each line of the change
//! reaches the client as `{ step, line }`, a cancel of the client reaches the
//! stack change, and the goal ends as the change ends: completed, failed
//! with the failure kind of the change and the log files that say more, or
//! cancelled with the reason of the cancel. A goal that its client does not
//! take once the daemon admitted its change (its accept fails) cancels the
//! change and follows it to its end all the same.

use super::codecs::Side;
use super::{InterfaceContext, end_goal, refuse};
use crate::services::fleet::ClientRef;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use config::runtime::{CopyName, CoreNodeName, CoreNodeTarget, Name};
use core_node_api::encoding::{
    CopyInfo, CopyReference, GoalClient, LaunchFeedbackStep, SelectionWord, StackJoinGoal,
    StackRemoveGoal,
};
use core_node_api::{DaemonFailure, DaemonFailureKind};
use daemon_config::daemon_interface::robot_leases::RobotKind;
use daemon_config::daemon_interface::{
    DaemonInterface, DaemonScope, RobotMembershipMember, RobotMembershipScope, ScopedOption,
};
use peppylib::messaging::{
    Failure, GoalContext, NonEmptyPayload, PendingGoal, ServiceAnswer, ServiceRequestContext,
    VerifiedCaller,
};
use peppylib::types::Payload;
use serde::{Deserialize, Serialize};
use stack_goal::{
    DEFAULT_BUDGETS, DaemonRoute, RunningStackGoal, SendError, StackGoalEnd, StackGoalEvent,
    StackGoalOutcome,
};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;

const ROBOT_MEMBERSHIP: DaemonInterface = DaemonInterface::RobotMembership;

/// How long `list` waits for the presence of the live machines.
const PRESENCE_TIMEOUT: Duration = Duration::from_secs(5);

/// The reason of the `busy` refusal of a `join` while another `join` of
/// `robot_membership:v1` runs on this daemon.
const JOIN_BUSY_REASON: &str =
    "a `join` of robot_membership:v1 is in progress on this daemon; wait for it to finish";

/// The locks of the provider.
#[derive(Default)]
pub(super) struct RobotMembership {
    /// Held by a `join` from before it reads the stack until the daemon's
    /// work on it ends.
    joins: Arc<Mutex<()>>,
    /// Held by a `join` from before it reads the stack until the daemon
    /// admitted or refused its stack goal. `list` waits for it.
    admission: Arc<Mutex<()>>,
}

/// The goal of `join`: a copy as a join adds it. Each item of
/// `launch_copies` has this form, and each item of `copies` has it with its
/// `running`.
#[derive(Deserialize, Serialize)]
struct JoinInput {
    name: String,
    option: String,
    core_node: String,
    features: Vec<FeatureInput>,
}

/// One feature of a `join` goal: the feature and the machine of its part.
#[derive(Deserialize, Serialize, Clone)]
struct FeatureInput {
    feature: String,
    core_node: String,
}

/// The goal of `remove`.
#[derive(Deserialize)]
struct RemoveInput {
    name: String,
}

/// The response of `list`.
#[derive(Serialize)]
struct ListResponse {
    options: Vec<ListedOption>,
    core_nodes: Vec<String>,
    this_core_node: String,
    copies: Vec<ListedCopy>,
    max_copies: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    change: Option<ListedChange>,
    launcher: String,
    launch_copies: Vec<JoinInput>,
}

#[derive(Serialize)]
struct ListedOption {
    option: String,
    model: String,
    description: String,
    features: Vec<ListedFeature>,
    kind: RobotKind,
}

#[derive(Serialize)]
struct ListedFeature {
    feature: String,
    description: String,
}

#[derive(Serialize)]
struct ListedCopy {
    #[serde(flatten)]
    copy: JoinInput,
    running: bool,
}

#[derive(Serialize)]
struct ListedChange {
    action: String,
    name: String,
    option: String,
}

/// One feedback of `join` and `remove`.
#[derive(Serialize)]
struct ChangeLine {
    step: String,
    line: String,
}

/// The sender that the check of the session verified, with its
/// `robot_membership` scope, or the refusal of a sender with none.
fn scope_of(
    ctx: &InterfaceContext,
    verified: Option<&VerifiedCaller>,
) -> Result<(ClientRef, RobotMembershipScope), Failure> {
    let (caller, sender) = ctx.caller(verified)?;
    match caller.scope(ROBOT_MEMBERSHIP) {
        Some(DaemonScope::RobotMembership(scope)) => Ok((sender, scope.clone())),
        _ => Err(failure(
            PeppyFailureKind::Rejected,
            format!(
                "{sender} has no daemon target of robot_membership:v1 with a scope; a launcher \
                 gives the scope of each daemon target in the `daemon_scopes` of the instance, \
                 keyed by the `link_id` of the target"
            ),
        )),
    }
}

/// Answers one request of `list`.
pub(super) async fn answer(
    ctx: &InterfaceContext,
    member: &str,
    request: &ServiceRequestContext,
) -> ServiceAnswer {
    if RobotMembershipMember::named(member) != Some(RobotMembershipMember::List) {
        return Err(failure(
            PeppyFailureKind::Rejected,
            format!("`{member}` is not a service of robot_membership:v1"),
        ));
    }
    let (_, scope) = scope_of(ctx, request.verified_caller())?;
    drop(ctx.robot_membership.admission.lock().await);
    let machines = ctx.live_machines(PRESENCE_TIMEOUT).await?;
    let fleet = super::robot_leases::fleet_listing(ctx);
    let (copies, launch_copies, change) = {
        let active = ctx.slice_ownership.active.lock();
        let (copies, launch_copies) = active
            .as_ref()
            .map(|active| (active.copies(), active.launch_copies()))
            .unwrap_or_default();
        (
            copies,
            launch_copies,
            ctx.slice_ownership.stack.copy_change(),
        )
    };
    let kind_of = |option: &str| {
        fleet
            .options
            .iter()
            .find(|listed| listed.option == option)
            .map_or(RobotKind::Physical, |listed| listed.kind)
    };
    let runs = |name: &str| {
        fleet.robots.iter().any(|robot| {
            robot.name == name
                && robot.software
                    == daemon_config::daemon_interface::robot_leases::SoftwareState::Running
        })
    };
    let response = ListResponse {
        options: scope
            .options()
            .iter()
            .map(|entry| ListedOption {
                option: entry.option().as_str().to_owned(),
                model: entry.model().as_str().to_owned(),
                description: entry.description().as_str().to_owned(),
                features: entry
                    .features()
                    .iter()
                    .map(|feature| ListedFeature {
                        feature: feature.feature().as_str().to_owned(),
                        description: feature.description().as_str().to_owned(),
                    })
                    .collect(),
                kind: kind_of(entry.option().as_str()),
            })
            .collect(),
        core_nodes: machines,
        this_core_node: ctx.core_node.clone(),
        copies: scoped_copies(&scope, &copies)
            .into_iter()
            .map(|copy| listed_copy(&scope, copy, runs(copy.name.as_str())))
            .collect(),
        max_copies: scope.max_copies().get(),
        change: change
            .filter(|change| in_scope(&scope, &change.option))
            .map(|change| ListedChange {
                action: change.action.as_str().to_owned(),
                name: change.name.as_str().to_owned(),
                option: change.option,
            }),
        launcher: fleet.launcher,
        launch_copies: scoped_copies(&scope, &launch_copies)
            .into_iter()
            .map(|copy| join_input(&scope, copy))
            .collect(),
    };
    ctx.encode(ROBOT_MEMBERSHIP, member, Side::Response, &response)
}

/// Runs one goal of `join` or `remove`.
pub(super) async fn run_goal(ctx: &InterfaceContext, member: &str, pending: PendingGoal) {
    let (sender, scope) = match scope_of(ctx, pending.verified_caller()) {
        Ok(scoped) => scoped,
        Err(refusal) => return refuse(pending, refusal).await,
    };
    match RobotMembershipMember::named(member) {
        Some(RobotMembershipMember::Join) => join(ctx, &scope, sender, pending).await,
        Some(RobotMembershipMember::Remove) => remove(ctx, &scope, sender, pending).await,
        _ => {
            let refusal = failure(
                PeppyFailureKind::Rejected,
                format!("`{member}` is not an action of robot_membership:v1"),
            );
            refuse(pending, refusal).await;
        }
    }
}

/// The route of a stack goal from this daemon to itself.
fn own_route(ctx: &InterfaceContext) -> DaemonRoute<'_> {
    DaemonRoute {
        messenger: &ctx.messenger,
        caller_core_node: &ctx.core_node,
        caller_instance_id: &ctx.instance_id,
        daemon_core_node: &ctx.core_node,
    }
}

fn goal_client(sender: &ClientRef) -> GoalClient {
    GoalClient {
        core_node: sender.core_node.clone(),
        instance_id: sender.instance_id.clone(),
    }
}

async fn join(
    ctx: &InterfaceContext,
    scope: &RobotMembershipScope,
    sender: ClientRef,
    pending: PendingGoal,
) {
    let name = RobotMembershipMember::Join.name();
    let input: JoinInput =
        match ctx.decode(ROBOT_MEMBERSHIP, name, Side::Goal, pending.request_bytes()) {
            Ok(input) => input,
            Err(refusal) => return refuse(pending, refusal).await,
        };
    let (copy, words) = match typed_join(scope, &ctx.core_node, &input) {
        Ok(join) => join,
        Err(reason) => return refuse(pending, failure(PeppyFailureKind::Rejected, reason)).await,
    };
    let Ok(lock) = Arc::clone(&ctx.robot_membership.joins).try_lock_owned() else {
        return refuse(pending, failure(DaemonFailureKind::Busy, JOIN_BUSY_REASON)).await;
    };
    let admission = Arc::clone(&ctx.robot_membership.admission)
        .lock_owned()
        .await;
    let change = Change::Addition {
        name: copy.name.clone(),
        option: copy.option.clone(),
    };
    let copies = ctx
        .slice_ownership
        .active
        .lock()
        .as_ref()
        .map(|active| active.copies())
        .unwrap_or_default();
    if let Err(reason) = refuse_join(scope, &copies, &copy.name) {
        return refuse(
            pending,
            failure(PeppyFailureKind::PreconditionFailed, reason),
        )
        .await;
    }
    let goal = StackJoinGoal {
        selections: words,
        client: Some(goal_client(&sender)),
        ..StackJoinGoal::new(copy, DEFAULT_BUDGETS)
    };
    let sent = stack_goal::send(own_route(ctx), &goal).await;
    drop(admission);
    run_change(ctx, pending, &change, sent).await;
    drop(lock);
}

async fn remove(
    ctx: &InterfaceContext,
    scope: &RobotMembershipScope,
    sender: ClientRef,
    pending: PendingGoal,
) {
    let name = RobotMembershipMember::Remove.name();
    let input: RemoveInput =
        match ctx.decode(ROBOT_MEMBERSHIP, name, Side::Goal, pending.request_bytes()) {
            Ok(input) => input,
            Err(refusal) => return refuse(pending, refusal).await,
        };
    let copy_name = match CopyName::new(input.name.as_str()) {
        Ok(copy_name) => copy_name,
        Err(error) => {
            return refuse(
                pending,
                failure(PeppyFailureKind::Rejected, error.to_string()),
            )
            .await;
        }
    };
    let copies = ctx
        .slice_ownership
        .active
        .lock()
        .as_ref()
        .map(|active| active.copies())
        .unwrap_or_default();
    if let Err(reason) = refuse_removal(scope, &copies, &copy_name) {
        return refuse(pending, failure(PeppyFailureKind::Rejected, reason)).await;
    }
    let change = Change::Removal {
        name: copy_name.clone(),
    };
    let goal = StackRemoveGoal {
        client: Some(goal_client(&sender)),
        ..StackRemoveGoal::new(copy_name)
    };
    let sent = stack_goal::send(own_route(ctx), &goal).await;
    run_change(ctx, pending, &change, sent).await;
}

/// Answers the goal of `change` with the end of its stack goal: refused
/// when the daemon refused the stack goal, else accepted and followed to
/// the end of the stack change.
async fn run_change(
    ctx: &InterfaceContext,
    pending: PendingGoal,
    change: &Change,
    sent: Result<RunningStackGoal, SendError>,
) {
    let mut running = match sent {
        Ok(running) => running,
        Err(SendError::Refused { failure, .. }) => {
            return refuse(pending, change.refused(&failure)).await;
        }
        Err(other) => {
            let refusal = failure(
                PeppyFailureKind::AttemptFailed,
                format!("the daemon did not take {}: {other}", change.subject()),
            );
            return refuse(pending, refusal).await;
        }
    };
    let goal = match pending.accept(Payload::new()).await {
        Ok(goal) => goal,
        Err(error) => {
            tracing::debug!(%error, "the goal of {} was not accepted", change.subject());
            undo_unaccepted(&mut running, change).await;
            return;
        }
    };
    let log_path = running.log_path().to_owned();
    let launch = LaunchFeedbackStep::LauncherStep.phase_label();
    publish_line(ctx, &goal, launch, change.accepted()).await;
    let mut cancel_sent = false;
    let ended = loop {
        tokio::select! {
            biased;
            () = goal.cancel_signal(), if !cancel_sent => {
                cancel_sent = true;
                if let Err(error) = running.cancel().await {
                    tracing::warn!(%error, "{} was not cancelled", change.subject());
                }
                publish_line(ctx, &goal, launch, change.cancelling()).await;
            }
            event = running.next() => match event {
                Ok(StackGoalEvent::Output(output)) => {
                    publish_line(ctx, &goal, output.step.phase_label(), output.line).await;
                }
                Ok(StackGoalEvent::Ended(end)) => break Ok(end),
                Err(error) => break Err(error),
            },
        }
    };
    match ended {
        Ok(end) => finish(&goal, change, &end, &log_path).await,
        Err(error) => {
            let message = change.failed(&error.to_string(), &[log_path.as_path()]);
            end_goal(goal.fail(failure(PeppyFailureKind::AttemptFailed, message))).await;
        }
    }
}

/// Cancels the stack change of a goal that its client did not take, and
/// follows the change to the end that the daemon gives it, as the daemon
/// undoes it: a `join` holds its lock until then.
async fn undo_unaccepted(running: &mut RunningStackGoal, change: &Change) {
    if let Err(error) = running.cancel().await {
        tracing::warn!(%error, "{} was not cancelled", change.subject());
    }
    loop {
        match running.next().await {
            Ok(StackGoalEvent::Output(_)) => {}
            Ok(StackGoalEvent::Ended(end)) => {
                tracing::debug!(outcome = ?end.outcome, "{} of a goal that was not accepted ended", change.subject());
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "the end of {} of a goal that was not accepted is unknown", change.subject());
                return;
            }
        }
    }
}

/// Ends the goal of `change` as its stack change ended with `end`.
async fn finish(goal: &GoalContext, change: &Change, end: &StackGoalEnd, log_path: &Path) {
    match &end.outcome {
        StackGoalOutcome::Completed(_) => end_goal(goal.complete(Payload::new())).await,
        StackGoalOutcome::Failed(daemon_failure) => {
            let mut logs = vec![log_path];
            logs.extend(end.node_logs.failed());
            let message = change.failed(&daemon_failure.message, &logs);
            end_goal(goal.fail(Failure::new(daemon_failure.kind, message))).await;
        }
        StackGoalOutcome::Cancelled { reason } => {
            let reason = format!("{} was {reason}", change.subject());
            end_goal(goal.complete_cancelled(reason, Payload::new())).await;
        }
    }
}

/// Publishes one line of a stack change as a feedback of its goal.
async fn publish_line(ctx: &InterfaceContext, goal: &GoalContext, step: &str, line: String) {
    let feedback = ChangeLine {
        step: step.to_owned(),
        line,
    };
    let encoded = ctx
        .encode(
            ROBOT_MEMBERSHIP,
            RobotMembershipMember::Join.name(),
            Side::Feedback,
            &feedback,
        )
        .map_err(|error| error.to_string())
        .and_then(|payload| NonEmptyPayload::try_new(payload).map_err(|error| error.to_string()));
    let published = match encoded {
        Ok(payload) => goal
            .publish_feedback(payload)
            .await
            .map_err(|error| error.to_string()),
        Err(error) => Err(error),
    };
    if let Err(error) = published {
        tracing::debug!(%error, "a line of a stack change was not published");
    }
}

/// What a change of the provider does, as every message about it names it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Change {
    /// A copy `name` of `option` joins the stack.
    Addition { name: CopyName, option: Name },
    /// Copy `name` leaves the stack.
    Removal { name: CopyName },
}

impl Change {
    /// "the addition of bravo", "the removal of bravo".
    fn subject(&self) -> String {
        match self {
            Self::Addition { name, .. } => format!("the addition of {name}"),
            Self::Removal { name } => format!("the removal of {name}"),
        }
    }

    /// The first line of a change the daemon accepted.
    fn accepted(&self) -> String {
        match self {
            Self::Addition { option, .. } => {
                format!("the daemon accepted {} ({option})", self.subject())
            }
            Self::Removal { .. } => format!("the daemon accepted {}", self.subject()),
        }
    }

    /// The line of a cancel the client asked for.
    fn cancelling(&self) -> String {
        format!(
            "the client cancelled {}; the daemon stops it and undoes it",
            self.subject()
        )
    }

    /// The message of a change that failed for `error`, with the log files
    /// that say more. An `error` that ends its own sentence keeps one period.
    fn failed(&self, error: &str, logs: &[&Path]) -> String {
        let logs: Vec<String> = logs.iter().map(|log| log.display().to_string()).collect();
        format!(
            "{} failed: {}. Log files: {}",
            self.subject(),
            error.trim_end_matches('.'),
            logs.join(", ")
        )
    }

    /// The refusal of a change whose stack goal the daemon refused.
    fn refused(&self, refusal: &DaemonFailure) -> Failure {
        Failure::new(
            refusal.kind,
            format!("the daemon refused {}: {}", self.subject(), refusal.message),
        )
    }
}

/// The scoped option `option` names.
fn scoped_option<'a>(
    scope: &'a RobotMembershipScope,
    option: &str,
) -> Result<&'a ScopedOption, String> {
    scope.option(option).ok_or_else(|| {
        format!(
            "`{option}` is not an option of this scope; it adds copies of {}",
            options_phrase(scope)
        )
    })
}

/// The `join` goal as the daemon takes it: the copy on its machine, and the
/// word of each feature on the machine of its part. A machine equal to this
/// one, `this`, is the coordinator, and a feature on the copy's machine
/// names no machine, so its part runs with the copy's base.
fn typed_join(
    scope: &RobotMembershipScope,
    this: &str,
    input: &JoinInput,
) -> Result<(CopyReference, Vec<SelectionWord>), String> {
    let name = CopyName::new(input.name.as_str()).map_err(|error| error.to_string())?;
    let entry = scoped_option(scope, &input.option)?;
    let machine = |value: &str| {
        CoreNodeName::new(value).map_err(|error| format!("machine `{value}` {error}"))
    };
    let copy_machine = machine(&input.core_node)?;
    let target_of = |machine: CoreNodeName| {
        if machine.as_str() == this {
            CoreNodeTarget::Coordinator
        } else {
            CoreNodeTarget::Named(machine)
        }
    };
    let mut words = Vec::with_capacity(input.features.len());
    let mut named: Vec<&str> = Vec::with_capacity(input.features.len());
    for item in &input.features {
        let Some(feature) = entry.feature(&item.feature) else {
            return Err(unknown_feature(entry, &item.feature));
        };
        if named.contains(&item.feature.as_str()) {
            return Err(format!(
                "`features` names feature `{}` twice; name each feature once",
                item.feature
            ));
        }
        named.push(item.feature.as_str());
        let part_machine = machine(&item.core_node)?;
        let core_node = (part_machine != copy_machine).then(|| target_of(part_machine));
        words.push(feature.word(core_node));
    }
    Ok((
        CopyReference {
            option: entry.option().clone(),
            name,
            core_node: target_of(copy_machine),
        },
        words,
    ))
}

/// Whether `option` is an option of the scope.
fn in_scope(scope: &RobotMembershipScope, option: &str) -> bool {
    scope.option(option).is_some()
}

/// The refusal of a feature `feature` that `entry` does not offer.
fn unknown_feature(entry: &ScopedOption, feature: &str) -> String {
    let option = entry.option();
    if entry.features().is_empty() {
        return format!("`{feature}` is not a feature of option `{option}`, which has no feature");
    }
    let features: Vec<&str> = entry
        .features()
        .iter()
        .map(|feature| feature.feature().as_str())
        .collect();
    format!(
        "`{feature}` is not a feature of option `{option}`; its features: {}",
        and_list(&features)
    )
}

/// One copy as `list` gives it, in the shape of a `join` goal, with whether
/// its software runs.
fn listed_copy(scope: &RobotMembershipScope, copy: &CopyInfo, running: bool) -> ListedCopy {
    ListedCopy {
        copy: join_input(scope, copy),
        running,
    }
}

/// One copy in the shape of a `join` goal: its name, its option, its
/// machine, and the features of its option that its selections hold, each
/// with the machine of its part.
fn join_input(scope: &RobotMembershipScope, copy: &CopyInfo) -> JoinInput {
    let features = scope
        .option(&copy.option)
        .map(|entry| entry.features_in(&copy.selections))
        .unwrap_or_default()
        .into_iter()
        .map(|placed| FeatureInput {
            feature: placed.feature.feature().as_str().to_owned(),
            core_node: placed
                .core_node
                .unwrap_or(&copy.core_node)
                .as_str()
                .to_owned(),
        })
        .collect();
    JoinInput {
        name: copy.name.as_str().to_owned(),
        option: copy.option.clone(),
        core_node: copy.core_node.as_str().to_owned(),
        features,
    }
}

/// The copies of the scope's options, in name order.
fn scoped_copies<'a>(scope: &RobotMembershipScope, copies: &'a [CopyInfo]) -> Vec<&'a CopyInfo> {
    let mut scoped: Vec<&CopyInfo> = copies
        .iter()
        .filter(|copy| in_scope(scope, &copy.option))
        .collect();
    scoped.sort_by(|left, right| left.name.as_str().cmp(right.name.as_str()));
    scoped
}

/// A name a copy on the stack has, of any option, and a stack that holds
/// `max_copies` copies of the scope's options or more, are refused.
fn refuse_join(
    scope: &RobotMembershipScope,
    copies: &[CopyInfo],
    name: &CopyName,
) -> Result<(), String> {
    if copies.iter().any(|copy| copy.name == *name) {
        return Err(format!(
            "a copy `{name}` is already on the stack; choose another name"
        ));
    }
    let scoped = scoped_copies(scope, copies);
    if scoped.len() < usize::from(scope.max_copies().get()) {
        return Ok(());
    }
    let names: Vec<&str> = scoped.iter().map(|copy| copy.name.as_str()).collect();
    Err(format!(
        "the stack holds {} of the {} ({}), the most the scope allows; remove one first",
        counted(scoped.len(), "copy", "copies"),
        options_phrase(scope),
        names.join(", ")
    ))
}

/// `remove` removes a copy of a scoped option only.
fn refuse_removal(
    scope: &RobotMembershipScope,
    copies: &[CopyInfo],
    name: &CopyName,
) -> Result<(), String> {
    if scoped_copies(scope, copies)
        .iter()
        .any(|copy| copy.name == *name)
    {
        return Ok(());
    }
    Err(format!(
        "no copy `{name}` of the {} on the stack",
        options_phrase(scope)
    ))
}

/// The scope's options as a message names them: "option so101_sim",
/// "options openarm_sim and so101_sim".
fn options_phrase(scope: &RobotMembershipScope) -> String {
    let options: Vec<&str> = scope
        .options()
        .iter()
        .map(|entry| entry.option().as_str())
        .collect();
    let noun = if options.len() == 1 {
        "option"
    } else {
        "options"
    };
    format!("{noun} {}", and_list(&options))
}

/// `items` as a sentence lists them: "a", "a and b", "a, b and c".
fn and_list(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_owned(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// "1 copy", "4 copies".
fn counted(count: usize, one: &str, many: &str) -> String {
    format!("{count} {}", if count == 1 { one } else { many })
}

#[cfg(test)]
mod tests;
