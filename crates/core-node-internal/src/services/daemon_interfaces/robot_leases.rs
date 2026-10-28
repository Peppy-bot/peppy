//! The members of `robot_leases:v1`, as the daemon serves them: the fleet
//! listing and its changes, the pinned documents, the leases, the move check
//! and the halt. Every rule of the leases and the halt is in the book of the
//! fleet ([`crate::services::fleet::FleetBook`]); this module reads each
//! call, checks it against the scope of its sender, and runs it on the book.

use super::codecs::Side;
use super::{Caller, InterfaceContext, end_goal, refuse};
use crate::services::fleet::{ClientRef, FleetBook, Following, Refusal};
use crate::services::response::failure;
use crate::services::stack::fleet_listing::pinned_document;
use config::failure_kind::PeppyFailureKind;
use daemon_config::daemon_interface::robot_leases::{
    ChangeKind, CheckMoveRequest, CheckMoveResponse, ContractRequest, ContractResponse,
    FleetListing, MoveTarget, OfferRequest, RobotLeasesMember, RobotRequest, TakeRequest,
};
use daemon_config::daemon_interface::{DaemonInterface, DaemonScope};
use peppylib::messaging::{
    Failure, GoalContext, NonEmptyPayload, PendingGoal, ServiceAnswer, ServiceRequestContext,
};
use peppylib::types::Payload;
use serde::Serialize;

const ROBOT_LEASES: DaemonInterface = DaemonInterface::RobotLeases;

/// Whether `caller` may call `member`: the command line calls every member,
/// `check_move` answers every sender, `changes` answers each MCP server of
/// peppy, and an instance calls what the scope of its daemon target of
/// `robot_leases:v1` gives.
fn authorize(
    caller: &Caller,
    member: RobotLeasesMember,
    sender: &ClientRef,
) -> Result<(), Failure> {
    if member == RobotLeasesMember::CheckMove || matches!(caller, Caller::CommandLine) {
        return Ok(());
    }
    if member == RobotLeasesMember::Changes && caller.is_mcp_server() {
        return Ok(());
    }
    if let Some(DaemonScope::RobotLeases(scope)) = caller.scope(ROBOT_LEASES)
        && scope.allows(member)
    {
        return Ok(());
    }
    Err(failure(
        PeppyFailureKind::Rejected,
        format!(
            "{sender} has no daemon target of robot_leases:v1 whose scope gives `{}`; a launcher \
             gives the scope of each daemon target in the `daemon_scopes` of the instance, keyed \
             by the `link_id` of the target",
            member.name()
        ),
    ))
}

/// Answers one request of a service of `robot_leases:v1`.
pub(super) async fn answer(
    ctx: &InterfaceContext,
    member: &str,
    request: &ServiceRequestContext,
) -> ServiceAnswer {
    let Some(member) = RobotLeasesMember::named(member) else {
        return Err(failure(
            PeppyFailureKind::Rejected,
            format!("`{member}` is not a member of robot_leases:v1"),
        ));
    };
    let (caller, sender) = ctx.caller(request.verified_caller())?;
    authorize(&caller, member, &sender)?;
    let bytes = request.message().payload_bytes();
    let name = member.name();
    match member {
        RobotLeasesMember::List => {
            let listing = fleet_listing(ctx);
            ctx.encode(ROBOT_LEASES, name, Side::Response, &listing)
        }
        RobotLeasesMember::Contract => {
            let request: ContractRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            let document = pinned_document(
                &ctx.node_stack,
                &request.name,
                &request.tag,
                &request.sha256,
            )
            .ok_or_else(|| {
                failure(
                    PeppyFailureKind::Rejected,
                    format!(
                        "this daemon holds no document of contract `{}:{}` at sha256 `{}`",
                        request.name, request.tag, request.sha256
                    ),
                )
            })?;
            ctx.encode(
                ROBOT_LEASES,
                name,
                Side::Response,
                &ContractResponse { document },
            )
        }
        RobotLeasesMember::Take => {
            let request: TakeRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            take(ctx, &sender, &request).map_err(Refusal::into_failure)?;
            Ok(Payload::new())
        }
        RobotLeasesMember::Release => {
            let request: RobotRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            ctx.slice_ownership
                .fleet
                .with(|book| book.release(&sender, &request.robot))
                .map_err(Refusal::into_failure)?;
            Ok(Payload::new())
        }
        RobotLeasesMember::Offer => {
            let request: OfferRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            offer(ctx, &sender, &request).map_err(Refusal::into_failure)?;
            Ok(Payload::new())
        }
        RobotLeasesMember::Open => {
            let request: RobotRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            ctx.slice_ownership
                .fleet
                .with(|book| book.open(&sender, &request.robot))
                .map_err(Refusal::into_failure)?;
            Ok(Payload::new())
        }
        RobotLeasesMember::Close => {
            let request: RobotRequest = ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            ctx.slice_ownership
                .fleet
                .with(|book| book.close(&sender, &request.robot))
                .map_err(Refusal::into_failure)?;
            Ok(Payload::new())
        }
        RobotLeasesMember::CheckMove => {
            let request: CheckMoveRequest =
                ctx.decode(ROBOT_LEASES, name, Side::Request, &bytes)?;
            let target = MoveTarget::try_from(request)
                .map_err(|reason| failure(PeppyFailureKind::Rejected, reason))?;
            let response = check_move(ctx, &sender, &target);
            ctx.encode(ROBOT_LEASES, name, Side::Response, &response)
        }
        RobotLeasesMember::Halt => {
            ctx.slice_ownership
                .fleet
                .halt(&sender.to_string())
                .map_err(|error| {
                    failure(
                        PeppyFailureKind::AttemptFailed,
                        format!(
                            "the stack is halted, but the daemon did not write the halt to \
                             disk, so the halt ends at a restart of the daemon; a next halt \
                             writes it again: {error}"
                        ),
                    )
                })?;
            Ok(Payload::new())
        }
        RobotLeasesMember::ClearHalt => {
            ctx.slice_ownership.fleet.clear_halt().map_err(|error| {
                failure(
                    PeppyFailureKind::AttemptFailed,
                    format!("the halt was not removed from disk: {error}"),
                )
            })?;
            Ok(Payload::new())
        }
        RobotLeasesMember::Changes | RobotLeasesMember::Lease => Err(failure(
            PeppyFailureKind::Rejected,
            format!("`{name}` of robot_leases:v1 is an action"),
        )),
    }
}

