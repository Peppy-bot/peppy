//! Retaining topics over the real transport: a subscriber that joins after a
//! publication reads the messages the publisher keeps and the live ones.
//!
//! The delivery cases run in both local topologies a node session uses:
//! relayed through the router, and as gossip peers with a direct link. The
//! declaration rules and the router restart run through the router. Gated on
//! `build_zenoh` because each test spawns a zenohd process; serialized via
//! [`common::ZENOH_SERIAL`].
//!
//! No case sleeps, and every read is bounded by a timeout that only a failure
//! reaches. Two barriers order the steps:
//!
//! - A subscription that has read a retained value receives every later
//!   publication: its publisher answered the history query after the router
//!   knew the subscription.
//! - A message published before a subscriber joins can still be in transit at
//!   the router and reach it live. [`routed`] waits that out, so a case that
//!   asserts the exact first read joins after it.

#![cfg(feature = "build_zenoh")]

mod common;
use common::{RECV_TIMEOUT, ZENOH_SERIAL, receiver, sender, test_node_target};

use bytes::Bytes;
use config::node::TopicRetention;
use pmi::{
    MessengerBackend, MessengerPublisher, Payload, PeppyMessagingInterfaceError, PublisherQoS,
    SubscriberBufferSizes, SubscriberQoS, Subscription, TopicWireSender, ZenohAdapter,
    ZenohNetProtocol, ZenohdInstance,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The two local topologies of a node session.
#[derive(Debug, Clone, Copy)]
enum Topology {
    /// Client sessions: every message relays through the router.
    Router,
    /// Peer sessions: gossip forms a direct link between the two.
    Peer,
}

const TOPOLOGIES: [Topology; 2] = [Topology::Router, Topology::Peer];

async fn start_router() -> ZenohdInstance {
    ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("zenohd should start")
}

async fn open_session(router: &ZenohdInstance, topology: Topology) -> ZenohAdapter {
    open_session_with_buffers(router, topology, SubscriberBufferSizes::default()).await
}

async fn open_session_with_buffers(
    router: &ZenohdInstance,
    topology: Topology,
    buffer_sizes: SubscriberBufferSizes,
) -> ZenohAdapter {
    started(adapter_for(router, topology, buffer_sizes)).await
}

fn adapter_for(
    router: &ZenohdInstance,
    topology: Topology,
    buffer_sizes: SubscriberBufferSizes,
) -> ZenohAdapter {
    ZenohAdapter::connect_to_with_discovery(
        ZenohNetProtocol::Tcp,
        &router.host,
        router.port,
        Vec::new(),
        matches!(topology, Topology::Peer),
        buffer_sizes,
        None,
    )
    .expect("adapter")
}

async fn started(mut adapter: ZenohAdapter) -> ZenohAdapter {
    adapter.start_session().await.expect("session should start");
    adapter
}

fn latest(depth: usize) -> TopicRetention {
    TopicRetention::latest(depth).expect("a depth in range")
}

/// The topic as `instance` publishes it, under the node [`sender`] publishes
/// as, so [`receiver`] subscribes to every instance.
fn sender_from(instance: &str, topic: &str) -> TopicWireSender {
    TopicWireSender::new(
        "test_core_node",
        instance,
        test_node_target("test_node"),
        None,
        topic,
    )
    .expect("valid wire fields")
}

/// Each case names its own topic per topology, so one router serves both.
fn topic_in(topology: Topology, case: &str) -> String {
    format!("{case}_{topology:?}").to_lowercase()
}

async fn subscribe(adapter: &ZenohAdapter, topic: &str, retention: TopicRetention) -> Subscription {
    adapter
        .subscribe_topic(&receiver(topic), SubscriberQoS::Standard, retention)
        .await
        .expect("subscribe")
}

fn declare(
    session: &ZenohAdapter,
    sender: &TopicWireSender,
    qos: PublisherQoS,
    retention: TopicRetention,
) -> Result<MessengerPublisher, PeppyMessagingInterfaceError> {
    session
        .declare_topic_publisher(sender, qos, retention)
        .map(MessengerPublisher::Zenoh)
}

async fn publish(publisher: &MessengerPublisher, values: impl IntoIterator<Item = u64>) {
    for value in values {
        publisher
            .publish(Bytes::from(value.to_string()))
            .await
            .expect("publish");
    }
}

/// The next value `subscription` reads.
async fn next(subscription: &Subscription, label: &str) -> u64 {
    next_within(RECV_TIMEOUT, subscription, label).await
}

async fn next_within(bound: Duration, subscription: &Subscription, label: &str) -> u64 {
    let message = tokio::time::timeout(bound, subscription.rx.recv_async())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        .unwrap_or_else(|_| panic!("the subscription closed before {label}"));
    String::from_utf8_lossy(&message.payload().as_bytes())
        .parse()
        .expect("a number")
}

async fn next_values(subscription: &Subscription, count: usize, label: &str) -> Vec<u64> {
    let mut values = Vec::with_capacity(count);
    for _ in 0..count {
        values.push(next(subscription, label).await);
    }
    values
}

/// Waits until `publisher`'s session routes the topic to a subscriber. With
/// one subscriber declared, the next publish reaches it.
async fn wait_for_subscriber(publisher: &ZenohAdapter, topic: &str) {
    assert!(
        publisher
            .wait_for_topic_subscriber(&sender(topic), RECV_TIMEOUT)
            .await
            .expect("matching status"),
        "no subscriber matched `{topic}`"
    );
}

/// Returns once a retaining subscription on a session of its own has read each
/// of `latest_values`: every publication up to them left its publisher before
/// the replies that subscription read, so a subscription declared afterwards
/// receives none of them live.
async fn routed(router: &ZenohdInstance, topology: Topology, topic: &str, latest_values: &[u64]) {
    let session = open_session(router, topology).await;
    let subscription = subscribe(&session, topic, latest(1)).await;
    let mut awaited: Vec<u64> = latest_values.to_vec();
    while !awaited.is_empty() {
        let value = next(&subscription, "a routed value").await;
        awaited.retain(|latest| *latest != value);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_subscriber_reads_the_retained_value_then_live_values() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "late_join");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, 1..=3).await;
        routed(&router, topology, &topic, &[3]).await;

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(1)).await;
        assert_eq!(
            next(&subscription, "the retained value").await,
            3,
            "{topology:?}"
        );

        publish(&publisher, 4..=5).await;
        assert_eq!(
            next_values(&subscription, 2, "the live values").await,
            [4, 5],
            "{topology:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retention_depth_replays_that_many_oldest_first() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "depth");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(3),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, 1..=5).await;
        routed(&router, topology, &topic, &[5]).await;

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(3)).await;
        assert_eq!(
            next_values(&subscription, 3, "the retained values").await,
            [3, 4, 5],
            "{topology:?}"
        );

        publish(&publisher, [6]).await;
        assert_eq!(
            next(&subscription, "the live value").await,
            6,
            "{topology:?}"
        );
    }
}

