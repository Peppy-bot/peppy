//! The daemon interfaces on the messaging layer. The daemon serves
//! `stack_copies:v1` and `fleet:v1` as the provider of each contract, under
//! its own producer address: every service and action of the compiled-in
//! document of each interface. It reads and writes each message with the
//! run-time codec of the document, the codec that generated nodes match
//! byte for byte, so a generated client, the MCP server of peppy and the
//! command line call these interfaces as they call any contract.
//!
//! The daemon checks each call against the scope of its sender
//! ([`caller::Caller`]): the command line has no scope and calls every
//! member; an instance of the launch calls what the scope of its daemon
//! target of the interface gives; `check_move` answers every sender, and
//! `changes` answers each MCP server of peppy. The sender is the caller that
//! the check of the session of the daemon verified
//! ([`crate::services::credentials`]).
//!
//! The daemon of the coordinator of a launch serves the interfaces of that
//! launch: a daemon that holds the slice of a launch of another machine
//! refuses every call and names the coordinator, whose daemon the daemon
//! targets of the instances of the launch reach.

mod caller;
mod codecs;
mod fleet_members;
mod stack_copies;

pub(crate) use caller::Caller;
use codecs::{Codecs, Side};

use crate::services::federation::SliceOwnership;
use crate::services::fleet::ClientRef;
use crate::services::response::failure;
use config::failure_kind::PeppyFailureKind;
use daemon_config::daemon_interface::DaemonInterface;
use futures::future::select_all;
use node_stack::NodeStack;
use peppylib::MessengerHandle;
use peppylib::messaging::{
    ConcurrentAction, Failure, PendingGoal, SenderTarget, ServiceAnswer, ServiceRequestContext,
    VerifiedCaller,
};
use peppylib::types::Payload;
use peppylib::{CoreNodePresenceMessenger, ServiceMessenger};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// What the provider of the daemon interfaces reads: the daemon's session
/// and identity, its stack, the launch it coordinates and the fleet of that
/// launch, and the codecs of the interfaces.
#[derive(Clone)]
pub(crate) struct InterfaceContext {
    pub(crate) messenger: MessengerHandle,
    pub(crate) core_node: String,
    /// The daemon's per-boot instance id, its producer address beside its
    /// core node.
    pub(crate) instance_id: String,
    pub(crate) node_stack: Arc<NodeStack>,
    pub(crate) slice_ownership: Arc<SliceOwnership>,
    codecs: Arc<Codecs>,
    stack_copies: Arc<stack_copies::StackCopies>,
}

impl InterfaceContext {
    pub(crate) fn new(
        messenger: MessengerHandle,
        core_node: &str,
        instance_id: &str,
        node_stack: Arc<NodeStack>,
        slice_ownership: Arc<SliceOwnership>,
    ) -> Self {
        Self {
            messenger,
            core_node: core_node.to_owned(),
            instance_id: instance_id.to_owned(),
            node_stack,
            slice_ownership,
            codecs: Arc::default(),
            stack_copies: Arc::default(),
        }
    }

    /// The refusal of every call on this daemon while it holds the slice of
    /// a launch that the daemon of another machine coordinates: that daemon
    /// serves the fleet and the copies of the launch, and the daemon targets
    /// of the instances of the launch reach it.
    fn served_by_the_coordinator(&self) -> Option<Failure> {
        let slice = self.slice_ownership.slice()?;
        (slice.coordinator_core_node != self.core_node).then(|| {
            failure(
                PeppyFailureKind::PreconditionFailed,
                format!(
                    "this machine runs a slice of launch `{}`, which `{}` coordinates: call the \
                     daemon of `{}`",
                    slice.launch_id, slice.coordinator_core_node, slice.coordinator_core_node
                ),
            )
        })
    }

    /// The sender of a request or a goal, as the scopes of the launch read
    /// it: the caller that the check of the session verified. A call that no
    /// check verified is refused.
    fn caller(&self, verified: Option<&VerifiedCaller>) -> Result<(Caller, ClientRef), Failure> {
        let verified = crate::services::raw_calls::verified(verified)?;
        Ok((
            Caller::of(&self.slice_ownership, verified),
            ClientRef::new(verified.core_node(), verified.instance_id()),
        ))
    }

    /// Decodes the message of the `side` of `member` of `interface` from
    /// `bytes`; a message that does not decode is `rejected`.
    fn decode<T: DeserializeOwned>(
        &self,
        interface: DaemonInterface,
        member: &str,
        side: Side,
        bytes: &[u8],
    ) -> Result<T, Failure> {
        let codec = self.codecs.codec(interface, member, side)?;
        let value = codec.decode(bytes).map_err(|error| {
            failure(
                PeppyFailureKind::Rejected,
                format!("the {side} of `{member}` does not decode: {error}"),
            )
        })?;
        serde_json::from_value(value).map_err(|error| {
            failure(
                PeppyFailureKind::Rejected,
                format!("the {side} of `{member}` does not parse: {error}"),
            )
        })
    }