/// The fleet listing now.
pub(super) fn fleet_listing(ctx: &InterfaceContext) -> FleetListing {
    ctx.slice_ownership.fleet.with(|book| book.listing())
}

/// `take`: a robot on the stack, or one whose lease the book holds (a lease
/// stays when its robot leaves the stack).
fn take(ctx: &InterfaceContext, sender: &ClientRef, request: &TakeRequest) -> Result<(), Refusal> {
    let active = ctx.slice_ownership.active.lock();
    let on_stack = active
        .as_ref()
        .is_some_and(|active| active.holds_copy(&request.robot));
    ctx.slice_ownership.fleet.with(|book| {
        if !on_stack && book.lease_of(&request.robot).is_none() {
            return Err(Refusal::rejected(format!(
                "no robot `{}` is on the stack",
                request.robot
            )));
        }
        book.take(sender, &request.robot, &request.name)
    })
}

/// `offer`: to an instance of the launch, named by its id.
fn offer(
    ctx: &InterfaceContext,
    sender: &ClientRef,
    request: &OfferRequest,
) -> Result<(), Refusal> {
    let active = ctx.slice_ownership.active.lock();
    let machine = active
        .as_ref()
        .and_then(|active| active.machine_of(&request.receiver))
        .ok_or_else(|| {
            Refusal::rejected(format!(
                "no instance `{}` is on the stack; an offer names the instance of a node of the \
                 stack",
                request.receiver
            ))
        })?;
    let receiver = ClientRef::new(machine, request.receiver.clone());
    ctx.slice_ownership
        .fleet
        .with(|book| book.offer(sender, &request.robots, &receiver))
}

/// `check_move`: the robot of the instance that the sender calls, or the
/// robot that it names.
fn check_move(
    ctx: &InterfaceContext,
    sender: &ClientRef,
    target: &MoveTarget,
) -> CheckMoveResponse {
    let active = ctx.slice_ownership.active.lock();
    let robot = match target {
        MoveTarget::Robot(robot) => Some(robot.clone()),
        MoveTarget::Instance(instance) => active
            .as_ref()
            .and_then(|active| active.robot_of(instance))
            .map(|copy| copy.as_str().to_owned()),
    };
    ctx.slice_ownership
        .fleet
        .with(|book| book.check_move(sender, robot.as_deref()))
}

/// Runs one goal of an action of `robot_leases:v1`.
pub(super) async fn run_goal(ctx: &InterfaceContext, member: &str, pending: PendingGoal) {
    let (caller, sender) = match ctx.caller(pending.verified_caller()) {
        Ok(verified) => verified,
        Err(refusal) => {
            refuse(pending, refusal).await;
            return;
        }
    };
    let member = match RobotLeasesMember::named(member) {
        Some(member @ (RobotLeasesMember::Changes | RobotLeasesMember::Lease)) => member,
        _ => {
            let refusal = failure(
                PeppyFailureKind::Rejected,
                format!("`{member}` is not an action of robot_leases:v1"),
            );
            refuse(pending, refusal).await;
            return;
        }
    };
    if let Err(refusal) = authorize(&caller, member, &sender) {
        refuse(pending, refusal).await;
        return;
    }
    match member {
        RobotLeasesMember::Changes => follow_changes(ctx, pending).await,
        _ => hold_leases(ctx, sender, pending).await,
    }
}