/// A subscription that asks for more than the topic keeps reads what the
/// topic keeps.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_topic_keeps_no_more_than_its_depth() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "depth_bound");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, 1..=3).await;
        routed(&router, topology, &topic, &[3]).await;

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(3)).await;
        assert_eq!(
            next(&subscription, "the one retained value").await,
            3,
            "{topology:?}"
        );

        publish(&publisher, [4]).await;
        assert_eq!(
            next(&subscription, "the live value").await,
            4,
            "{topology:?}"
        );
    }
}

/// A publisher with nothing retained answers the subscriber's history query at
/// once, so the first publication is delivered at every depth.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_before_any_publication_reads_the_first_one() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        for depth in [1, 3] {
            let topic = topic_in(topology, &format!("first_publication_depth{depth}"));
            let subscriber_session = open_session(&router, topology).await;
            let subscription = subscribe(&subscriber_session, &topic, latest(depth)).await;

            let publisher_session = open_session(&router, topology).await;
            let publisher = declare(
                &publisher_session,
                &sender(&topic),
                PublisherQoS::Standard,
                latest(depth),
            )
            .expect("declare the retaining publisher");
            publish(&publisher, [1]).await;

            assert_eq!(
                next(&subscription, "the first publication").await,
                1,
                "{topology:?}, depth {depth}"
            );
        }
    }
}

/// The new session publishes 9, and the first value a new subscriber reads is
/// 9: nothing of the earlier session's retained value is left.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_restarted_publisher_retains_nothing_of_its_earlier_session() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "restart");
        let mut earlier_session = open_session(&router, topology).await;
        let earlier_publisher = declare(
            &earlier_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&earlier_publisher, [1]).await;
        earlier_session
            .stop_session()
            .await
            .expect("the earlier session should stop");

        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(1)).await;
        wait_for_subscriber(&publisher_session, &topic).await;
        publish(&publisher, [9]).await;

        assert_eq!(
            next(&subscription, "the new session's value").await,
            9,
            "{topology:?}"
        );
    }
}

