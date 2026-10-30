//! Node-side daemon-liveness watchdog (the uncatchable-death safety net).
//!
//! The daemon publishes a periodic beat on the [`TopicId::DaemonHeartbeat`]
//! topic (see `core_node::services::clock::publish_daemon_heartbeat`) and holds
//! a presence token for its core node name (see
//! `core_node::services::presence::claim_name`). Both carry the per-boot
//! instance id that each daemon process draws when it starts, and the runtime
//! config of the node names the instance id of the daemon that spawned it: the
//! node's own daemon. This watchdog cancels the node's shutdown token, so that
//! the node tears down via its normal clean-shutdown path instead of lingering
//! as an orphan, when:
//!
//! - its own daemon sends no beat for the configured grace period: the daemon
//!   is presumed dead for good. Beats of other daemons do not count;
//! - another daemon of the node's core node beats, and the node has no evidence
//!   that its own daemon lives: a new daemon of the core node runs. A new
//!   daemon starts with an empty stack, so no stack holds this node any more,
//!   and the next launch starts the nodes again.
//!
//! The evidence is the presence token of the own daemon. While the watchdog
//! knows the token is live, the beats of another daemon come from a second
//! daemon that claims the same core node name (a name collision, which each
//! daemon reports), and the node keeps running. When the watchdog does not
//! know the token is live (it went away, or the first report of it has not
//! arrived yet), a fresh presence read decides. Only a read that lists the
//! token keeps the node: a read can miss a live token under load, so a token
//! missing from a read is no evidence that the daemon lives.
//!
//! A clean ctrl+C / `systemctl stop` of the daemon kills its nodes immediately
//! and never relies on this; this only handles an *uncatchable* daemon death
//! (SIGKILL / OOM / crash) where the daemon runs no cleanup. A restart of the
//! daemon's messaging router is not a new daemon: the daemon process keeps its
//! instance id and its session, its beats resume inside the grace period, and
//! its presence token comes back. Beats and presence tokens of another core
//! node never reach the watchdog: both are filtered by the core node name on
//! the wire.

use std::collections::HashSet;
use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::TopicId;

use crate::error::Result;
use crate::messaging::{
    CoreNodePresence, CoreNodePresenceMessenger, LivelinessEvent, LivelinessWatch, MessengerHandle,
    Subscription, TopicBuffer,
};
use crate::runtime::{CancellationToken, NodeRunner, TaskHandle, spawn};

/// The per-boot instance id of a daemon process. A daemon draws a new one each
/// time it starts, so two different ids are two daemon processes.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DaemonInstanceId(String);

impl std::fmt::Display for DaemonInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the watchdog shuts the node down.
#[derive(Debug, PartialEq, Eq)]
enum DaemonLoss {
    /// The own daemon sent no beat for the whole grace period.
    Silent { grace: Duration },
    /// Another daemon of the node's core node beat while the own daemon was
    /// not known to live.
    Replaced {
        own: DaemonInstanceId,
        new: DaemonInstanceId,
    },
}

impl std::fmt::Display for DaemonLoss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Silent { grace } => write!(
                f,
                "No heartbeat of this node's daemon for {grace:?}: the daemon is presumed gone \
                 after an uncatchable death"
            ),
            Self::Replaced { own, new } => write!(
                f,
                "The daemon instance {new} of this node's core node beats, and the daemon \
                 instance {own} that started this node is not live: a new daemon runs, and no \
                 stack of it holds this node"
            ),
        }
    }
}

/// One thing the watchdog hears about the daemons of the node's core node.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DaemonSignal {
    /// A beat of the daemon with this instance id.
    Beat(DaemonInstanceId),
    /// The presence token of the daemon with this instance id is live.
    Present(DaemonInstanceId),
    /// The presence token of the daemon with this instance id went away.
    Absent(DaemonInstanceId),
}

