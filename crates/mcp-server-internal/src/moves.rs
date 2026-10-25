//! The fleet rules of the server for each member with the mark
//! `moves_robot`.
//!
//! Before each call of such a member, the server asks `fleet.check_move` of
//! the daemon that serves the fleet of the stack, and refuses the call with
//! the failure kind and the message of a refusal ([`MoveRules::check`]).
//! While a goal on such a member runs, the server follows `fleet.changes`
//! ([`MoveRules::follow_marked_goal`]), and at a halt it cancels each of
//! these goals as their client does ([`MarkedSurface`]). The server follows
//! such a goal until it ends, also after the client of its call stopped
//! waiting for it ([`follow_unwaited_goal`]).

#[cfg(test)]
mod tests;

use crate::bridges::TaskSurface;
use crate::serve::ServeError;
use config::failure_kind::FailureKindName;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::daemon_interface::fleet::{
    CheckMoveRequest, CheckMoveResponse, FleetChange, FleetMember,
};
use peppy_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, MemberBinding, ServiceClient,
};
use peppy_mcp_runtime::ProviderFailure;
use peppylib::PeppyError;
use peppylib::messaging::{DEFAULT_FEEDBACK_BOUND, MessengerHandle, ProducerRef};
use serde_json::json;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How long the server waits for the daemon to answer a move check, to
/// admit the follow of the changes, and for one result request of a goal
/// that it follows to its end.
const DAEMON_ANSWER_BOUND: Duration = Duration::from_secs(10);

/// Why the server does not make a call of a member that moves a robot.
#[derive(Debug, Clone, PartialEq)]
pub enum MoveRefused {
    /// The daemon refuses the move: the kind and the message of its
    /// refusal, which the call gives.
    Refused(ProviderFailure),
    /// The daemon did not answer the move check.
    Unanswered(String),
}

/// The fleet rules of one server. Every entry of the server shares them, so
/// one follow of the changes serves every goal on a member that moves a
/// robot.
#[derive(Clone)]
pub struct MoveRules {
    inner: Arc<Inner>,
}

struct Inner {
    messenger: MessengerHandle,
    identity: ConsumerIdentity,
    /// The daemon that serves the fleet: the producer of the fleet slot of
    /// the server.
    fleet: ProducerRef,
    check_move: ServiceClient,
    check_move_binding: MemberBinding,
    changes: ActionClient,
    changes_binding: MemberBinding,
    /// The client that halted the stack, while the follow of the changes
    /// sees a halt.
    halt: watch::Sender<Option<String>>,
    follow: parking_lot::Mutex<Follow>,
}

/// The goals on members that move a robot that run, and the follow of the
/// changes that runs while one does.
#[derive(Default)]
struct Follow {
    goals: usize,
    /// Counts the follows of the changes: a follow that its stop has not
    /// reached yet changes the halt only while it is the current one.
    generation: u64,
    stop: Option<CancellationToken>,
}

impl MoveRules {
    /// The rules of a server that reaches the daemon of the fleet, `fleet`,
    /// on `messenger` as `identity`.
    pub fn new(
        messenger: MessengerHandle,
        identity: ConsumerIdentity,
        fleet: ProducerRef,
    ) -> Result<Self, ServeError> {
        let document = DaemonInterface::Fleet.document();
        let check_move = FleetMember::CheckMove.name();
        let changes = FleetMember::Changes.name();
        Ok(Self {
            inner: Arc::new(Inner {
                messenger,
                identity,
                fleet,
                check_move: ServiceClient::of_contract(document, check_move)?,
                check_move_binding: MemberBinding::of_contract(document, check_move)?,
                changes: ActionClient::of_contract(document, changes)?,
                changes_binding: MemberBinding::of_contract(document, changes)?,
                halt: watch::Sender::new(None),
                follow: parking_lot::Mutex::default(),
            }),
        })
    }

    /// Asks the daemon whether the server may call a member that moves a
    /// robot on `producer`.
    pub async fn check(&self, producer: &ProducerRef) -> Result<(), MoveRefused> {
        let inner = &self.inner;
        let request = serde_json::to_value(CheckMoveRequest {
            instance: Some(producer.instance_id.clone()),
            robot: None,
        })
        .expect("a check_move request serializes");
        let unanswered = |reason: String| {
            MoveRefused::Unanswered(format!(
                "the daemon did not answer the move check of `{}`: {reason}",
                producer.instance_id
            ))
        };
        let answer = inner
            .check_move
            .call(
                &inner.messenger,
                &inner.identity,
                &inner.check_move_binding,
                &inner.fleet,
                &request,
                DAEMON_ANSWER_BOUND,
            )
            .await
            .map_err(|error| unanswered(error.to_string()))?;
        let response: CheckMoveResponse =
            serde_json::from_value(answer).map_err(|error| unanswered(error.to_string()))?;
        match response.refusal {
            None => Ok(()),
            Some(refusal) => Err(MoveRefused::Refused(ProviderFailure::new(
                FailureKindName::from(refusal.failure_kind),
                refusal.message,
            ))),
        }
    }

    /// Notes a goal on a member that moves a robot, which runs until the
    /// returned guard drops. The server follows the changes of the fleet
    /// while such a goal runs.
    pub fn follow_marked_goal(&self) -> MarkedGoal {
        let mut follow = self.inner.follow.lock();
        follow.goals += 1;
        if follow.goals == 1 {
            // A new follow reads the halt from its first listing: until then
            // the server sees none, as the move check of the goal saw none.
            follow.generation += 1;
            self.inner.halt.send_replace(None);
            let stop = CancellationToken::new();
            follow.stop = Some(stop.clone());
            tokio::spawn(follow_changes(
                Arc::clone(&self.inner),
                follow.generation,
                stop,
            ));
        }
        MarkedGoal {
            rules: self.clone(),
            halt: self.inner.halt.subscribe(),
        }
    }
}