/// Publishes `feedback` on `goal`, encoded as the feedback of `member`.
async fn publish<T: serde::Serialize>(
    ctx: &InterfaceContext,
    goal: &GoalContext,
    member: &str,
    feedback: &T,
) -> Result<(), String> {
    let payload = ctx
        .encode(ROBOT_LEASES, member, Side::Feedback, feedback)
        .map_err(|error| error.to_string())?;
    let payload = NonEmptyPayload::try_new(payload).map_err(|error| error.to_string())?;
    goal.publish_feedback(payload)
        .await
        .map_err(|error| error.to_string())
}

/// One feedback of `changes`, as the daemon publishes it: the listing that
/// the book gave with the change.
#[derive(Serialize)]
struct ChangeFeedback<'a> {
    change: ChangeKind,
    robot: &'a str,
    #[serde(with = "daemon_config::daemon_interface::robot_leases::decimal_u64")]
    revision: u64,
    listing: &'a FleetListing,
}

/// `changes`: the listing, then one feedback per change with the revision
/// and the listing that the book gave with it, until the client cancels the
/// goal or is gone.
async fn follow_changes(ctx: &InterfaceContext, pending: PendingGoal) {
    // The book gives the first listing and the changes after it in one step,
    // so no change between the two is lost.
    let Following {
        revision,
        listing,
        mut changes,
    } = ctx.slice_ownership.fleet.with(FleetBook::follow);
    let goal = match pending.accept(Payload::new()).await {
        Ok(goal) => goal,
        Err(error) => {
            tracing::debug!(%error, "a `changes` goal was not accepted");
            return;
        }
    };
    let name = RobotLeasesMember::Changes.name();
    let first = ChangeFeedback {
        change: ChangeKind::Listing,
        robot: "",
        revision,
        listing: &listing,
    };
    if let Err(error) = publish(ctx, &goal, name, &first).await {
        tracing::debug!(%error, "the listing of the fleet was not published");
        return;
    }
    loop {
        let change = tokio::select! {
            biased;
            () = goal.cancel_signal() => break,
            change = changes.recv() => match change {
                Some(change) => change,
                None => break,
            },
        };
        let feedback = ChangeFeedback {
            change: change.kind,
            robot: &change.robot,
            revision: change.revision,
            listing: &change.listing,
        };
        if let Err(error) = publish(ctx, &goal, name, &feedback).await {
            tracing::debug!(%error, "a change of the fleet was not published");
            return;
        }
    }
    end_goal(goal.complete_cancelled(
        "the client of the changes cancelled the goal",
        Payload::new(),
    ))
    .await;
}

/// `lease`: while the goal runs, its sender holds leases, and the goal gives
/// each event of its leases. When the goal ends, every lease of the sender
/// ends or goes back to its giver.
async fn hold_leases(ctx: &InterfaceContext, client: ClientRef, pending: PendingGoal) {
    let generation = pending.sender().generation.to_string();
    let holding = match ctx
        .slice_ownership
        .fleet
        .with(|book| book.begin_holding(&client, &generation))
    {
        Ok(holding) => holding,
        Err(refusal) => {
            refuse(pending, refusal.into_failure()).await;
            return;
        }
    };
    let goal = match pending.accept(Payload::new()).await {
        Ok(goal) => goal,
        Err(error) => {
            tracing::debug!(%error, "a `lease` goal was not accepted");
            ctx.slice_ownership.fleet.with(|book| {
                book.end_holding(&client, &generation, "its lease goal was not accepted")
            });
            return;
        }
    };
    let mut events = holding.events;
    let name = RobotLeasesMember::Lease.name();
    let reason = loop {
        tokio::select! {
            biased;
            () = goal.cancel_signal() => break "the holder cancelled its lease goal",
            () = holding.ended.cancelled() => {
                break "a later process of the holder started its own lease goal";
            }
            event = events.recv() => match event {
                Some(event) => {
                    if let Err(error) = publish(ctx, &goal, name, &event).await {
                        tracing::debug!(%error, "an event of a lease was not published");
                    }
                }
                None => break "the book of the fleet closed",
            },
        }
    };
    let cause = match goal.cancel_cause() {
        Some(cause) => cause.reason().to_owned(),
        None => reason.to_owned(),
    };
    ctx.slice_ownership
        .fleet
        .with(|book| book.end_holding(&client, &generation, &cause));
    end_goal(goal.complete_cancelled(cause, Payload::new())).await;
}