/// One session subscribes live only, then retaining. Once the retaining
/// subscription has read the retained value, the live-only one is routed too
/// and holds nothing: the next publication is the first value it reads.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_only_subscriber_reads_only_live_values() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "live_only_subscriber");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, [1]).await;
        routed(&router, topology, &topic, &[1]).await;

        let subscriber_session = open_session(&router, topology).await;
        let live_only = subscribe(&subscriber_session, &topic, TopicRetention::LiveOnly).await;
        let retaining = subscribe(&subscriber_session, &topic, latest(1)).await;
        assert_eq!(
            next(&retaining, "the retained value").await,
            1,
            "{topology:?}"
        );
        assert!(
            live_only.rx.is_empty(),
            "{topology:?}: the live-only subscription was replayed a value"
        );

        publish(&publisher, [2]).await;
        assert_eq!(next(&live_only, "the live value").await, 2, "{topology:?}");
    }
}

/// A retaining subscriber on a live-only publisher reads its live values.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_retaining_subscriber_reads_a_live_only_publisher() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "live_only_publisher");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            TopicRetention::LiveOnly,
        )
        .expect("declare the live-only publisher");

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(3)).await;
        wait_for_subscriber(&publisher_session, &topic).await;
        publish(&publisher, [1]).await;

        assert_eq!(
            next(&subscription, "the live value").await,
            1,
            "{topology:?}"
        );
    }
}

/// Subscribers join while the publisher runs. Whatever the interleaving of
/// the replay and the live stream, each reads a strictly increasing stream,
/// and the last one reads on to the newest value once the publisher stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joins_during_a_publish_loop_read_an_increasing_stream_up_to_the_newest_value() {
    const JOINS: usize = 40;
    const READS_PER_JOIN: usize = 8;
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        for depth in [1, 3] {
            let topic = topic_in(topology, &format!("race_depth{depth}"));
            let publisher_session = open_session(&router, topology).await;
            let publisher = declare(
                &publisher_session,
                &sender(&topic),
                PublisherQoS::Important,
                latest(depth),
            )
            .expect("declare the retaining publisher");
            let stop = Arc::new(AtomicBool::new(false));
            let mut publishing = tokio::spawn({
                let stop = Arc::clone(&stop);
                async move {
                    let mut value = 0;
                    while !stop.load(Ordering::Relaxed) {
                        value += 1;
                        publish(&publisher, [value]).await;
                        tokio::task::yield_now().await;
                    }
                    value
                }
            });

            let subscriber_session = open_session(&router, topology).await;
            for join in 0..JOINS {
                let subscription = subscribe(&subscriber_session, &topic, latest(depth)).await;
                let values = next_values(&subscription, READS_PER_JOIN, "a value").await;
                assert!(
                    values.windows(2).all(|pair| pair[0] < pair[1]),
                    "{topology:?}, depth {depth}, join {join}: {values:?} is not increasing"
                );
            }

            // The reader keeps draining while the publisher stops, so a full
            // queue never holds the publish loop.
            let subscription = subscribe(&subscriber_session, &topic, latest(depth)).await;
            let mut read = next(&subscription, "the last join's first value").await;
            stop.store(true, Ordering::Relaxed);
            let mut newest = None;
            while newest != Some(read) {
                tokio::select! {
                    published = &mut publishing, if newest.is_none() => {
                        newest = Some(published.expect("the publish loop"));
                    }
                    value = next(&subscription, "a value up to the newest") => {
                        assert!(
                            value > read,
                            "{topology:?}, depth {depth}: {value} after {read} goes back"
                        );
                        read = value;
                    }
                }
            }
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn every_qos_profile_retains() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let publisher_session = open_session(&router, topology).await;
        let subscriber_session = open_session(&router, topology).await;
        for (qos, value) in [
            (PublisherQoS::BestEffort, 1),
            (PublisherQoS::Standard, 2),
            (PublisherQoS::Important, 3),
            (PublisherQoS::Critical, 4),
        ] {
            let topic = topic_in(topology, &format!("qos_{qos:?}"));
            let publisher = declare(&publisher_session, &sender(&topic), qos, latest(1))
                .expect("declare the retaining publisher");
            publish(&publisher, [value]).await;

            let subscription = subscribe(&subscriber_session, &topic, latest(1)).await;
            assert_eq!(
                next(&subscription, "the retained value").await,
                value,
                "{topology:?}, {qos:?}"
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_retained_value_outlives_the_publisher_handle() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "dropped_handle");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, [1]).await;
        drop(publisher);

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(1)).await;
        assert_eq!(
            next(&subscription, "the retained value").await,
            1,
            "{topology:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscription_over_two_publishers_reads_one_value_from_each() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "two_publishers");
        let mut publisher_sessions = Vec::new();
        for (instance, value) in [("left", 10), ("right", 20)] {
            let session = open_session(&router, topology).await;
            let publisher = declare(
                &session,
                &sender_from(instance, &topic),
                PublisherQoS::Standard,
                latest(1),
            )
            .expect("declare the retaining publisher");
            publish(&publisher, [value - 1, value]).await;
            publisher_sessions.push(session);
        }
        routed(&router, topology, &topic, &[10, 20]).await;

        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(1)).await;
        let mut values = next_values(&subscription, 2, "a retained value").await;
        values.sort_unstable();
        assert_eq!(values, [10, 20], "{topology:?}");
    }
}

