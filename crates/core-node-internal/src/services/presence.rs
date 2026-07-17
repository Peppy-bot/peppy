use crate::Result;
use peppylib::{CoreNodePresenceMessenger, MessengerHandle};
use pmi::{LivelinessEvent, LivelinessToken};
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error};

/// Minimum interval between successive name-collision alarms. A reconnect can
/// replay the same live token, so rate limiting keeps router flapping from
/// flooding the daemon log.
const NAME_COLLISION_ALARM_COOLDOWN: Duration = Duration::from_secs(60);

/// Settle window a *federated* daemon's [`claim_name`] holds its candidacy
/// open before committing, re-querying throughout.
///
/// A liveliness query is answered from the local router's token registry,
/// which converges only after the router's configured `connect` links come up
/// and their declaration exchange flushes — so a daemon booting into an
/// established mesh would otherwise win a one-horse election and commit
/// before the incumbent's token arrives. Holding the candidacy open until
/// the view has been clean for this long makes "whoever committed first
/// wins" deterministic: the joiner observes the incumbent and refuses
/// pre-commit, and the incumbent is never asked to step down. Standalone
/// daemons (no configured links) skip the settle, so the common boot pays
/// nothing. Sized far above the observed post-link declaration flush
/// (~100ms) while keeping a federated boot comfortably interactive.
pub const NAME_CLAIM_LINKED_SETTLE: Duration = Duration::from_secs(3);

/// Re-query cadence inside the settle window: fast enough to refuse promptly
/// when the incumbent's token lands, slow enough not to hammer the router.
const NAME_CLAIM_SETTLE_POLL: Duration = Duration::from_millis(150);

/// Prefix reserved for short-lived election tokens. A dot cannot occur in a
/// real instance id (`config::runtime::Name`), so committed claims and startup
/// contenders remain unambiguous in the broker snapshot.
const NAME_CLAIM_CANDIDATE_PREFIX: &str = ".claim.";

/// Refuses a claimed core-node name, otherwise declares and returns this
/// daemon generation's retained presence token.
///
/// `settle` holds the candidacy open that long before committing, re-querying
/// throughout, so a claim racing token propagation across freshly-established
/// router links still observes the incumbent and refuses pre-commit. Pass
/// [`NAME_CLAIM_LINKED_SETTLE`] when the daemon's router has configured
/// federation links and [`Duration::ZERO`] for a standalone router, whose
/// local registry is authoritative immediately.
pub(crate) async fn claim_name(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    settle: Duration,
) -> Result<LivelinessToken> {
    let candidate_id = format!("{NAME_CLAIM_CANDIDATE_PREFIX}{instance_id}");
    let candidate = CoreNodePresenceMessenger::declare(messenger, core_node_name, &candidate_id)
        .await
        .map_err(crate::Error::from)?;
    let claimed_at = Instant::now();

    // Declare candidacy before inspecting the broker so there is no
    // check-then-declare gap. A committed (non-candidate) token always wins;
    // otherwise simultaneous candidates deterministically keep only the
    // smallest generation id. The winner waits until losers have dropped
    // their candidate tokens — and until the settle window has passed with a
    // clean view — before committing its real presence token.
    loop {
        let claims = CoreNodePresenceMessenger::list_live(
            messenger,
            Some(core_node_name),
            CoreNodePresenceMessenger::LIST_TIMEOUT,
        )
        .await?;
        // A routed Zenoh liveliness query can return the same logical token
        // through more than one matching path. Arbitrate identities as a set:
        // counting the raw replies makes one candidate reported twice look like
        // two contenders and leaves the winner spinning here forever.
        let mut claim_ids: Vec<&str> = claims
            .iter()
            .map(|presence| presence.instance_id.as_str())
            .collect();
        claim_ids.sort_unstable();
        claim_ids.dedup();

        if claim_ids
            .iter()
            .any(|instance_id| !instance_id.starts_with(NAME_CLAIM_CANDIDATE_PREFIX))
        {
            return Err(crate::Error::CoreNodeNameTaken {
                name: core_node_name.to_string(),
            });
        }

        let winner = claim_ids
            .iter()
            .filter_map(|instance_id| instance_id.strip_prefix(NAME_CLAIM_CANDIDATE_PREFIX))
            .min();
        if winner != Some(instance_id) {
            return Err(crate::Error::CoreNodeNameTaken {
                name: core_node_name.to_string(),
            });
        }
        if claim_ids.len() == 1 && claimed_at.elapsed() >= settle {
            break;
        }

        // Waiting on the settle clock is paced; waiting only for a losing
        // candidate to drop its token is not (that is sub-millisecond on the
        // same broker, and the pre-settle behavior).
        if claimed_at.elapsed() < settle {
            tokio::time::sleep(NAME_CLAIM_SETTLE_POLL).await;
        } else {
            tokio::task::yield_now().await;
        }
    }

    let token = CoreNodePresenceMessenger::declare(messenger, core_node_name, instance_id)
        .await
        .map_err(crate::Error::from)?;
    drop(candidate);
    Ok(token)
}

