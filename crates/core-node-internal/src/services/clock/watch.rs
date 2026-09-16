//! What this daemon knows about the clock domains it hosts.
//!
//! A domain's publisher runs here, so this daemon is the one that can say
//! whether it has supplied an instant yet and what the last one was. It learns
//! that the same way any consumer does, by subscribing to the domain's stream,
//! which keeps the answer honest: a domain reports ready when its ticks are
//! actually on the wire, not when its publisher merely started.
//!
//! The watches are driven by the listing that reads them
//! ([`super::list`]): each call opens one for any hosted domain not yet
//! watched and drops the ones whose publisher has left the stack, so the
//! registry has one owner: the listing. The cost is that the first listing
//! after a domain appears can report it not ready although it is already
//! ticking; the next one, a subscription later, reports it correctly.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use config::runtime::{ClockDomainId, Name, ProducerRef};
use parking_lot::Mutex;
use peppylib::MessengerHandle;
use tokio::task::JoinHandle;

use crate::Result;

/// One hosted domain, as the listing reports it.
#[derive(Debug, Clone)]
pub(crate) struct HostedDomain {
    pub(crate) domain: ClockDomainId,
    pub(crate) publisher_instance_id: Name,
    /// Whether the domain has published an instant this daemon has seen.
    pub(crate) ready: bool,
    /// The last instant seen, or `None` when none has been.
    pub(crate) last_tick_ns: Option<u64>,
}

struct Watch {
    domain: ClockDomainId,
    last_tick: Arc<AtomicU64>,
    feeder: JoinHandle<()>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.feeder.abort();
    }
}

/// The domains this daemon hosts, keyed by the instance supplying each one.
#[derive(Default)]
pub(crate) struct ClockWatches {
    watches: Mutex<BTreeMap<Name, Watch>>,
}

impl ClockWatches {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Watches `domain` unless this instance's exact lifetime is already
    /// watched. A publisher restarting under the same domain name mints a new
    /// lifetime, so its identity differs and the old watch is replaced rather
    /// than left reporting a timeline nothing publishes any more.
    pub(crate) async fn ensure(
        &self,
        messenger: &MessengerHandle,
        as_core_node: &str,
        as_instance_id: &str,
        domain: &ClockDomainId,
        publisher_instance_id: &Name,
    ) -> Result<()> {
        if self
            .watches
            .lock()
            .get(publisher_instance_id)
            .is_some_and(|watch| &watch.domain == domain)
        {
            return Ok(());
        }
        // A hosted domain's publisher runs on this daemon, so its address is
        // this machine plus the instance supplying it.
        let publisher = ProducerRef::new(as_core_node, publisher_instance_id.as_str());
        let mut subscription = peppylib::clock::subscribe_domain_stream(
            messenger,
            as_core_node,
            as_instance_id,
            domain,
            &publisher,
        )
        .await?;
        let last_tick = Arc::new(AtomicU64::new(0));
        let feeder_tick = Arc::clone(&last_tick);
        let feeder = tokio::spawn(async move {
            while let Some(message) = subscription.on_next_message().await {
                if let Ok(tick) =
                    core_node_api::encoding::ClockTick::decode(message.payload_bytes().as_ref())
                {
                    feeder_tick.store(tick.time(), Ordering::Relaxed);
                }
            }
        });
        self.watches.lock().insert(
            publisher_instance_id.clone(),
            Watch {
                domain: domain.clone(),
                last_tick,
                feeder,
            },
        );
        Ok(())
    }

    /// Drops the watches whose publisher is no longer on the stack. A domain
    /// leaves with the instance that supplied it, so nothing keeps reporting a
    /// timeline that has no source.
    pub(crate) fn prune(&self, live_publishers: &BTreeSet<Name>) {
        self.watches
            .lock()
            .retain(|instance_id, _| live_publishers.contains(instance_id));
    }