    /// Encodes `message`, the `side` of `member` of `interface`.
    fn encode<T: Serialize>(
        &self,
        interface: DaemonInterface,
        member: &str,
        side: Side,
        message: &T,
    ) -> Result<Payload, Failure> {
        let codec = self.codecs.codec(interface, member, side)?;
        let encode_failed = |error: String| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("the {side} of `{member}` does not encode: {error}"),
            )
        };
        let value =
            serde_json::to_value(message).map_err(|error| encode_failed(error.to_string()))?;
        codec
            .encode(&value)
            .map(Payload::from)
            .map_err(|error| encode_failed(error.to_string()))
    }

    /// The machines of the federation now: this machine and every live
    /// daemon of the same workspace or project.
    async fn live_machines(&self, timeout: Duration) -> Result<Vec<String>, Failure> {
        let mut machines = CoreNodePresenceMessenger::live_core_node_names(
            &self.messenger,
            &self.core_node,
            timeout,
        )
        .await
        .map_err(|error| {
            failure(
                PeppyFailureKind::AttemptFailed,
                format!("the live machines of the federation could not be read: {error}"),
            )
        })?;
        machines.insert(self.core_node.clone());
        Ok(machines.into_iter().collect())
    }
}

/// Serves every member of every daemon interface on the messaging layer,
/// and gives the task that runs until a listener ends.
pub(crate) async fn serve(ctx: InterfaceContext) -> crate::Result<JoinHandle<crate::Result<()>>> {
    let mut listeners: Vec<JoinHandle<crate::Result<()>>> = Vec::new();
    for interface in DaemonInterface::ALL {
        let document = interface.document();
        for service in &document.interfaces.services {
            listeners.push(serve_service(&ctx, interface, &service.name).await?);
        }
        for action in &document.interfaces.actions {
            listeners.push(serve_action(&ctx, interface, &action.name).await?);
        }
    }
    Ok(tokio::spawn(async move {
        let mut waiting = listeners;
        while !waiting.is_empty() {
            let (result, _index, rest) = select_all(waiting).await;
            result??;
            waiting = rest;
        }
        Ok(())
    }))
}

/// The identity a member of `interface` is served under.
fn target_of(interface: DaemonInterface) -> crate::Result<SenderTarget> {
    Ok(SenderTarget::contract(interface.name(), interface.tag())?)
}

async fn serve_service(
    ctx: &InterfaceContext,
    interface: DaemonInterface,
    member: &str,
) -> crate::Result<JoinHandle<crate::Result<()>>> {
    let mut endpoint = ServiceMessenger::listen(
        &ctx.messenger,
        &ctx.core_node,
        &ctx.instance_id,
        target_of(interface)?,
        member,
    )
    .await?;
    let ctx = ctx.clone();
    let member = member.to_owned();
    Ok(tokio::spawn(async move {
        loop {
            let ctx = ctx.clone();
            let member = member.clone();
            let handled = endpoint
                .spawn_next_request_handler(move |request| async move {
                    answer_service(&ctx, interface, &member, &request).await
                })
                .await?;
            if handled.is_none() {
                return Ok(());
            }
        }
    }))
}

async fn answer_service(
    ctx: &InterfaceContext,
    interface: DaemonInterface,
    member: &str,
    request: &ServiceRequestContext,
) -> ServiceAnswer {
    if let Some(refusal) = ctx.served_by_the_coordinator() {
        return Err(refusal);
    }
    match interface {
        DaemonInterface::Fleet => fleet_members::answer(ctx, member, request).await,
        DaemonInterface::StackCopies => stack_copies::answer(ctx, member, request).await,
    }
}

async fn serve_action(
    ctx: &InterfaceContext,
    interface: DaemonInterface,
    member: &str,
) -> crate::Result<JoinHandle<crate::Result<()>>> {
    let reports_feedback = interface
        .document()
        .interfaces
        .actions
        .iter()
        .any(|action| action.name == member && action.feedback_topic.is_some());
    let mut action = ConcurrentAction::expose(
        &ctx.messenger,
        &ctx.core_node,
        &ctx.instance_id,
        target_of(interface)?,
        member,
        reports_feedback,
    )
    .await?;
    if interface == DaemonInterface::StackCopies {
        // A stack change goes on through a router restart, as each stack
        // goal of the daemon does.
        action.set_sender_grace(crate::services::DAEMON_SENDER_GRACE);
    }
    let ctx = ctx.clone();
    let member = member.to_owned();
    Ok(tokio::spawn(async move {
        while let Some(pending) = action.recv_next_goal().await? {
            let ctx = ctx.clone();
            let member = member.clone();
            tokio::spawn(async move {
                if let Some(refusal) = ctx.served_by_the_coordinator() {
                    refuse(pending, refusal).await;
                    return;
                }
                match interface {
                    DaemonInterface::Fleet => fleet_members::run_goal(&ctx, &member, pending).await,
                    DaemonInterface::StackCopies => {
                        stack_copies::run_goal(&ctx, &member, pending).await;
                    }
                }
            });
        }
        Ok(())
    }))
}

/// Refuses `pending` with `refusal`, logging a refusal that does not reach
/// its sender.
async fn refuse(pending: PendingGoal, refusal: Failure) {
    if let Err(error) = pending.refuse(refusal).await {
        tracing::debug!(%error, "the refusal of a goal of a daemon interface was not sent");
    }
}

/// Runs `end` on the goal and logs an end that does not reach its client.
async fn end_goal(end: impl Future<Output = peppylib::PeppyResult<()>>) {
    if let Err(error) = end.await {
        tracing::debug!(%error, "the end of a goal of a daemon interface was not sent");
    }
}