impl Inner {
    /// Sets the halt that the follow of `generation` reads, while that
    /// follow is the current one.
    fn note_halt(&self, generation: u64, halted_by: Option<String>) {
        let follow = self.follow.lock();
        if follow.generation == generation && follow.goals > 0 {
            self.halt.send_replace(halted_by);
        }
    }
}

/// Follows the changes of the fleet until `stop`, and notes each halt that
/// a listing gives. Each feedback of `changes` gives the listing after its
/// change, the first one the listing at the start of the follow.
async fn follow_changes(inner: Arc<Inner>, generation: u64, stop: CancellationToken) {
    let goal = json!({});
    let fired = tokio::select! {
        () = stop.cancelled() => return,
        fired = inner.changes.fire_goal(
            &inner.messenger,
            &inner.identity,
            &inner.changes_binding,
            &inner.fleet,
            &goal,
            DEFAULT_FEEDBACK_BOUND,
            DAEMON_ANSWER_BOUND,
        ) => fired,
    };
    let mut handle = match fired {
        Ok(handle) => handle,
        Err(error) => {
            tracing::warn!(%error, "the server cannot follow the changes of the fleet, so it sees no halt");
            return;
        }
    };
    if let Some(refusal) = handle.refusal() {
        tracing::warn!(%refusal, "the daemon refuses the follow of the changes of the fleet, so the server sees no halt");
        return;
    }
    let _cancel = handle.cancel_on_drop("the server stopped following the changes of the fleet");
    loop {
        let feedback = tokio::select! {
            () = stop.cancelled() => return,
            feedback = handle.next_feedback() => feedback,
        };
        match feedback {
            Ok(Some(value)) => match serde_json::from_value::<FleetChange>(value) {
                Ok(change) => inner.note_halt(
                    generation,
                    change.listing.halted.then_some(change.listing.halted_by),
                ),
                Err(error) => {
                    tracing::warn!(%error, "a change of the fleet does not parse");
                }
            },
            Ok(None) => {
                tracing::warn!("the daemon ended the follow of the changes of the fleet");
                return;
            }
            Err(error) => {
                tracing::warn!(%error, "the follow of the changes of the fleet failed, so the server sees no halt");
                return;
            }
        }
    }
}

/// A goal on a member that moves a robot, while it runs: the server follows
/// the changes of the fleet until the last such goal drops its guard.
pub struct MarkedGoal {
    rules: MoveRules,
    halt: watch::Receiver<Option<String>>,
}

impl MarkedGoal {
    /// Resolves once the server sees a halt of the stack.
    pub fn halted(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut halt = self.halt.clone();
        async move {
            if halt.wait_for(Option::is_some).await.is_err() {
                std::future::pending::<()>().await;
            }
        }
    }
}

impl Clone for MarkedGoal {
    /// Another note of the same goal, which keeps the follow of the changes
    /// running until it drops too.
    fn clone(&self) -> Self {
        self.rules.follow_marked_goal()
    }
}

impl Drop for MarkedGoal {
    fn drop(&mut self) {
        let mut follow = self.rules.inner.follow.lock();
        follow.goals -= 1;
        if follow.goals == 0
            && let Some(stop) = follow.stop.take()
        {
            stop.cancel();
        }
    }
}

/// The surface of a goal on a member that moves a robot: the cancel of its
/// client and a halt of the stack each cancel the goal.
pub(crate) struct MarkedSurface<'a, S> {
    pub(crate) client: &'a S,
    pub(crate) marked: &'a MarkedGoal,
}

impl<S: TaskSurface> TaskSurface for MarkedSurface<'_, S> {
    fn report_feedback(&self, message: String) -> impl Future<Output = ()> + Send {
        self.client.report_feedback(message)
    }

    fn cancel_requested(&self) -> impl Future<Output = ()> + Send {
        let client = self.client.cancel_requested();
        let halted = self.marked.halted();
        async move {
            tokio::select! {
                () = client => {}
                () = halted => {}
            }
        }
    }
}

/// Follows `handle`, a goal on a member that moves a robot whose client
/// stopped waiting for it before it ended, until it ends: the goal runs on,
/// and a halt of the stack cancels it unless it got its cancel
/// (`cancel_sent`).
pub(crate) async fn follow_unwaited_goal(
    handle: GoalHandle,
    messenger: MessengerHandle,
    marked: MarkedGoal,
    mut cancel_sent: bool,
) {
    loop {
        tokio::select! {
            outcome = handle.result(&messenger, DAEMON_ANSWER_BOUND) => match outcome {
                Ok(outcome) => {
                    tracing::debug!(goal_id = handle.goal_id(), ?outcome, "a goal that its client stopped waiting for ended");
                    return;
                }
                Err(error) if no_answer_yet(&error) => {}
                Err(error) => {
                    tracing::warn!(%error, goal_id = handle.goal_id(), "the end of a goal that its client stopped waiting for is unknown");
                    return;
                }
            },
            () = marked.halted(), if !cancel_sent => {
                cancel_sent = true;
                if let Err(error) = handle.cancel(&messenger, DAEMON_ANSWER_BOUND).await {
                    tracing::warn!(%error, goal_id = handle.goal_id(), "the cancel of a goal at a halt failed; the provider may still run it");
                }
            }
        }
    }
}

/// Whether a result request failed only because the goal has not ended yet.
fn no_answer_yet(error: &ConsumerError) -> bool {
    matches!(
        error,
        ConsumerError::Messaging(
            PeppyError::ActionResultTimeout { .. } | PeppyError::ServiceTimeout { .. }
        )
    )
}
