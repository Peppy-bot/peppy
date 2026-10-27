//! The fleet rules of the server for each member with the mark
//! `moves_robot`.
//!
//! Before each call of such a member, the server asks
//! `robot_leases.check_move` of the daemon that serves the fleet of the
//! stack, and refuses the call with the failure kind and the message of a
//! refusal ([`MoveRules::check`]). An allowed move gives the revision of the
//! fleet that the check read ([`MoveClearance`]). While a goal on such a
//! member runs, the server follows `robot_leases.changes`
//! ([`MoveRules::follow_marked_goal`]), and cancels each of these goals as
//! their client does ([`MarkedSurface`]) at a halt that a change of a later
//! revision than its check gives, and when the follow of the changes ends
//! before the server stops it: a server that cannot see a halt runs no such
//! goal ([`FleetStop`]). The server follows such a goal until it ends, also
//! after the client of its call stopped waiting for it
//! ([`follow_unwaited_goal`]).

#[cfg(test)]
mod tests;

use crate::bridges::TaskSurface;
use crate::serve::ServeError;
use config::failure_kind::FailureKindName;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::daemon_interface::robot_leases::{
    CheckMoveRequest, CheckMoveResponse, FleetChange, RobotLeasesMember,
};
use peppy_codec::consumer::{
    ActionClient, ConsumerError, ConsumerIdentity, GoalHandle, MemberBinding, ServiceClient,
};
use peppy_mcp_runtime::ProviderFailure;
use peppylib::PeppyError;
use peppylib::messaging::{DEFAULT_FEEDBACK_BOUND, MessengerHandle, ProducerRef};
use serde_json::json;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

/// How long the server waits for the daemon to answer a move check, to
/// admit the follow of the changes, and for one result request of a goal
/// that it follows to its end.
const DAEMON_ANSWER_BOUND: Duration = Duration::from_secs(10);

/// How long the server waits before it asks again for the end of a goal
/// that it follows to its end, after a request that reached no provider
/// while the provider still lives.
const RESULT_RETRY_PAUSE: Duration = Duration::from_secs(1);

/// Why the server does not make a call of a member that moves a robot.
#[derive(Debug, Clone, PartialEq)]
pub enum MoveRefused {
    /// The daemon refuses the move: the kind and the message of its
    /// refusal, which the call gives.
    Refused(ProviderFailure),
    /// The daemon did not answer the move check.
    Unanswered(String),
}

/// A move that the daemon allows: the revision of the fleet that the move
/// check read. A goal of the move is cancelled only at a halt of a later
/// revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoveClearance {
    revision: u64,
}

/// Why the server cancels a goal on a member that moves a robot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FleetStop {
    /// The stack is halted, by this client, since the move check of the
    /// goal.
    Halted(String),
    /// The follow of the changes of the fleet ended before the server
    /// stopped it, for this reason, so the server cannot see a halt.
    FollowLost(String),
}

impl fmt::Display for FleetStop {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Halted(by) => write!(f, "the stack is halted (by {by})"),
            Self::FollowLost(reason) => write!(
                f,
                "the follow of the changes of the fleet ended, so no halt can be seen: {reason}"
            ),
        }
    }
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
    /// What the follow of the changes sees.
    view: watch::Sender<FleetView>,
    follow: parking_lot::Mutex<Follow>,
}

/// What the follow of the changes of one generation sees.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FleetView {
    generation: u64,
    state: ViewState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ViewState {
    /// The follow waits for its first listing.
    Waiting,
    /// The last feedback of the follow: its revision, and the client that
    /// halted the stack while it is halted.
    Seen {
        revision: u64,
        halted_by: Option<String>,
    },
    /// The follow ended before the server stopped it, for this reason.
    Lost(String),
}

impl FleetView {
    /// Why a goal that relies on the follow of `generation`, whose move
    /// check read the revision `clearance`, must be cancelled, if it must.
    fn stop_of(&self, generation: u64, clearance: MoveClearance) -> Option<FleetStop> {
        if self.generation != generation {
            return Some(FleetStop::FollowLost(
                "the server follows the changes again for later goals".to_owned(),
            ));
        }
        match &self.state {
            ViewState::Waiting => None,
            ViewState::Seen {
                revision,
                halted_by: Some(by),
            } if *revision > clearance.revision => Some(FleetStop::Halted(by.clone())),
            ViewState::Seen { .. } => None,
            ViewState::Lost(reason) => Some(FleetStop::FollowLost(reason.clone())),
        }
    }
}

