//! What the stack keeps of an instance between its processes: its restart
//! policy, the attempts in a row and the restarts so far, the goal it was
//! first started from, the pairs it held and the registrations its last
//! exit took; the backoff that paces its restarts and the attempts it yields;
//! what an exit records and a respawn starts from; the two kinds of spawn
//! and the run a start belongs to; what a slot keeps of an instance's
//! relations and what a claim and removal did with it; and the health counts
//! the messaging-outage guard reads.

use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use config::node::{ResolvedRestart, RestartDeclaration, RestartOrigin, RestartPolicy};
use core_node_api::encoding::{NodeRunGoal, ObservationTargets};
use core_node_api::{ExitCause, RestartStatus, unix_ms};
use tokio_util::sync::CancellationToken;

use super::pairing::PlannedPair;

/// What the health monitor records on an instance whose process it is about
/// to kill.
#[derive(Debug, Clone, Copy)]
pub struct HealthKill {
    /// How long the process answered no health check: the monitor's bound.
    pub unanswered_for: Duration,
    /// When the process last answered a health check; `None` when it never
    /// did. The run counts up to it.
    pub last_answer: Option<Instant>,
}

/// The restart policy an instance was first started under and the goal it
/// was started from, which its restarts keep together.
#[derive(Debug, Clone)]
pub struct FirstStart {
    /// The policy, its cap on `attempt` and the level that wrote it.
    pub restart: ResolvedRestart,
    /// The goal kept whole; `Arc` because lookups clone the slot.
    pub goal: Arc<NodeRunGoal>,
}

#[cfg(any(test, feature = "test-support"))]
impl FirstStart {
    /// The first start of a fixture instance of `name:tag` under the default
    /// policy, from a goal of its own.
    pub fn fixture(instance_id: &config::runtime::Name, name: &str, tag: &str) -> Self {
        Self {
            restart: ResolvedRestart::default(),
            goal: Arc::new(NodeRunGoal::for_internal_execution(
                config::runtime::NodeInstancePlan::new(instance_id.clone()),
                name,
                tag,
            )),
        }
    }
}

/// What the stack keeps of an instance across its processes.
#[derive(Debug, Clone, Default)]
pub struct Supervision {
    /// `None` for the root instance and a test fixture built without a
    /// spawn, which have no goal to restart from and keep `never`.
    pub(crate) first_start: Option<FirstStart>,
    /// The health monitor's kill of the process; set before the kill, taken
    /// by the exit, which then counts as a failure.
    pub(crate) health_kill: Option<HealthKill>,
    /// Processes after the first that reached `running`, over the instance's
    /// life.
    pub(crate) restart_count: u32,
    /// The attempt `max_restarts` caps, as [`RestartStatus::attempt`]
    /// reports it.
    pub(crate) attempt: u32,
    pub(crate) last_exit: Option<ExitCause>,
    /// When the current process reached `running`, for `ran_for`; cleared at
    /// its exit.
    pub(crate) run_started: Option<Instant>,
    /// When the instance last reached `running`; kept across exits.
    pub(crate) running_since: Option<SystemTime>,
    pub(crate) next_start: Option<SystemTime>,
    /// Cancels a restart the instance waits for. Made with the slot, never
    /// replaced, cancelled by the slot's claim (`claim_once`).
    pub restart_token: CancellationToken,
    /// The pairs delivered to this instance that no stop, refusal or newer
    /// pair on a slot that takes one peer has ended; a respawn requests each
    /// again.
    pub held_pairs: Vec<PlannedPair>,
    /// The registrations of the last process, taken at its exit and edited
    /// by joins and removals while the instance is `restarting`.
    pub(crate) registrations: Registrations,
}

impl Supervision {
    /// The policy, its cap on `attempt` and the level that wrote it; `never`
    /// for a slot without a first start, which has no goal to restart from.
    pub(crate) fn restart(&self) -> ResolvedRestart {
        self.first_start.as_ref().map_or(
            ResolvedRestart {
                declaration: RestartDeclaration::policy_only(RestartPolicy::Never),
                origin: RestartOrigin::Default,
            },
            |first| first.restart,
        )
    }