/// The subscriber's queue tier holds 4 messages and the topic retains 16: the
/// whole replay is readable, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_depth_above_the_queue_tier_replays_in_full() {
    const DEPTH: usize = 16;
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "deep_replay");
        let publisher_session = open_session(&router, topology).await;
        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Important,
            latest(DEPTH),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, 1..=DEPTH as u64).await;

        let small_queues = SubscriberBufferSizes {
            standard: 4,
            high_throughput: 4,
        };
        let subscriber_session = open_session_with_buffers(&router, topology, small_queues).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(DEPTH)).await;
        assert_eq!(
            next_values(&subscription, DEPTH, "the retained values").await,
            (1..=DEPTH as u64).collect::<Vec<_>>(),
            "{topology:?}"
        );
    }
}

/// A session subscribes to a topic it retains itself, so the replay is
/// delivered on the declaring thread while the subscription is declared: the
/// queue holds all of it. The declaration runs on a thread of its own, and
/// the test bounds its wait for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_sessions_own_deep_replay_fits_its_subscription_queue() {
    const DEPTH: usize = 16;
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "own_deep_replay");
        let small_queues = SubscriberBufferSizes {
            standard: 4,
            high_throughput: 4,
        };
        let session = Arc::new(open_session_with_buffers(&router, topology, small_queues).await);
        let publisher = declare(
            &session,
            &sender(&topic),
            PublisherQoS::Important,
            latest(DEPTH),
        )
        .expect("declare the retaining publisher");
        publish(&publisher, 1..=DEPTH as u64).await;

        let (declared_tx, declared_rx) = tokio::sync::oneshot::channel();
        std::thread::spawn({
            let runtime = tokio::runtime::Handle::current();
            let session = Arc::clone(&session);
            let topic = topic.clone();
            move || {
                let subscription = runtime.block_on(subscribe(&session, &topic, latest(DEPTH)));
                // The test has gone when its bound ran out.
                let _ = declared_tx.send(subscription);
            }
        });
        let subscription = tokio::time::timeout(RECV_TIMEOUT, declared_rx)
            .await
            .unwrap_or_else(|_| panic!("{topology:?}: the replay did not fit the queue"))
            .expect("the declaring thread sends its subscription");
        assert_eq!(
            next_values(&subscription, DEPTH, "the retained values").await,
            (1..=DEPTH as u64).collect::<Vec<_>>(),
            "{topology:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_second_declaration_states_the_same_retention_and_qos() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    let mut session = open_session(&router, Topology::Router).await;
    let topic = sender("conflict");
    let _publisher = declare(&session, &topic, PublisherQoS::Standard, latest(1))
        .expect("declare the retaining publisher");
    declare(&session, &topic, PublisherQoS::Standard, latest(1))
        .expect("the same retention and QoS share the publisher");

    let refused = declare(&session, &topic, PublisherQoS::Standard, latest(3));
    assert!(
        matches!(
            refused,
            Err(PeppyMessagingInterfaceError::RetainingTopicMismatch { .. })
        ),
        "another depth should be refused"
    );

    let live_only_first = sender("live_first");
    declare(
        &session,
        &live_only_first,
        PublisherQoS::Standard,
        TopicRetention::LiveOnly,
    )
    .expect("a live-only declaration");
    let refused = declare(
        &session,
        &live_only_first,
        PublisherQoS::Standard,
        latest(1),
    );
    assert!(
        matches!(
            refused,
            Err(PeppyMessagingInterfaceError::RetainingTopicMismatch { .. })
        ),
        "a retaining declaration after a live-only one should be refused"
    );
    let one_shot = session
        .publish_topic(
            &topic,
            Payload::from_bytes(Bytes::from_static(b"9")),
            PublisherQoS::Standard,
            true,
        )
        .await;
    assert!(
        matches!(
            one_shot,
            Err(PeppyMessagingInterfaceError::RetainingTopicMismatch { .. })
        ),
        "a one-shot publish on a retaining topic should be refused"
    );
}

