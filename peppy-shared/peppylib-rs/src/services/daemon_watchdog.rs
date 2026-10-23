//! Node-side daemon-liveness watchdog (the uncatchable-death safety net).
//!
//! The daemon publishes a periodic beat on the [`TopicId::DaemonHeartbeat`]
//! topic (see `core_node::services::clock::publish_daemon_heartbeat`). Each
//! daemon process publishes under the per-boot instance id that it draws when
//! it starts, and that id is in the key of each beat. This watchdog follows
//! the daemon of the first beat that it gets and cancels the node's shutdown
//! token, so that the node tears down via its normal clean-shutdown path
//! instead of lingering as an orphan, when:
//!
//! - no beat arrives for the configured grace period: the daemon is presumed
//!   dead for good;
//! - a beat comes from another per-boot instance id: a new daemon of the
//!   node's core node runs. A new daemon starts with an empty stack, so no
//!   stack holds this node any more, and the next launch starts the nodes
//!   again.
//!
//! A clean ctrl+C / `systemctl stop` of the daemon kills its nodes immediately
//! and never relies on this; this only handles an *uncatchable* daemon death
//! (SIGKILL / OOM / crash) where the daemon runs no cleanup. A restart of the
//! daemon's messaging router is not a new daemon: the daemon process keeps its
//! instance id, each beat resets the window, and the node rides out the
//! outage. The beats of a daemon of another core node never reach the
//! watchdog, because its subscription pins the target of the node's own core
//! node on the wire.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use core_node_api::TopicId;

use crate::error::Result;
use crate::messaging::{Subscription, TopicBuffer};
use crate::runtime::{CancellationToken, NodeRunner, TaskHandle, spawn};

/// The per-boot instance id of a daemon process, read from the key of its
/// beat. A daemon draws a new one each time it starts, so beats with two
/// different ids come from two daemon processes.
#[derive(Debug, Clone, PartialEq, Eq)]
struct DaemonInstanceId(String);

impl std::fmt::Display for DaemonInstanceId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Why the watchdog shuts the node down.
#[derive(Debug, PartialEq, Eq)]
enum DaemonLoss {
    /// No beat arrived for the whole grace period.
    Silent { grace: Duration },
    /// A beat came from another daemon process of the node's core node than
    /// the one the watchdog follows.
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
                "No daemon heartbeat for {grace:?}: the daemon is presumed gone after an \
                 uncatchable death"
            ),
            Self::Replaced { own, new } => write!(
                f,
                "A heartbeat of this node's core node came from the daemon instance {new}, \
                 not from the daemon instance {own} that this node follows: a new daemon \
                 runs, and no stack of it holds this node"
            ),
        }
    }
}

/// Abstraction over "wait for the next daemon beat" so the watchdog logic can
/// be unit-tested with paused time and no live messaging subscription.
trait BeatSource {
    /// Resolves with the instance id of the daemon that sent the next beat,
    /// or `None` if the source closed. `Send` so the watchdog future stays
    /// spawnable.
    fn next_beat(&mut self) -> impl Future<Output = Option<DaemonInstanceId>> + Send;
}

impl BeatSource for Subscription {
    async fn next_beat(&mut self) -> Option<DaemonInstanceId> {
        let beat = self.on_next_message().await?;
        Some(DaemonInstanceId(beat.instance_id().to_owned()))
    }
}