    /// The policy and how the restarts went, as the wire reports them.
    pub(crate) fn status(&self) -> RestartStatus {
        let restart = self.restart();
        RestartStatus {
            restart: restart.declaration,
            restart_origin: restart.origin,
            restart_count: self.restart_count,
            attempt: self.attempt,
            last_exit: self.last_exit.clone(),
            running_since_ms: self.running_since.map(unix_ms),
            next_start_ms: self.next_start.map(unix_ms),
        }
    }

    /// Advances the backoff after an exit or a failed start: `attempt` and
    /// `next_start` follow, and `next_start` is cleared at the attempt
    /// `max_restarts` refuses.
    pub(super) fn advance_backoff(
        &mut self,
        backoff: &RestartBackoff,
        ran_for: Option<Duration>,
    ) -> Attempt {
        let NextAttempt { attempt, delay } = backoff.next(self.attempt, ran_for);
        self.attempt = attempt.get();
        match self.restart().declaration.max_restarts() {
            Some(max_restarts) if attempt > max_restarts => {
                self.next_start = None;
                Attempt::Capped {
                    max_restarts,
                    token: self.restart_token.clone(),
                }
            }
            _ => {
                self.next_start = SystemTime::now().checked_add(delay);
                Attempt::Scheduled {
                    attempt,
                    delay,
                    scheduled_at: Instant::now(),
                    token: self.restart_token.clone(),
                }
            }
        }
    }
}

/// The observations and lifecycle watchers of an instance, taken at its
/// exit and kept for its next process.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Registrations {
    pub observations: BTreeMap<String, ObservationTargets>,
    pub watchers: Vec<String>,
}

/// The waits between an instance's exits and its next starts.
#[derive(Clone, Copy, Debug)]
pub struct RestartBackoff {
    /// The wait before the first of the attempts in a row.
    pub first_delay: Duration,
    /// The longest wait; the backoff stays here once it reaches it.
    pub max_delay: Duration,
    /// A run at least this long starts the backoff over.
    pub reset_after: Duration,
}

/// The attempt the backoff yields next and the wait before it.
#[derive(Debug, PartialEq)]
pub struct NextAttempt {
    pub attempt: NonZeroU32,
    pub delay: Duration,
}

impl RestartBackoff {
    /// The next attempt: a run of `reset_after` or longer starts the backoff
    /// over. `ran_for` is `None` when the last process never reached
    /// `running`.
    pub fn next(&self, previous_attempt: u32, ran_for: Option<Duration>) -> NextAttempt {
        let attempt = if ran_for.is_some_and(|ran_for| ran_for >= self.reset_after) {
            NonZeroU32::MIN
        } else {
            NonZeroU32::MIN.saturating_add(previous_attempt)
        };
        let delay = 2u32
            .checked_pow(attempt.get() - 1)
            .map_or(self.max_delay, |multiplier| {
                self.first_delay.saturating_mul(multiplier)
            })
            .min(self.max_delay);
        NextAttempt { attempt, delay }
    }
}

/// What the backoff yielded at an exit or a failed start.
#[derive(Debug)]
pub enum Attempt {
    /// The next attempt, due `delay` after `scheduled_at`; `token` cancels
    /// the wait.
    Scheduled {
        attempt: NonZeroU32,
        delay: Duration,
        scheduled_at: Instant,
        token: CancellationToken,
    },
    /// The attempt past `max_restarts`, which the cap refuses; the instance
    /// is `Failed`. `token` tells whether a stop claimed it since.
    Capped {
        max_restarts: NonZeroU32,
        token: CancellationToken,
    },
}

impl Attempt {
    /// The slot's restart token, cancelled once a stop claims the instance.
    pub fn token(&self) -> &CancellationToken {
        match self {
            Self::Scheduled { token, .. } | Self::Capped { token, .. } => token,
        }
    }
}