/// Where the watchdog hears about the daemons of the node's core node,
/// abstracted so the watchdog logic can be unit-tested with paused time and no
/// live messaging session.
trait DaemonSignals {
    /// Resolves with the next signal, or `None` once the session closed.
    /// `Send` so the watchdog future stays spawnable.
    fn next_signal(&mut self) -> impl Future<Output = Option<DaemonSignal>> + Send;

    /// Whether a fresh presence read lists the presence token of `daemon`. A
    /// read can miss a live token under load, so only `true` is evidence.
    fn reads_present(&mut self, daemon: &DaemonInstanceId) -> impl Future<Output = bool> + Send;
}

/// The beats and the presence tokens of every daemon process of the node's
/// core node, on the node's messaging session. Both subscriptions pin the core
/// node name and leave the daemon's instance id open, so the signals of a new
/// daemon reach the watchdog.
struct CoreNodeDaemons {
    messenger: MessengerHandle,
    core_node: String,
    presence: LivelinessWatch<CoreNodePresence>,
    beats: Subscription,
}

impl CoreNodeDaemons {
    /// Watches the presence tokens before it subscribes to the beats, so the
    /// tokens that are live already are requested before any beat can arrive.
    async fn watch(node_runner: &NodeRunner) -> Result<Self> {
        let messenger = node_runner.messenger().clone();
        let core_node = node_runner.processor().bound_core_node().to_owned();
        let presence = CoreNodePresenceMessenger::watch(&messenger, &core_node).await?;
        let beats = crate::core_node::subscribe_core_topic(
            node_runner,
            TopicId::DaemonHeartbeat.name(),
            TopicBuffer::Backpressure,
        )
        .await?;
        Ok(Self {
            messenger,
            core_node,
            presence,
            beats,
        })
    }
}

impl DaemonSignals for CoreNodeDaemons {
    async fn next_signal(&mut self) -> Option<DaemonSignal> {
        tokio::select! {
            // A presence report that already arrived applies before a beat.
            biased;
            event = self.presence.rx.recv_async() => Some(match event.ok()? {
                LivelinessEvent::Alive(claim) => {
                    DaemonSignal::Present(DaemonInstanceId(claim.instance_id))
                }
                LivelinessEvent::Gone(claim) => {
                    DaemonSignal::Absent(DaemonInstanceId(claim.instance_id))
                }
            }),
            beat = self.beats.on_next_message() => {
                Some(DaemonSignal::Beat(DaemonInstanceId(beat?.instance_id().to_owned())))
            }
        }
    }

    async fn reads_present(&mut self, daemon: &DaemonInstanceId) -> bool {
        let read = CoreNodePresenceMessenger::list_live(
            &self.messenger,
            Some(&self.core_node),
            CoreNodePresenceMessenger::LIST_TIMEOUT,
        )
        .await;
        match read {
            Ok(claims) => claims.iter().any(|claim| claim.instance_id == daemon.0),
            Err(error) => {
                tracing::warn!(
                    "The presence read of core node {} failed: {error}",
                    self.core_node
                );
                false
            }
        }
    }
}