/// The goals on members that move a robot that run, and the follow of the
/// changes that runs while one does.
#[derive(Default)]
struct Follow {
    goals: usize,
    /// Counts the follows of the changes: a follow changes the view only
    /// while it is the current one.
    generation: u64,
    /// The stop of the follow that runs; none once the follow ended, so the
    /// next goal starts a follow of its own.
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
        let document = DaemonInterface::RobotLeases.document();
        let check_move = RobotLeasesMember::CheckMove.name();
        let changes = RobotLeasesMember::Changes.name();
        Ok(Self {
            inner: Arc::new(Inner {
                messenger,
                identity,
                fleet,
                check_move: ServiceClient::of_contract(document, check_move)?,
                check_move_binding: MemberBinding::of_contract(document, check_move)?,
                changes: ActionClient::of_contract(document, changes)?,
                changes_binding: MemberBinding::of_contract(document, changes)?,
                view: watch::Sender::new(FleetView {
                    generation: 0,
                    state: ViewState::Waiting,
                }),
                follow: parking_lot::Mutex::default(),
            }),
        })
    }

    /// Asks the daemon whether the server may call a member that moves a
    /// robot on `producer`.
    pub async fn check(&self, producer: &ProducerRef) -> Result<MoveClearance, MoveRefused> {
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
            None => Ok(MoveClearance {
                revision: response.revision,
            }),
            Some(refusal) => Err(MoveRefused::Refused(ProviderFailure::new(
                FailureKindName::from(refusal.failure_kind),
                refusal.message,
            ))),
        }
    }

    /// Notes a goal on a member that moves a robot, which `clearance`
    /// allowed and which runs until the returned guard drops. The server
    /// follows the changes of the fleet while such a goal runs: a goal that
    /// finds no follow running starts one.
    pub fn follow_marked_goal(&self, clearance: MoveClearance) -> MarkedGoal {
        let mut follow = self.inner.follow.lock();
        follow.goals += 1;
        if follow.stop.is_none() {
            follow.generation += 1;
            self.inner.view.send_replace(FleetView {
                generation: follow.generation,
                state: ViewState::Waiting,
            });
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
            view: self.inner.view.subscribe(),
            generation: follow.generation,
            clearance,
        }
    }
}

impl Inner {
    /// Sets the state that the follow of `generation` sees, while that
    /// follow is the current one.
    fn note_view(&self, generation: u64, state: ViewState) {
        let follow = self.follow.lock();
        if follow.generation == generation && follow.goals > 0 {
            self.view.send_replace(FleetView { generation, state });
        }
    }

    /// Notes that the follow of `generation` ended before its stop, for
    /// `reason`: the goals that rely on it are cancelled, and the next goal
    /// starts a follow of its own.
    fn lose_follow(&self, generation: u64, reason: String) {
        tracing::warn!(%reason, "the server lost the follow of the changes of the fleet, so it cancels each goal that moves a robot");
        let mut follow = self.follow.lock();
        if follow.generation != generation {
            return;
        }
        follow.stop = None;
        self.view.send_replace(FleetView {
            generation,
            state: ViewState::Lost(reason),
        });
    }
}

/// Follows the changes of the fleet until `stop`, and notes the revision
/// and the halt that each feedback gives. Each feedback of `changes` gives
/// the listing after its change, the first one the listing at the start of
/// the follow. A follow that ends before `stop` is lost.
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
            return inner.lose_follow(
                generation,
                format!("the follow did not reach the daemon: {error}"),
            );
        }
    };
    if let Some(refusal) = handle.refusal() {
        return inner.lose_follow(
            generation,
            format!("the daemon refuses the follow: {refusal}"),
        );
    }
    let _cancel = handle.cancel_on_drop("the server stopped following the changes of the fleet");
    loop {
        let feedback = tokio::select! {
            () = stop.cancelled() => return,
            feedback = handle.next_feedback() => feedback,
        };
        let change = match feedback {
            Ok(Some(value)) => serde_json::from_value::<FleetChange>(value),
            Ok(None) => {
                return inner.lose_follow(generation, "the daemon ended the follow".to_owned());
            }
            Err(error) => {
                return inner.lose_follow(generation, format!("the follow failed: {error}"));
            }
        };
        match change {
            Ok(change) => inner.note_view(
                generation,
                ViewState::Seen {
                    revision: change.revision,
                    halted_by: change.listing.halted.then_some(change.listing.halted_by),
                },
            ),
            Err(error) => {
                return inner.lose_follow(
                    generation,
                    format!("a change of the fleet does not parse: {error}"),
                );
            }
        }
    }
}