/// After a stop and a start, the session declares the topic with another
/// depth, and the first value a new subscriber reads is the new session's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn stopping_the_session_forgets_its_retaining_topics() {
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    for topology in TOPOLOGIES {
        let topic = topic_in(topology, "session_stop");
        let mut publisher_session = open_session(&router, topology).await;
        let earlier_publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(1),
        )
        .expect("declare the retaining publisher");
        publish(&earlier_publisher, [1]).await;

        publisher_session
            .stop_session()
            .await
            .expect("session should stop");
        publisher_session
            .start_session()
            .await
            .expect("session should start");

        let publisher = declare(
            &publisher_session,
            &sender(&topic),
            PublisherQoS::Standard,
            latest(3),
        )
        .expect("the stop freed the topic for another depth");
        let subscriber_session = open_session(&router, topology).await;
        let subscription = subscribe(&subscriber_session, &topic, latest(3)).await;
        wait_for_subscriber(&publisher_session, &topic).await;
        publish(&publisher, [9]).await;

        assert_eq!(
            next(&subscription, "the new session's value").await,
            9,
            "{topology:?}"
        );
    }
}

/// A session started again without a stop declares its topics anew, and the
/// first value a new subscriber reads is the new session's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn starting_the_session_again_forgets_its_retaining_topics() {
    const TOPIC: &str = "session_restart";
    let _lock = ZENOH_SERIAL.lock().await;
    let router = start_router().await;
    let mut publisher_session = open_session(&router, Topology::Router).await;
    let earlier_publisher = declare(
        &publisher_session,
        &sender(TOPIC),
        PublisherQoS::Standard,
        latest(1),
    )
    .expect("declare the retaining publisher");
    publish(&earlier_publisher, [1]).await;

    publisher_session
        .start_session()
        .await
        .expect("session should start again");

    let publisher = declare(
        &publisher_session,
        &sender(TOPIC),
        PublisherQoS::Standard,
        latest(3),
    )
    .expect("the start freed the topic for another depth");
    let subscriber_session = open_session(&router, Topology::Router).await;
    let subscription = subscribe(&subscriber_session, TOPIC, latest(3)).await;
    wait_for_subscriber(&publisher_session, TOPIC).await;
    publish(&publisher, [9]).await;
    assert_eq!(next(&subscription, "the new session's value").await, 9);
}

/// The router restarts under a publisher and a subscriber that keep their
/// sessions, and the publisher publishes during the outage. Once both
/// sessions are back the subscriber reads the current value, and none of the
/// values it missed before it, then the next publication.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_subscriber_that_reconnects_reads_the_current_value_then_live_values() {
    /// Covers the sessions' reconnect backoff; only a failure waits this long.
    const RECONNECT_TIMEOUT: Duration = Duration::from_secs(60);
    const TOPIC: &str = "reconnect";
    let _lock = ZENOH_SERIAL.lock().await;
    let mut router = start_router().await;
    let open_reconnecting_session = async |router: &ZenohdInstance| {
        started(
            adapter_for(router, Topology::Router, SubscriberBufferSizes::default())
                .with_session_reconnect(),
        )
        .await
    };

    let publisher_session = open_reconnecting_session(&router).await;
    let publisher = declare(
        &publisher_session,
        &sender(TOPIC),
        PublisherQoS::Standard,
        latest(1),
    )
    .expect("declare the retaining publisher");
    let subscriber_session = open_reconnecting_session(&router).await;
    let subscription = subscribe(&subscriber_session, TOPIC, latest(1)).await;
    publish(&publisher, [1]).await;
    assert_eq!(next(&subscription, "the value before the outage").await, 1);

    router
        .messenger()
        .stop_router()
        .await
        .expect("the router should stop");
    publish(&publisher, [2, 3]).await;
    router
        .messenger()
        .start_router()
        .await
        .expect("the router should start again");

    assert_eq!(
        next_within(
            RECONNECT_TIMEOUT,
            &subscription,
            "the current value after the reconnect"
        )
        .await,
        3,
        "the subscriber reads the current value, with no replay of the outage"
    );
    publish(&publisher, [4]).await;
    assert_eq!(next(&subscription, "the live value").await, 4);
}
