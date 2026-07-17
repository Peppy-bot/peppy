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

/// Prefix reserved for short-lived election tokens. A dot cannot occur in a
/// real instance id (`config::runtime::Name`), so committed claims and startup
/// contenders remain unambiguous in the broker snapshot.
const NAME_CLAIM_CANDIDATE_PREFIX: &str = ".claim.";

/// Refuses a claimed core-node name, otherwise declares and returns this
/// daemon generation's retained presence token.
pub(crate) async fn claim_name(
    messenger: &MessengerHandle,
    core_node_name: &str,
    instance_id: &str,
) -> Result<LivelinessToken> {
    let candidate_id = format!("{NAME_CLAIM_CANDIDATE_PREFIX}{instance_id}");
    let candidate = CoreNodePresenceMessenger::declare(messenger, core_node_name, &candidate_id)
        .await
        .map_err(crate::Error::from)?;

    // Declare candidacy before inspecting the broker so there is no
    // check-then-declare gap. A committed (non-candidate) token always wins;
    // otherwise simultaneous candidates deterministically keep only the
    // smallest generation id. The winner waits until losers have dropped
    // their candidate tokens before committing its real presence token.
    loop {
        let claims = CoreNodePresenceMessenger::list_live(
            messenger,
            Some(core_node_name),
            CoreNodePresenceMessenger::LIST_TIMEOUT,
        )
        .await?;
        if claims.iter().any(|presence| {
            !presence
                .instance_id
                .starts_with(NAME_CLAIM_CANDIDATE_PREFIX)
        }) {
            return Err(crate::Error::CoreNodeNameTaken {
                name: core_node_name.to_string(),
            });
        }

        let winner = claims
            .iter()
            .filter_map(|presence| {
                presence
                    .instance_id
                    .strip_prefix(NAME_CLAIM_CANDIDATE_PREFIX)
            })
            .min();
        if winner != Some(instance_id) {
            return Err(crate::Error::CoreNodeNameTaken {
                name: core_node_name.to_string(),
            });
        }
        if claims.len() == 1 {
            break;
        }

        tokio::task::yield_now().await;
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

/// Watches this daemon's presence name with history replay and raises a
/// rate-limited alarm whenever a foreign daemon instance claims it. Log, don't
/// kill: shutting down a daemon over a collision would take its spawned nodes
/// down with it.
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
            if presence.instance_id == own_instance_id {
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
}