    pub(crate) fn snapshot(&self) -> Vec<HostedDomain> {
        self.watches
            .lock()
            .iter()
            .map(|(publisher_instance_id, watch)| {
                let last = watch.last_tick.load(Ordering::Relaxed);
                HostedDomain {
                    domain: watch.domain.clone(),
                    publisher_instance_id: publisher_instance_id.clone(),
                    ready: last != 0,
                    last_tick_ns: (last != 0).then_some(last),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::tests::started_mock_messenger;
    use config::runtime::{ClockIncarnation, CoreNodeName};

    const AS_CORE_NODE: &str = "cn-sim";
    const AS_INSTANCE: &str = "core_instance";

    fn domain(name: &str, incarnation: u64) -> ClockDomainId {
        ClockDomainId::new(
            Name::new(name).expect("valid domain name"),
            CoreNodeName::new(AS_CORE_NODE).expect("valid core node name"),
            ClockIncarnation::try_from(incarnation).expect("non-zero"),
        )
    }

    fn instance_id(id: &str) -> Name {
        Name::new(id).expect("valid instance id")
    }

    async fn watch(
        watches: &ClockWatches,
        messenger: &MessengerHandle,
        domain: &ClockDomainId,
        publisher: &str,
    ) {
        watches
            .ensure(
                messenger,
                AS_CORE_NODE,
                AS_INSTANCE,
                domain,
                &instance_id(publisher),
            )
            .await
            .expect("a hosted domain on a live session is watchable");
    }

    /// One watch per publisher: a listing runs on every `clock list`, so
    /// watching a domain already watched must leave the one subscription
    /// reading it. A domain that has not ticked reports not ready.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn watching_a_domain_twice_leaves_one_watch() {
        let messenger = started_mock_messenger().await;
        let watches = ClockWatches::new();
        let robot = domain("robot", 1);

        watch(&watches, &messenger, &robot, "sim_inst").await;
        watch(&watches, &messenger, &robot, "sim_inst").await;

        let hosted = watches.snapshot();
        assert_eq!(hosted.len(), 1, "one publisher, one watch");
        assert_eq!(hosted[0].domain, robot);
        assert!(!hosted[0].ready, "no tick has arrived on the mock session");
        assert_eq!(hosted[0].last_tick_ns, None);
    }

    /// A publisher restarting under the same name mints a new lifetime, and
    /// the watch follows it: the registry reports the timeline being published
    /// now.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_new_lifetime_of_a_name_replaces_the_watch_of_the_old_one() {
        let messenger = started_mock_messenger().await;
        let watches = ClockWatches::new();

        watch(&watches, &messenger, &domain("robot", 1), "sim_inst").await;
        let replacement = domain("robot", 2);
        watch(&watches, &messenger, &replacement, "sim_inst").await;

        let hosted = watches.snapshot();
        assert_eq!(hosted.len(), 1);
        assert_eq!(
            hosted[0].domain, replacement,
            "the watch reports the lifetime being published now"
        );
    }

    /// A domain leaves with the instance supplying it, so pruning to the
    /// publishers the stack still runs drops the watch of the one that left
    /// and keeps every other.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pruning_drops_the_watch_whose_publisher_left() {
        let messenger = started_mock_messenger().await;
        let watches = ClockWatches::new();
        let staying = domain("robot", 1);

        watch(&watches, &messenger, &staying, "sim_inst").await;
        watch(&watches, &messenger, &domain("bench", 1), "bench_inst").await;
        assert_eq!(watches.snapshot().len(), 2);

        watches.prune(&BTreeSet::from([instance_id("sim_inst")]));

        let hosted = watches.snapshot();
        assert_eq!(hosted.len(), 1);
        assert_eq!(hosted[0].domain, staying);

        watches.prune(&BTreeSet::new());
        assert!(
            watches.snapshot().is_empty(),
            "a stack running no publisher hosts no domain"
        );
    }
}