/// Subscribe to the daemon heartbeat and spawn the watchdog loop. Returns the
/// task handle so the caller can hold it for the node's lifetime.
pub async fn spawn_daemon_watchdog(
    node_runner: Arc<NodeRunner>,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<TaskHandle<Result<()>>> {
    let beats = subscribe_daemon_beats(&node_runner).await?;
    Ok(spawn(run_watchdog(beats, grace, cancellation_token)))
}

/// The beats of every daemon process of the node's core node. The
/// subscription is keyed the same way as the clock's: it pins the target of
/// the core node and leaves the daemon's per-boot instance id open, so the
/// beats of a new daemon reach the watchdog.
async fn subscribe_daemon_beats(node_runner: &NodeRunner) -> Result<Subscription> {
    crate::core_node::subscribe_core_topic(
        node_runner,
        TopicId::DaemonHeartbeat.name(),
        TopicBuffer::Backpressure,
    )
    .await
}

/// Testable core: follows the daemon until it is lost (see
/// [`watch_own_daemon`]), then cancels the token, which triggers the node's
/// clean shutdown, and exits. Also exits promptly if some other path cancels
/// the token (e.g. an explicit `SHUTDOWN_SERVICE`) so the task does not
/// linger. Returns `Ok(())` in every case: the loop ending is not itself an
/// error.
async fn run_watchdog<S: BeatSource>(
    source: S,
    grace: Duration,
    cancellation_token: CancellationToken,
) -> Result<()> {
    let loss = tokio::select! {
        biased;
        // Another shutdown path already fired; stop watching.
        _ = cancellation_token.cancelled() => return Ok(()),
        loss = watch_own_daemon(source, grace) => loss,
    };
    // The source closed (session gone): losing the session is not proof that
    // the daemon is gone, and the paths that own the session handle its loss.
    let Some(loss) = loss else {
        return Ok(());
    };
    tracing::warn!("{loss}; shutting down the node");
    cancellation_token.cancel();
    Ok(())
}

/// Follows the daemon of the first beat until that daemon is lost: each beat
/// of it resets the `grace` window, and the first beat of another daemon ends
/// the watch at once. Returns `None` if the source closes first.
async fn watch_own_daemon<S: BeatSource>(mut source: S, grace: Duration) -> Option<DaemonLoss> {
    let mut followed: Option<DaemonInstanceId> = None;
    loop {
        let Ok(beat) = tokio::time::timeout(grace, source.next_beat()).await else {
            return Some(DaemonLoss::Silent { grace });
        };
        // A closed source ends the watch with no loss.
        let sender = beat?;
        let own = followed.get_or_insert_with(|| sender.clone());
        if sender != *own {
            return Some(DaemonLoss::Replaced {
                own: own.clone(),
                new: sender,
            });
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
    use crate::messaging::{MessengerHandle, SenderTarget, TopicMessenger};
    use crate::runtime::{Processor, STANDALONE_CORE_NODE, StandaloneConfig};

    /// A grace period no scripted test reaches: a watch that ends under it
    /// ends for a beat, never for the timeout.
    const LONG_GRACE: Duration = Duration::from_secs(3600);

    fn daemon(instance_id: &str) -> DaemonInstanceId {
        DaemonInstanceId(instance_id.to_owned())
    }

    /// A source that never yields a beat: `next_beat` is pending forever, so
    /// only the grace timeout can fire.
    struct NeverBeats;
    impl BeatSource for NeverBeats {
        async fn next_beat(&mut self) -> Option<DaemonInstanceId> {
            std::future::pending().await
        }
    }

    /// A source whose session is gone: `next_beat` reports it closed at once.
    struct ClosedSource;
    impl BeatSource for ClosedSource {
        async fn next_beat(&mut self) -> Option<DaemonInstanceId> {
            None
        }
    }

    /// A source that yields a beat of one daemon every `every`.
    struct PeriodicBeats {
        every: Duration,
    }
    impl BeatSource for PeriodicBeats {
        async fn next_beat(&mut self) -> Option<DaemonInstanceId> {
            tokio::time::sleep(self.every).await;
            Some(daemon("the-daemon"))
        }
    }

    /// One step of a [`ScriptedBeats`] source.
    enum Step {
        /// A beat of the daemon with this instance id.
        Beat(&'static str),
        /// No beat for this long.
        Silence(Duration),
    }

    /// A source that plays its steps in order, then reports itself closed.
    struct ScriptedBeats(VecDeque<Step>);

    impl ScriptedBeats {
        fn new(steps: impl IntoIterator<Item = Step>) -> Self {
            Self(steps.into_iter().collect())
        }
    }

    impl BeatSource for ScriptedBeats {
        async fn next_beat(&mut self) -> Option<DaemonInstanceId> {
            loop {
                match self.0.pop_front()? {
                    Step::Silence(silence) => tokio::time::sleep(silence).await,
                    Step::Beat(instance_id) => return Some(daemon(instance_id)),
                }
            }
        }
    }

    /// With no beats, the watchdog cancels the token, but not before the grace
    /// period elapses (a beat-less node must wait the full window).
    #[tokio::test(start_paused = true)]
    async fn fires_after_grace_with_no_beats() {
        let token = CancellationToken::new();
        let grace = Duration::from_secs(120);
        let handle = spawn(run_watchdog(NeverBeats, grace, token.clone()));

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
        let handle = spawn(run_watchdog(NeverBeats, LONG_GRACE, token.clone()));

        token.cancel();
        handle.await.expect("join").expect("watchdog ok");
    }

    /// A closed source ends the watchdog without cancelling the token: losing
    /// the session is not proof that the daemon died, and the paths that own
    /// the session handle its loss.
    #[tokio::test(start_paused = true)]
    async fn exits_without_firing_when_the_source_closes() {
        let token = CancellationToken::new();
        let handle = spawn(run_watchdog(ClosedSource, LONG_GRACE, token.clone()));

        handle.await.expect("join").expect("watchdog ok");
        assert!(
            !token.is_cancelled(),
            "a closed source must not trigger a node shutdown"
        );
    }

    /// The first beat names the daemon that the node follows. A later beat of
    /// another instance id is a new daemon of the core node, and ends the watch
    /// at once, whatever beats follow it.
    #[tokio::test(start_paused = true)]
    async fn a_beat_of_another_daemon_instance_ends_the_watch() {
        let source = ScriptedBeats::new([
            Step::Beat("first-daemon"),
            Step::Beat("first-daemon"),
            Step::Beat("second-daemon"),
            Step::Beat("first-daemon"),
        ]);

        let loss = watch_own_daemon(source, LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon("first-daemon"),
                new: daemon("second-daemon"),
            })
        );
    }

    /// A restart of the daemon's messaging router silences the beats for less
    /// than the grace period, and the daemon process keeps its instance id: the
    /// watch goes on until the source closes.
    #[tokio::test(start_paused = true)]
    async fn a_router_restart_does_not_end_the_watch() {
        let grace = Duration::from_secs(120);
        let source = ScriptedBeats::new([
            Step::Beat("the-daemon"),
            Step::Silence(grace - Duration::from_secs(1)),
            Step::Beat("the-daemon"),
            Step::Silence(grace - Duration::from_secs(1)),
            Step::Beat("the-daemon"),
        ]);

        let loss = watch_own_daemon(source, grace).await;

        assert_eq!(loss, None, "the watch ends only when the source closes");
    }

    /// A new daemon cancels the node's shutdown token on its first beat, with
    /// no wait for the grace period: the injected clock does not move. The
    /// source closes after that beat, and a closed source cancels nothing, so
    /// only the new daemon can cancel the token.
    #[tokio::test(start_paused = true)]
    async fn a_new_daemon_cancels_the_token_at_its_first_beat() {
        let token = CancellationToken::new();
        let started = tokio::time::Instant::now();
        let source = ScriptedBeats::new([Step::Beat("first-daemon"), Step::Beat("second-daemon")]);

        spawn(run_watchdog(source, LONG_GRACE, token.clone()))
            .await
            .expect("join")
            .expect("watchdog ok");

        assert!(token.is_cancelled(), "a new daemon stops the node");
        assert_eq!(
            tokio::time::Instant::now(),
            started,
            "the node stops without waiting for the grace period"
        );
    }

    /// A standalone node on an in-memory mock session: its bound core node is
    /// [`STANDALONE_CORE_NODE`].
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
                manifest: { name: "watched", tag: "v1" },
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

    /// The subscription of the watchdog reads the daemon instance id from the
    /// key of each beat, and carries no beat of a daemon of another core node:
    /// the beat of the other core node, published first, never becomes the
    /// daemon that the node follows.
    #[tokio::test(start_paused = true)]
    async fn beats_of_another_core_node_never_reach_the_watchdog() {
        let (node_runner, messenger) = node_runner_on_mock_session().await;
        let beats = subscribe_daemon_beats(&node_runner)
            .await
            .expect("the watchdog subscribes");

        publish_daemon_beat(&messenger, "another-core-node", "other-daemon").await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "first-daemon").await;
        publish_daemon_beat(&messenger, STANDALONE_CORE_NODE, "second-daemon").await;

        let loss = watch_own_daemon(beats, LONG_GRACE).await;

        assert_eq!(
            loss,
            Some(DaemonLoss::Replaced {
                own: daemon("first-daemon"),
                new: daemon("second-daemon"),
            })
        );
    }
}