/// A goal on a member that moves a robot, while it runs: the server follows
/// the changes of the fleet until the last such goal drops its guard.
pub struct MarkedGoal {
    rules: MoveRules,
    view: watch::Receiver<FleetView>,
    /// The follow of the changes that the goal relies on.
    generation: u64,
    clearance: MoveClearance,
}

impl MarkedGoal {
    /// Resolves once the server must cancel the goal, with why: a halt
    /// that a change of a later revision than the move check of the goal
    /// gives, or the end of the follow of the changes before its stop.
    pub fn must_stop(&self) -> impl Future<Output = FleetStop> + Send + 'static {
        let mut view = self.view.clone();
        let (generation, clearance) = (self.generation, self.clearance);
        async move {
            let seen = view
                .wait_for(|view| view.stop_of(generation, clearance).is_some())
                .await
                .map(|view| view.stop_of(generation, clearance));
            match seen {
                Ok(Some(stop)) => stop,
                Ok(None) | Err(_) => std::future::pending().await,
            }
        }
    }
}

impl Clone for MarkedGoal {
    /// Another note of the same goal, which keeps the follow of the changes
    /// running until it drops too.
    fn clone(&self) -> Self {
        self.rules.inner.follow.lock().goals += 1;
        Self {
            rules: self.rules.clone(),
            view: self.view.clone(),
            generation: self.generation,
            clearance: self.clearance,
        }
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
        let stop = self.marked.must_stop();
        async move {
            tokio::select! {
                () = client => {}
                stop = stop => tracing::warn!(%stop, "the server cancels a goal that moves a robot"),
            }
        }
    }
}

/// Follows `handle`, a goal on a member that moves a robot whose client
/// stopped waiting for it before it ended, until it ends: the goal runs on,
/// and the server cancels it ([`MarkedGoal::must_stop`]) unless it got its
/// cancel (`cancel_sent`). A result request that gets no answer yet, or that
/// reaches no provider while the provider lives, is asked again; a provider
/// that is gone ends the goal abandoned.
pub(crate) async fn follow_unwaited_goal(
    handle: GoalHandle,
    messenger: MessengerHandle,
    marked: MarkedGoal,
    mut cancel_sent: bool,
) {
    let mut pause = false;
    loop {
        let request = async {
            if pause {
                tokio::time::sleep(RESULT_RETRY_PAUSE).await;
            }
            handle.result(&messenger, DAEMON_ANSWER_BOUND).await
        };
        tokio::select! {
            outcome = request => match outcome {
                Ok(outcome) => {
                    tracing::debug!(goal_id = handle.goal_id(), ?outcome, "a goal that its client stopped waiting for ended");
                    return;
                }
                Err(error) => match result_miss(&error) {
                    Some(ResultMiss::NotEnded) => pause = false,
                    Some(ResultMiss::Unreached) => pause = true,
                    None => {
                        tracing::warn!(%error, goal_id = handle.goal_id(), "the end of a goal that its client stopped waiting for is unknown");
                        return;
                    }
                },
            },
            stop = marked.must_stop(), if !cancel_sent => {
                cancel_sent = true;
                tracing::warn!(%stop, goal_id = handle.goal_id(), "the server cancels a goal that moves a robot");
                if let Err(error) = handle.cancel(&messenger, DAEMON_ANSWER_BOUND).await {
                    tracing::warn!(%error, goal_id = handle.goal_id(), "the cancel of a goal failed; the provider may still run it");
                }
            }
        }
    }
}

/// A result request that gave no end, but after which the goal can still
/// end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResultMiss {
    /// The provider did not answer in time: the goal has not ended yet.
    NotEnded,
    /// The request reached no provider, and the probe of the provider says
    /// that it lives: a provider that is gone gives the goal abandoned
    /// instead.
    Unreached,
}

/// The miss that a result request that failed with `error` is, if it is
/// one; every other error leaves the end of the goal unknown.
fn result_miss(error: &ConsumerError) -> Option<ResultMiss> {
    match error {
        ConsumerError::Messaging(
            PeppyError::ActionResultTimeout { .. } | PeppyError::ServiceTimeout { .. },
        ) => Some(ResultMiss::NotEnded),
        ConsumerError::Messaging(PeppyError::ActionResultUnreachable { .. }) => {
            Some(ResultMiss::Unreached)
        }
        _ => None,
    }
}