/// Watch the daemons of the node's core node and spawn the watchdog loop. The
/// own daemon is the one that the node's runtime config names. Returns the
/// task handle so the caller can hold it for the node's lifetime.
pub async fn spawn_daemon_watchdog(
    node_runner: Arc<NodeRunner>,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<TaskHandle<Result<()>>> {
    let own = DaemonInstanceId(
        node_runner
            .processor()
            .bound_core_node_instance_id()
            .to_owned(),
    );
    let daemons = CoreNodeDaemons::watch(&node_runner).await?;
    Ok(spawn(run_watchdog(daemons, own, grace, cancellation_token)))
}

/// Testable core: follows the own daemon until it is lost (see
/// [`watch_own_daemon`]), then cancels the token, which triggers the node's
/// clean shutdown, and exits. Also exits promptly if some other path cancels
/// the token (e.g. an explicit `SHUTDOWN_SERVICE`) so the task does not
/// linger. Returns `Ok(())` in every case: the loop ending is not itself an
/// error.
async fn run_watchdog<S: DaemonSignals>(
    daemons: S,
    own: DaemonInstanceId,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let loss = tokio::select! {
        biased;
        // Another shutdown path already fired; stop watching.
        _ = cancellation_token.cancelled() => return Ok(()),
        loss = watch_own_daemon(daemons, own, grace) => loss,
    };
    // The session closed: losing the session is not proof that the daemon is
    // gone, and the paths that own the session handle its loss.
    let Some(loss) = loss else {
        return Ok(());
    };
    tracing::warn!("{loss}; shutting down the node");
    cancellation_token.cancel();
    Ok(())
}

/// Follows the own daemon until it is lost: each beat of it restarts the
/// `grace` window, and a beat of another daemon ends the watch unless the own
/// daemon is known to live (see the module documentation). Returns `None` if
/// the session closes first.
async fn watch_own_daemon<S: DaemonSignals>(
    mut daemons: S,
    own: DaemonInstanceId,
    grace: Duration,
) -> Option<DaemonLoss> {
    let mut own_token_live = false;
    let mut reported_collisions: HashSet<DaemonInstanceId> = HashSet::new();
    let silence = tokio::time::sleep(grace);
    tokio::pin!(silence);
    loop {
        let signal = tokio::select! {
            biased;
            // A closed session ends the watch with no loss.
            signal = daemons.next_signal() => signal?,
            () = &mut silence => return Some(DaemonLoss::Silent { grace }),
        };
        match signal {
            DaemonSignal::Beat(daemon) if daemon == own => {
                silence.as_mut().reset(tokio::time::Instant::now() + grace);
            }
            DaemonSignal::Present(daemon) if daemon == own => own_token_live = true,
            DaemonSignal::Absent(daemon) if daemon == own => own_token_live = false,
            DaemonSignal::Present(_) | DaemonSignal::Absent(_) => {}
            DaemonSignal::Beat(other) => {
                let own_lives = own_token_live || daemons.reads_present(&own).await;
                if !own_lives {
                    return Some(DaemonLoss::Replaced { own, new: other });
                }
                if reported_collisions.insert(other.clone()) {
                    tracing::warn!(
                        "The daemon instance {other} beats under the core node name of this \
                         node while the daemon instance {own} that started this node lives: two \
                         daemons claim the name, and this node keeps running"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;

    use config::node::{QoSProfile, TopicRetention};
    use core_node_api::encoding::ClockTick;
    use core_node_api::names::CORE_NODE_TAG;
    use pmi::{Messenger, MessengerAdapter, MessengerBackend, MockAdapter};

    use super::*;
    use crate::messaging::{SenderTarget, TopicMessenger};
    use crate::runtime::{Processor, STANDALONE_CORE_NODE, StandaloneConfig};

    /// A grace period no scripted test reaches unless it waits for it: a watch
    /// that ends under it ends for a signal, never for the timeout.
    const LONG_GRACE: Duration = Duration::from_secs(3600);

    fn daemon(instance_id: &str) -> DaemonInstanceId {
        DaemonInstanceId(instance_id.to_owned())
    }

    /// A source that never yields a signal: `next_signal` is pending forever,
    /// so only the grace timeout can fire.
    struct NoSignals;
    impl DaemonSignals for NoSignals {
        async fn next_signal(&mut self) -> Option<DaemonSignal> {
            std::future::pending().await
        }

        async fn reads_present(&mut self, _daemon: &DaemonInstanceId) -> bool {
            unreachable!("no beat of another daemon arrives")
        }
    }

    /// A source whose session is gone: `next_signal` reports it closed at once.
    struct ClosedSession;
    impl DaemonSignals for ClosedSession {
        async fn next_signal(&mut self) -> Option<DaemonSignal> {
            None
        }

        async fn reads_present(&mut self, _daemon: &DaemonInstanceId) -> bool {
            unreachable!("no beat of another daemon arrives")
        }
    }

    /// A source that yields a beat of the own daemon every `every`.
    struct PeriodicBeats {
        every: Duration,
    }
    impl DaemonSignals for PeriodicBeats {
        async fn next_signal(&mut self) -> Option<DaemonSignal> {
            tokio::time::sleep(self.every).await;
            Some(DaemonSignal::Beat(daemon(OWN)))
        }

        async fn reads_present(&mut self, _daemon: &DaemonInstanceId) -> bool {
            unreachable!("no beat of another daemon arrives")
        }
    }

    /// The instance id of the own daemon in the scripted tests.
    const OWN: &str = "own-daemon";

    /// One step of a [`ScriptedDaemons`] source.
    enum Step {
        /// A beat of the daemon with this instance id.
        Beat(&'static str),
        /// The presence token of the daemon with this instance id is live.
        Present(&'static str),
        /// The presence token of the daemon with this instance id went away.
        Absent(&'static str),
        /// No signal for this long.
        Silence(Duration),
        /// No signal any more, and the session stays open.
        Hang,
    }

    /// A source that plays its steps in order, then reports its session
    /// closed. Its presence reads list the daemons in `listed`, and it counts
    /// them.
    struct ScriptedDaemons {
        steps: VecDeque<Step>,
        listed: Vec<&'static str>,
        reads: usize,
    }

    impl ScriptedDaemons {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self {
                steps: steps.into_iter().collect(),
                listed: Vec::new(),
                reads: 0,
            }
        }

        fn listing(mut self, listed: impl IntoIterator<Item = &'static str>) -> Self {
            self.listed = listed.into_iter().collect();
            self
        }
    }

    impl DaemonSignals for &mut ScriptedDaemons {
        async fn next_signal(&mut self) -> Option<DaemonSignal> {
            loop {
                match self.steps.pop_front()? {
                    Step::Beat(instance_id) => {
                        return Some(DaemonSignal::Beat(daemon(instance_id)));
                    }
                    Step::Present(instance_id) => {
                        return Some(DaemonSignal::Present(daemon(instance_id)));
                    }
                    Step::Absent(instance_id) => {
                        return Some(DaemonSignal::Absent(daemon(instance_id)));
                    }
                    Step::Silence(silence) => tokio::time::sleep(silence).await,
                    Step::Hang => std::future::pending::<()>().await,
                }
            }
        }

        async fn reads_present(&mut self, daemon: &DaemonInstanceId) -> bool {
            self.reads += 1;
            self.listed.iter().any(|listed| *listed == daemon.0)
        }
    }

    /// With no beats, the watchdog cancels the token, but not before the grace
    /// period elapses (a beat-less node must wait the full window).
    #[tokio::test(start_paused = true)]
    async fn fires_after_grace_with_no_beats() {
        let token = CancellationToken::new();
        let grace = Duration::from_secs(120);
        let handle = spawn(run_watchdog(NoSignals, daemon(OWN), grace, token.clone()));

        tokio::time::advance(Duration::from_secs(119)).await;
        assert!(
            !token.is_cancelled(),
            "watchdog fired before the grace period"
        );

        tokio::time::advance(Duration::from_secs(2)).await;
        handle.await.expect("join").expect("watchdog ok");
        assert!(
            token.is_cancelled(),
            "watchdog did not fire after the grace period"
        );
    }

    /// While beats keep arriving inside the window, the watchdog never fires,
    /// even across many grace periods' worth of time.
    #[tokio::test(start_paused = true)]
    async fn never_fires_while_beats_arrive() {
        let token = CancellationToken::new();
        let grace = Duration::from_secs(120);
        // Beat well inside the window.
        let handle = spawn(run_watchdog(
            PeriodicBeats {
                every: Duration::from_secs(5),
            },
            daemon(OWN),
            grace,
            token.clone(),
        ));

        // Advance far past several grace periods; steady beats keep it alive.
        for _ in 0..100 {
            tokio::time::advance(Duration::from_secs(5)).await;
            tokio::task::yield_now().await;
        }
        assert!(
            !token.is_cancelled(),
            "watchdog fired despite a steady heartbeat"
        );

        // Stop the loop so the test ends cleanly.
        token.cancel();
        handle.await.expect("join").expect("watchdog ok");
    }

    /// If another path cancels the token, the watchdog exits immediately rather
    /// than waiting out the (here, very long) grace period.
    #[tokio::test(start_paused = true)]
    async fn exits_promptly_when_token_cancelled_elsewhere() {
        let token = CancellationToken::new();
        let handle = spawn(run_watchdog(
            NoSignals,
            daemon(OWN),
            LONG_GRACE,
            token.clone(),
        ));

        token.cancel();
        handle.await.expect("join").expect("watchdog ok");
    }

    /// A closed session ends the watchdog without cancelling the token: losing
    /// the session is not proof that the daemon died, and the paths that own
    /// the session handle its loss.
    #[tokio::test(start_paused = true)]
    async fn exits_without_firing_when_the_session_closes() {
        let token = CancellationToken::new();
        let handle = spawn(run_watchdog(
            ClosedSession,
            daemon(OWN),
            LONG_GRACE,
            token.clone(),
        ));

        handle.await.expect("join").expect("watchdog ok");
        assert!(
            !token.is_cancelled(),
            "a closed session must not trigger a node shutdown"
        );
    }

    /// The token of the own daemon went away and a new daemon of the core node
    /// beats: the watch ends at that beat, once a fresh read does not list the
    /// own daemon either.
    #[tokio::test(start_paused = true)]
    async fn a_new_daemon_ends_the_watch_when_the_own_daemon_is_gone() {
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Beat(OWN),
            Step::Absent(OWN),
            Step::Beat("new-daemon"),
            Step::Beat(OWN),
        ])
        .listing(["new-daemon"]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon(OWN),
                new: daemon("new-daemon"),
            })
        );
        assert_eq!(daemons.reads, 1);
    }

    /// The own daemon died before the node heard anything of it: neither a beat
    /// nor its token reached the node. The first beat of a new daemon ends the
    /// watch, with no wait for the grace period.
    #[tokio::test(start_paused = true)]
    async fn a_new_daemon_ends_the_watch_of_a_node_that_never_heard_its_own_daemon() {
        let mut daemons =
            ScriptedDaemons::new([Step::Present("new-daemon"), Step::Beat("new-daemon")])
                .listing(["new-daemon"]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon(OWN),
                new: daemon("new-daemon"),
            })
        );
    }

    /// A second daemon that claims the same core node name beats while the
    /// token of the own daemon is live: the node keeps running, with no
    /// presence read, until the session closes.
    #[tokio::test(start_paused = true)]
    async fn a_same_name_daemon_does_not_end_the_watch_while_the_own_daemon_lives() {
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Present("twin-daemon"),
            Step::Beat("twin-daemon"),
            Step::Beat(OWN),
            Step::Beat("twin-daemon"),
        ]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(loss, None, "the watch ends only when the session closes");
        assert_eq!(daemons.reads, 0, "a live token needs no presence read");
    }

    /// No report of the own token arrived yet when a second daemon of the same
    /// name beats: a fresh read that lists the own daemon keeps the node.
    #[tokio::test(start_paused = true)]
    async fn a_read_that_lists_the_own_daemon_keeps_the_node() {
        let mut daemons = ScriptedDaemons::new([Step::Beat("twin-daemon"), Step::Present(OWN)])
            .listing([OWN, "twin-daemon"]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(loss, None, "the watch ends only when the session closes");
        assert_eq!(daemons.reads, 1);
    }

    /// During a name collision the own daemon dies: the next beat of the other
    /// daemon ends the watch.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_own_daemon_during_a_collision_ends_the_watch() {
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Present("twin-daemon"),
            Step::Beat("twin-daemon"),
            Step::Absent(OWN),
            Step::Beat("twin-daemon"),
        ])
        .listing(["twin-daemon"]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon(OWN),
                new: daemon("twin-daemon"),
            })
        );
    }

    /// A restart of the daemon's messaging router: the token of the own daemon
    /// goes away and comes back, and its beats stop for less than the grace
    /// period. The watch goes on until the session closes.
    #[tokio::test(start_paused = true)]
    async fn a_router_restart_does_not_end_the_watch() {
        let grace = Duration::from_secs(120);
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Beat(OWN),
            Step::Absent(OWN),
            Step::Silence(grace - Duration::from_secs(1)),
            Step::Present(OWN),
            Step::Beat(OWN),
            Step::Silence(grace - Duration::from_secs(1)),
            Step::Beat(OWN),
        ]);

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), grace).await;

        assert_eq!(loss, None, "the watch ends only when the session closes");
        assert_eq!(daemons.reads, 0);
    }

    /// The beats of a second daemon of the same name do not restart the grace
    /// window: the own daemon's silence ends the watch exactly one grace period
    /// after its last beat.
    #[tokio::test(start_paused = true)]
    async fn only_the_beats_of_the_own_daemon_restart_the_grace_window() {
        let grace = Duration::from_secs(120);
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Beat(OWN),
            Step::Silence(Duration::from_secs(60)),
            Step::Beat("twin-daemon"),
            Step::Silence(Duration::from_secs(50)),
            Step::Beat("twin-daemon"),
            Step::Hang,
        ]);
        let started = tokio::time::Instant::now();

        let loss = watch_own_daemon(&mut daemons, daemon(OWN), grace).await;

        assert_eq!(loss, Some(DaemonLoss::Silent { grace }));
        assert_eq!(tokio::time::Instant::now() - started, grace);
    }

    /// A new daemon cancels the node's shutdown token at its first beat, with
    /// no wait for the grace period: the injected clock does not move. The
    /// session closes after that beat, and a closed session cancels nothing,
    /// so only the new daemon can cancel the token.
    #[tokio::test(start_paused = true)]
    async fn a_new_daemon_cancels_the_token_at_its_first_beat() {
        let token = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let mut daemons = ScriptedDaemons::new([
            Step::Present(OWN),
            Step::Beat(OWN),
            Step::Absent(OWN),
            Step::Beat("new-daemon"),
        ]);

        run_watchdog(&mut daemons, daemon(OWN), LONG_GRACE, token.clone())
            .await
            .expect("watchdog ok");

        assert!(token.is_cancelled(), "a new daemon stops the node");
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "the node stops without waiting for the grace period"
        );
    }

    /// A standalone node on an in-memory mock session, and the session: its
    /// bound core node is [`STANDALONE_CORE_NODE`].
    async fn node_runner_on_mock_session() -> (NodeRunner, MessengerHandle) {
        let mut messenger = Messenger::new(MessengerAdapter::Mock(MockAdapter::default()));
        messenger
            .start_session()
            .await
            .expect("the mock session starts");
        let messenger = MessengerHandle::from_shared(Arc::new(tokio::sync::Mutex::new(messenger)));
        let manifest = config::node::NodeConfigParser::from_content(
            r#"{
                peppy_schema: "node/v1",
                manifest: { name: "watched", tag: "v1", description: ["A test node."] },
                execution: { language: "rust", run_cmd: ["watched"] },
            }"#,
        )
        .expect("the manifest parses");
        let processor = Processor::standalone_from_manifest(manifest, &StandaloneConfig::new())
            .expect("a standalone processor");
        let node_runner =
            NodeRunner::over_messenger(messenger.clone(), processor, CancellationToken::new());
        (node_runner, messenger)
    }

    /// Publishes one beat the way a daemon does: on the target of its core
    /// node, under its per-boot instance id, on the default link.
    async fn publish_daemon_beat(
        messenger: &MessengerHandle,
        core_node: &str,
        daemon_instance_id: &str,
    ) {
        let publisher = TopicMessenger::declare_publisher(
            messenger,
            core_node,
            daemon_instance_id,
            SenderTarget::node(core_node, CORE_NODE_TAG).expect("a valid target"),
            None,
            TopicId::DaemonHeartbeat.name(),
            QoSProfile::SensorData,
            TopicRetention::LiveOnly,
        )
        .await
        .expect("the beat publisher is declared");
        let beat = ClockTick::new(ClockTick::MIN_TIME_NS)
            .encode()
            .expect("the beat encodes");
        publisher
            .publish(beat)
            .await
            .expect("the beat is published");
    }

    /// Declares the presence token a daemon holds for its core node name.
    async fn declare_daemon_presence(
        messenger: &MessengerHandle,
        core_node: &str,
        daemon_instance_id: &str,
    ) -> crate::messaging::LivelinessToken {
        CoreNodePresenceMessenger::declare(messenger, core_node, daemon_instance_id)
            .await
            .expect("the presence token is declared")
    }

    /// On a real session, the watchdog hears no beat and no token of a daemon
    /// of another core node. Here a daemon of `another-core-node` holds a
    /// token under the instance id of the own daemon: if the token reached the
    /// watchdog, or a read listed it, the own daemon would look alive and the
    /// beat of the new daemon would not end the watch. If the beats of
    /// `another-core-node` reached the watchdog, the watch would end at the
    /// beat of `other-daemon` instead.
    #[tokio::test(start_paused = true)]
    async fn signals_of_another_core_node_never_reach_the_watchdog() {
        let (node_runner, messenger) = node_runner_on_mock_session().await;
        let _other_token = declare_daemon_presence(&messenger, "another-core-node", OWN).await;
        let daemons = CoreNodeDaemons::watch(&node_runner)
            .await
            .expect("the watchdog watches");

        publish_daemon_beat(&messenger, "another-core-node", OWN).await;
        publish_daemon_beat(&messenger, "another-core-node", "other-daemon").await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "new-daemon").await;

        let loss = watch_own_daemon(daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon(OWN),
                new: daemon("new-daemon"),
            })
        );
    }

    /// On a real session, a second daemon of the same core node name beats
    /// while the token of the own daemon is live: its beats neither stop the
    /// node nor restart the grace window, so the watch ends for silence.
    #[tokio::test(start_paused = true)]
    async fn a_same_name_daemon_does_not_stop_a_node_whose_daemon_lives() {
        let (node_runner, messenger) = node_runner_on_mock_session().await;
        let _own_token = declare_daemon_presence(&messenger, STANDALONE_CORE_NODE, OWN).await;
        let _twin_token =
            declare_daemon_presence(&messenger, STANDALONE_CORE_NODE, "twin-daemon").await;
        let daemons = CoreNodeDaemons::watch(&node_runner)
            .await
            .expect("the watchdog watches");

        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "twin-daemon").await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, OWN).await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "twin-daemon").await;

        let loss = watch_own_daemon(daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(loss, Some(DaemonLoss::Silent { grace: LONG_GRACE }));
    }

    /// On a real session, the token of the own daemon goes away, and a new
    /// daemon of the core node beats: the watch ends at that beat.
    #[tokio::test(start_paused = true)]
    async fn the_end_of_the_own_daemon_token_lets_a_new_daemon_stop_the_node() {
        let (node_runner, messenger) = node_runner_on_mock_session().await;
        let daemons = CoreNodeDaemons::watch(&node_runner)
            .await
            .expect("the watchdog watches");

        let own_token = declare_daemon_presence(&messenger, STANDALONE_CORE_NODE, OWN).await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, OWN).await;
        drop(own_token);
        let _new_token =
            declare_daemon_presence(&messenger, STANDALONE_CORE_NODE, "new-daemon").await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "new-daemon").await;

        let loss = watch_own_daemon(daemons, daemon(OWN), LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon(OWN),
                new: daemon("new-daemon"),
            })
        );
    }
}