fn alarm_cooldown_elapsed(last_alarm: Option<Instant>, now: Instant) -> bool {
    last_alarm.is_none_or(|at| now.duration_since(at) >= NAME_COLLISION_ALARM_COOLDOWN)
}

fn is_committed_foreign_claim(own_instance_id: &str, observed_instance_id: &str) -> bool {
    observed_instance_id != own_instance_id
        && !observed_instance_id.starts_with(NAME_CLAIM_CANDIDATE_PREFIX)
}

/// Watches this daemon's presence name with history replay and raises a
/// rate-limited alarm whenever a foreign daemon instance claims it. Log, don't
/// kill: this daemon committed the name first (the joiner's settling claim is
/// the side that refuses; see [`claim_name`]), and shutting down a running
/// daemon over a collision would take its spawned nodes down with it.
pub(crate) async fn watch_for_duplicate_name(
    messenger: MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
    cancel: CancellationToken,
) -> Result<JoinHandle<Result<()>>> {
    let watch = CoreNodePresenceMessenger::watch(&messenger, core_node_name).await?;
    let own_instance_id = instance_id.to_string();
    let core_node_name = core_node_name.to_string();

    Ok(tokio::spawn(async move {
        // Keep the watch guard alive alongside its receiver for the lifetime of
        // the task. `history(true)` replays tokens that predate subscription.
        let watch = watch;
        let mut last_alarm: Option<Instant> = None;
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                event = watch.rx.recv_async() => match event {
                    Ok(event) => event,
                    Err(_) => break,
                },
            };

            let LivelinessEvent::Alive(presence) = event else {
                continue;
            };
            // Election candidates are deliberately short-lived and do not own
            // the name. A history replay can deliver our just-dropped candidate
            // after the committed token, so treating it as a collision raises a
            // false alarm on an otherwise clean startup.
            if !is_committed_foreign_claim(&own_instance_id, &presence.instance_id) {
                continue;
            }
            debug!(
                foreign_instance_id = %presence.instance_id,
                core_node_name,
                "observed foreign core-node presence claim"
            );
            let now = Instant::now();
            if alarm_cooldown_elapsed(last_alarm, now) {
                last_alarm = Some(now);
                error!(
                    "core-node name collision: daemon instance '{}' is advertising presence \
                     under this daemon's name '{}' (own instance '{}'); core-node API calls \
                     route by name and may land on either daemon. Set `core_node_name` in \
                     `~/.peppy/conf/peppy_config.json5` on one of them to a unique name and \
                     restart it",
                    presence.instance_id, core_node_name, own_instance_id,
                );
            }
        }
        Ok(())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tests::started_mock_messenger;
    use crate::test_support::quiet_subscriber_guard;
    use pmi::{
        Messenger, MessengerAdapter, MessengerBackend, SubscriberBufferSizes, ZenohAdapter,
        ZenohNetProtocol,
    };
    use std::sync::Arc;
    use tokio::sync::Mutex;

    /// A single daemon using the router-relay topology must finish the
    /// candidate-token election. The in-memory mock cannot reproduce Zenoh's
    /// client/router liveliness routing, so keep this as a real-router
    /// regression test.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn router_topology_name_claim_settles() {
        let router = ZenohAdapter::start_router_ephemeral_in_mode(
            "127.0.0.1",
            None,
            false,
            SubscriberBufferSizes::default(),
            None,
        )
        .await
        .expect("start a router-topology Zenoh router");
        let adapter = ZenohAdapter::connect_to_with_discovery(
            ZenohNetProtocol::Tcp,
            &router.host,
            router.port,
            Vec::new(),
            false,
            SubscriberBufferSizes::default(),
            None,
        )
        .expect("build a router-topology daemon session")
        .with_session_reconnect()
        .with_namespace(Some(config::org::OrgNamespace::local()));
        let mut client = Messenger::new(MessengerAdapter::Zenoh(adapter));
        client
            .start_session()
            .await
            .expect("connect a router-topology daemon session");
        let messenger = MessengerHandle::from_shared(Arc::new(Mutex::new(client)));

        let token = tokio::time::timeout(
            Duration::from_secs(1),
            claim_name(
                &messenger,
                "router_claim_node",
                "router_claim_instance",
                Duration::ZERO,
            ),
        )
        .await
        .expect("a lone router-topology daemon must not hang in name election")
        .expect("a lone daemon should claim an unclaimed name");

        let claims = CoreNodePresenceMessenger::list_live(
            &messenger,
            Some("router_claim_node"),
            CoreNodePresenceMessenger::LIST_TIMEOUT,
        )
        .await
        .expect("list the committed claim");
        assert!(
            claims
                .iter()
                .any(|claim| claim.instance_id == "router_claim_instance"),
            "the committed claim must be visible after election: {claims:?}"
        );

        drop(token);
        drop(messenger);
        drop(router);
    }

    /// The load-bearing settle behavior: a committed foreign token that lands
    /// *mid-window* (token propagation across freshly-established router
    /// links finishing after the claim's first query) refuses the claim
    /// pre-commit instead of letting a one-horse election win blind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settling_claim_refuses_a_committed_token_that_lands_mid_window() {
        let _subscriber = quiet_subscriber_guard();
        let messenger = started_mock_messenger().await;

        let claim = tokio::spawn({
            let messenger = messenger.clone();
            async move {
                claim_name(
                    &messenger,
                    "settle_node",
                    "joiner_instance",
                    Duration::from_secs(60),
                )
                .await
            }
        });
        // Land the incumbent's token after the claim's first (clean) query
        // round, inside the settle window.
        tokio::time::sleep(NAME_CLAIM_SETTLE_POLL / 2).await;
        let _incumbent =
            CoreNodePresenceMessenger::declare(&messenger, "settle_node", "incumbent_instance")
                .await
                .expect("declare the incumbent's committed token");

        let outcome = tokio::time::timeout(Duration::from_secs(2), claim)
            .await
            .expect("the settling claim must refuse promptly, not wait out the window")
            .expect("claim task should not panic");
        let err = match outcome {
            Err(err) => err,
            Ok(_token) => panic!("a committed token landing mid-settle must refuse the claim"),
        };
        assert!(
            matches!(err, crate::Error::CoreNodeNameTaken { ref name } if name == "settle_node"),
            "the refusal must be the boot refusal: {err}"
        );
    }

    /// A clean settle window commits — after, not before, the window passes —
    /// and a zero window commits immediately (the standalone-router path).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn settling_claim_commits_after_a_clean_window() {
        let _subscriber = quiet_subscriber_guard();
        let messenger = started_mock_messenger().await;

        let settle = Duration::from_millis(300);
        let started = Instant::now();
        let _token = claim_name(&messenger, "clean_settle_node", "sole_instance", settle)
            .await
            .expect("a clean window must commit the claim");
        assert!(
            started.elapsed() >= settle,
            "the claim must hold candidacy for the whole settle window"
        );

        let instant = Instant::now();
        let _other = claim_name(&messenger, "instant_node", "sole_instance", Duration::ZERO)
            .await
            .expect("a zero settle claims an unclaimed name");
        assert!(
            instant.elapsed() < settle,
            "a zero settle must not wait out any window"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn duplicate_watch_stops_promptly_on_cancel() {
        let cancel = CancellationToken::new();
        let handle = watch_for_duplicate_name(
            started_mock_messenger().await,
            "test_core_node",
            "test_instance",
            cancel.clone(),
        )
        .await
        .expect("duplicate-name watch should spawn");

        cancel.cancel();

        let outcome = tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("watch must stop promptly after cancel")
            .expect("watch task should not panic");
        outcome.expect("watch should exit Ok after a clean cancel");
    }

    #[test]
    fn duplicate_alarm_is_rate_limited() {
        let fired_at = Instant::now();
        assert!(
            alarm_cooldown_elapsed(None, fired_at),
            "a first alarm has no cooldown to wait out"
        );
        assert!(!alarm_cooldown_elapsed(
            Some(fired_at),
            fired_at + Duration::from_secs(1),
        ));
        assert!(alarm_cooldown_elapsed(
            Some(fired_at),
            fired_at + NAME_COLLISION_ALARM_COOLDOWN,
        ));
    }

    #[test]
    fn collision_watch_ignores_own_claim_and_election_candidates() {
        assert!(!is_committed_foreign_claim("own", "own"));
        assert!(!is_committed_foreign_claim("own", ".claim.foreign"));
        assert!(is_committed_foreign_claim("own", "foreign"));
    }
}