/// What a spawn is: a new instance registered with the policy and goal it
/// keeps for its restarts, or the next process of a `restarting` one, which
/// keeps its slot's.
#[derive(Debug)]
pub enum SpawnTarget {
    New(FirstStart),
    Respawn,
}

/// The run a start belongs to, as a run that panicked names the start it
/// left in flight.
#[derive(Debug)]
pub enum StartRun {
    /// The first start of a new instance from this goal.
    New(Arc<NodeRunGoal>),
    /// The next process of the `restarting` instance first started from
    /// `goal`, begun when the instance had made `restart_count` restarts
    /// ([`Supervision::restart_count`]). A restart's commit counts one, and
    /// the attempt after a failed start begins only once the failed run
    /// returned, so at any time one run at most is open under the slot at
    /// that count.
    Respawn {
        goal: Arc<NodeRunGoal>,
        restart_count: u32,
    },
}

/// What a respawn starts from, read off a `restarting` slot under the read
/// lock: the goal of its next process, the pairs it requests again, the
/// policy it keeps and what names its start.
#[derive(Debug)]
pub struct RespawnInputs {
    pub goal: NodeRunGoal,
    pub held_pairs: Vec<PlannedPair>,
    /// The policy the instance was first started under, which its restarts
    /// keep.
    pub restart: ResolvedRestart,
    /// The goal of the instance's first start and the restarts it has made,
    /// which name this start ([`StartRun::Respawn`]).
    pub first_start_goal: Arc<NodeRunGoal>,
    pub restart_count: u32,
    /// The run log of the last start, which the next start's log names.
    pub previous_run_log: Option<std::path::PathBuf>,
}

/// What the slot recorded of a self-exit, for the daemon to announce the
/// exit from and schedule the restart it may wait for.
#[derive(Debug)]
pub struct ExitRecord {
    /// The exit as recorded: a health kill is its own cause whatever the
    /// status.
    pub exit: ExitCause,
    /// The registrations the exit took; a `restarting` slot keeps a copy for
    /// its next process.
    pub registrations: Registrations,
    /// The backoff's say when the policy restarts the instance.
    pub next: Option<Attempt>,
}

/// Who hears of an instance's deliberate end beyond what the registries
/// hold, as its slot keeps them: the daemons of the remote peers its held
/// pairs name, and the watchers kept for its restart. An exit empties the
/// registries, so a stop reads these off the slot it removes.
#[derive(Debug, Default, PartialEq)]
pub struct KeptRelations {
    pub pair_daemons: BTreeSet<String>,
    pub watchers: Vec<String>,
}

impl KeptRelations {
    pub(super) fn of(supervision: &Supervision) -> Self {
        Self {
            pair_daemons: supervision
                .held_pairs
                .iter()
                .filter(|held| held.peer.core_node != held.own.core_node)
                .map(|held| held.peer.core_node.clone())
                .collect(),
            watchers: supervision.registrations.watchers.clone(),
        }
    }
}

/// What a claim and removal did with one instance.
#[derive(Debug, PartialEq)]
pub enum SlotRemoval {
    /// The slot is gone, claimed with the relations it kept.
    Removed(KeptRelations),
    /// The slot stays, and why.
    Left(SlotLeft),
    /// No slot has the id.
    Untracked,
}

/// What a stop path's claim of a process found.
#[derive(Debug, PartialEq)]
pub enum ProcessClaim {
    /// The slot ran the process unclaimed, and is claimed now.
    Claimed,
    /// The slot runs the process, and another stop path claimed it.
    ClaimedElsewhere,
    /// No slot has the id, or it runs another process or none.
    Gone,
}

/// What a stop's claim of a starting instance found.
#[derive(Debug, PartialEq)]
pub enum StartingClaim {
    /// A restart's start, now claimed, which cancels it; the slot's
    /// relations come back for the stop to tear down.
    Claimed(KeptRelations),
    /// A first start, which a stop does not end.
    FirstStart,
    /// A start another stop path already claimed.
    ClaimedElsewhere,
    /// The instance is no longer starting.
    NotStarting,
}

/// Why a claim and removal left an instance's slot in place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SlotLeft {
    /// Its start is in flight, a restart's when `restart`.
    Starting { restart: bool },
    /// It runs a process the caller did not stop: the one a start committed
    /// that was in flight when the caller began, or one a later run under its
    /// id started.
    RunsAnotherProcess,
}

/// The `running` instances of a daemon other than the core node's own, and
/// how many of them are healthy, for the messaging-outage guard of the health
/// monitor.
#[derive(Debug, Clone, Copy, Default)]
pub struct RunningHealth {
    pub running: usize,
    pub healthy: usize,
}

#[cfg(test)]
mod tests {
    use super::{Attempt, NextAttempt, RestartBackoff, Supervision};
    use config::node::{ResolvedRestart, RestartDeclaration, RestartOrigin, RestartPolicy};
    use std::num::NonZeroU32;
    use std::time::Duration;

    /// A slot without a first start, the core node's own instance, has no
    /// goal to restart from: it resolves to `never`, with the `default` origin.
    #[test]
    fn a_slot_without_a_first_start_is_never_restarted() {
        let restart = Supervision::default().restart();
        assert_eq!(
            restart,
            ResolvedRestart {
                declaration: RestartDeclaration::policy_only(RestartPolicy::Never),
                origin: RestartOrigin::Default,
            }
        );
        assert_eq!(restart.to_string(), "never (default)");
    }

    const BACKOFF: RestartBackoff = RestartBackoff {
        first_delay: Duration::from_secs(1),
        max_delay: Duration::from_secs(60),
        reset_after: Duration::from_secs(120),
    };

    fn attempt(n: u32) -> NonZeroU32 {
        NonZeroU32::new(n).expect("attempts start at 1")
    }

    #[test]
    fn restart_backoff_doubles_to_max_delay() {
        let short_run = Some(Duration::from_secs(5));
        let mut previous = 0;
        let mut delays = Vec::new();
        for _ in 0..8 {
            let next = BACKOFF.next(previous, short_run);
            previous = next.attempt.get();
            delays.push(next.delay.as_secs());
        }
        assert_eq!(previous, 8);
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60]);
        let saturated = BACKOFF.next(u32::MAX, None);
        assert_eq!(saturated.attempt.get(), u32::MAX);
        assert_eq!(
            saturated.delay, BACKOFF.max_delay,
            "a saturated backoff stays at its longest delay"
        );
    }

    #[test]
    fn restart_backoff_starts_over_after_the_reset_window() {
        let first = NextAttempt {
            attempt: attempt(1),
            delay: Duration::from_secs(1),
        };
        let sixth = NextAttempt {
            attempt: attempt(6),
            delay: Duration::from_secs(32),
        };
        assert_eq!(BACKOFF.next(5, Some(Duration::from_secs(120))), first);
        assert_eq!(BACKOFF.next(5, Some(Duration::from_secs(119))), sixth);
        assert_eq!(BACKOFF.next(5, None), sixth);
    }

    /// A delay the clock cannot add is still scheduled; only the wall-clock
    /// time of the next start is unknown.
    #[test]
    fn a_delay_past_the_clock_schedules_with_no_next_start() {
        let unbounded = RestartBackoff {
            first_delay: Duration::MAX,
            max_delay: Duration::MAX,
            reset_after: Duration::MAX,
        };
        let mut supervision = Supervision::default();
        let attempt = supervision.advance_backoff(&unbounded, None);
        assert!(
            matches!(attempt, Attempt::Scheduled { delay, .. } if delay == Duration::MAX),
            "{attempt:?}"
        );
        assert_eq!(supervision.attempt, 1);
        assert_eq!(supervision.next_start, None);
    }
}
