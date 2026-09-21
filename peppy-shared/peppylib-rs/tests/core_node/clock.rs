use std::time::Duration;

use config::node::QoSProfile;
use config::runtime::{
    ClockBinding, ClockDomainId, ClockIncarnation, CoreNodeName, Name, ProducerRef,
};
use core_node_api::encoding::{ClockResponse, ClockTick};
use core_node_api::{ServiceId, TopicId, names};
use peppylib::clock;
use peppylib::messaging::{MessengerHandle, SenderTarget, ServiceMessenger, TopicMessenger};
use peppylib::testing::EphemeralRouter;
use tempfile::TempDir;

use super::common::{
    CORE_NODE, SERVER_INSTANCE, publish_once, start_router_and_runner, test_node_target,
    wait_for_topic_subscriber, wait_until_reachable,
};

/// Spins up a single-shot `clock` service listener that returns `response`
/// verbatim. The handler decodes the inbound `ClockRequest` to assert wire
/// shape, even though it ignores the value.
async fn spawn_clock_stub_listener(server: MessengerHandle, response: ClockResponse) {
    let mut endpoint = ServiceMessenger::listen(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        ServiceId::Clock.name(),
    )
    .await
    .expect("listen should succeed");

    tokio::spawn(async move {
        endpoint
            .handle_next_request(|request| async move {
                let payload = request.message().payload();
                let _inbound = core_node_api::encoding::ClockRequest::decode(payload.as_ref())
                    .expect("decode ClockRequest");
                Ok(response.encode().expect("encode ClockResponse"))
            })
            .await
            .expect("handle_next_request should succeed");
    });
}

async fn setup_synchronize_stub(
    response: ClockResponse,
) -> (EphemeralRouter, TempDir, peppylib::runtime::NodeRunner) {
    let (router, temp_dir, node_runner, server) = start_router_and_runner().await;
    spawn_clock_stub_listener(server, response).await;
    wait_until_reachable(node_runner.messenger(), ServiceId::Clock.name()).await;
    (router, temp_dir, node_runner)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn synchronize_returns_typed_clock_sync() {
    // Canned t1/t2 are far smaller than the live `t0` from SystemTime::now(),
    // so the local clock leads the server and the offset must come out negative.
    let response = ClockResponse::new(0, 2_000_000_000_000, 2_000_000_000_005);

    let (_router, _temp_dir, node_runner) = setup_synchronize_stub(response.clone()).await;

    let sync = clock::synchronize(&node_runner, Some(Duration::from_secs(3)))
        .await
        .expect("synchronize should succeed");

    assert_eq!(sync.raw.server_recv_time, 2_000_000_000_000);
    assert_eq!(sync.raw.server_send_time, 2_000_000_000_005);
    assert!(
        sync.offset_ns < 0,
        "expected local clock to lead canned server time, got offset {} ns",
        sync.offset_ns,
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscribe_clock_yields_typed_ticks() {
    let (_router, _temp_dir, node_runner, server) = start_router_and_runner().await;

    // Subscribe via the high-level helper *before* publishing — otherwise the
    // first tick can land before zenoh discovery routes the subscription, and
    // the test races against propagation. With the subscription up first, any
    // tick published after the await point is delivered.
    let mut sub = clock::subscribe(&node_runner)
        .await
        .expect("subscribe_clock should succeed");

    // Deterministically wait until the publisher's session sees the subscription
    // (peer-mode discovery is not instantaneous) instead of guessing a fixed
    // settle delay, so the emit below cannot be dropped before routing.
    wait_for_topic_subscriber(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        TopicId::Clock.name(),
    )
    .await;

    let canned = ClockTick::new(1_700_000_000_123_456_789);
    publish_once(
        &server,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        TopicId::Clock.name(),
        QoSProfile::SensorData,
        canned.encode().expect("encode tick"),
    )
    .await
    .expect("emit should succeed");

    let tick = tokio::time::timeout(Duration::from_secs(2), sub.on_next_tick())
        .await
        .expect("tick should arrive within 2 s")
        .expect("on_next_tick should not error")
        .expect("subscription should not have closed");

    assert_eq!(tick, canned);
}

/// The instance a domain declaration names as its publisher.
const SOURCE_INSTANCE: &str = "sim_inst";

/// A domain on this test's machine, at the given lifetime.
fn domain(incarnation: u64) -> ClockDomainId {
    ClockDomainId::new(
        Name::new("robot").expect("valid name"),
        CoreNodeName::new(CORE_NODE).expect("valid core node name"),
        ClockIncarnation::try_from(incarnation).expect("non-zero"),
    )
}

fn source_ref() -> ProducerRef {
    ProducerRef::new(CORE_NODE, SOURCE_INSTANCE)
}

/// A standalone runner bound to `clock`, the daemon-less spelling of the
/// binding a launch or a `peppy node run` would have stamped on it.
async fn start_runner_with_clock(
    clock: ClockBinding,
) -> (
    EphemeralRouter,
    TempDir,
    peppylib::runtime::NodeRunner,
    MessengerHandle,
) {
    let router = EphemeralRouter::start().await.expect("start zenoh router");
    let observer = router.connect().await.expect("observer handle");
    let temp_dir = TempDir::new().expect("temp dir should be created");
    let peppy_config_path = super::common::write_standalone_peppy_config(&temp_dir);
    let standalone_config = peppylib::runtime::StandaloneConfig::new()
        .with_messaging(router.host(), router.port())
        .with_instance_id(super::common::CLIENT_INSTANCE)
        .with_clock(clock);
    let processor =
        peppylib::runtime::Processor::new_standalone(&peppy_config_path, &standalone_config)
            .expect("standalone processor");
    let node_runner = peppylib::runtime::NodeRunner::new(processor)
        .await
        .expect("node runner");
    (router, temp_dir, node_runner, observer)
}

/// Publishes one tick on `domain`'s stream as the instance that supplies it,
/// once a subscriber for that exact stream is routed. Reports whether one was.
async fn publish_domain_tick(
    observer: &MessengerHandle,
    domain: &ClockDomainId,
    time_ns: u64,
) -> bool {
    let link_id = domain.link_id();
    let target =
        SenderTarget::node(domain.core_node.as_str(), names::CORE_NODE_TAG).expect("valid target");
    let routed = TopicMessenger::wait_for_subscriber_with_link_id(
        observer,
        CORE_NODE,
        SOURCE_INSTANCE,
        target.clone(),
        Some(&link_id),
        TopicId::Clock.name(),
        Duration::from_secs(2),
    )
    .await
    .expect("waiting for a subscriber is not an error");
    let publisher = TopicMessenger::declare_publisher(
        observer,
        CORE_NODE,
        SOURCE_INSTANCE,
        target,
        Some(&link_id),
        TopicId::Clock.name(),
        QoSProfile::SensorData,
    )
    .await
    .expect("declare the domain's publisher");
    publisher
        .publish(ClockTick::new(time_ns).encode().expect("encode tick"))
        .await
        .expect("publish the tick");
    routed
}

/// Reads `now_ns` until it answers or the budget runs out: the feeder task
/// caches a tick a moment after the wire delivers it.
async fn wait_for_now_ns(clock: &clock::PeppyClock) -> Option<u64> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while tokio::time::Instant::now() < deadline {
        if let Ok(now) = clock.now_ns() {
            return Some(now);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    None
}

/// A consumer reads the instants its domain's publisher supplies, and reads
/// nothing before the first one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_reads_the_ticks_of_its_domain() {
    const SIM_NS: u64 = 42_000_000_000;
    let (_router, _dir, runner, observer) =
        start_runner_with_clock(ClockBinding::consumer(domain(1), source_ref())).await;

    let clock = clock::for_node(&runner).await.expect("build the clock");
    assert!(
        clock.now_ns().is_err(),
        "a domain that has not ticked is not ready"
    );

    assert!(
        publish_domain_tick(&observer, &domain(1), SIM_NS).await,
        "the consumer's subscription must be routed before the tick goes out"
    );

    assert_eq!(
        wait_for_now_ns(&clock).await,
        Some(SIM_NS),
        "the consumer reads the instant its publisher supplied"
    );
}

/// Reusing a domain name mints a new lifetime, and a consumer left on the old
/// one reads nothing from it: the replacement's ticks address a stream the old
/// consumer never subscribed to. This is what stops a replacement from
/// silently rebinding the consumers of the domain it replaces.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_of_an_earlier_lifetime_reads_nothing_from_the_replacement() {
    let (_router, _dir, runner, observer) =
        start_runner_with_clock(ClockBinding::consumer(domain(1), source_ref())).await;

    let clock = clock::for_node(&runner).await.expect("build the clock");

    // The replacement publishes under the same name on the same machine.
    publish_domain_tick(&observer, &domain(2), 99_000_000_000).await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    assert!(
        clock.now_ns().is_err(),
        "a replacement's ticks must not reach a consumer of the earlier lifetime"
    );
}

/// The instance a domain names supplies its time: it reads back what it
/// committed before anything returns through the transport, and its domain
/// carries that same instant to whoever reads it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publisher_reads_its_committed_time_and_its_domain_carries_it() {
    const SIM_NS: u64 = 7_000_000_000;
    let (_router, _dir, runner, _observer) =
        start_runner_with_clock(ClockBinding::publisher(domain(1))).await;

    let publisher = clock::ClockPublisher::for_node(&runner)
        .await
        .expect("asking is not an error")
        .expect("the binding names this instance the publisher");
    assert_eq!(publisher.domain(), &domain(1));

    let reader = clock::for_node(&runner).await.expect("build the clock");
    assert!(
        reader.now_ns().is_err(),
        "a publisher that has committed nothing is not ready"
    );

    // Its own subscription, which must see exactly what it sends.
    let mut ticks = clock::subscribe(&runner).await.expect("subscribe");

    publisher
        .publish(SIM_NS)
        .await
        .expect("publish the instant");

    assert_eq!(
        reader.now_ns().expect("the committed instant reads back"),
        SIM_NS,
        "a source reads its own clock without waiting for a transport echo"
    );

    let tick = tokio::time::timeout(Duration::from_secs(5), ticks.on_next_tick())
        .await
        .expect("a tick arrives within 5 s")
        .expect("on_next_tick should not error")
        .expect("the subscription stays open");
    assert_eq!(tick.time(), SIM_NS);
}

/// An instance whose deployment named it no domain gets no publisher, and with
/// it no way to drive anyone's time.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_instance_named_no_domain_cannot_publish() {
    let (_router, _temp_dir, node_runner, _server) = start_router_and_runner().await;

    let publisher = clock::ClockPublisher::for_node(&node_runner)
        .await
        .expect("asking is not an error");
    assert!(
        publisher.is_none(),
        "a wall-time instance must get no publisher"
    );
}

/// A consumer is bound to a domain, not granted authority over it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_consumer_cannot_publish_its_domain() {
    let (_router, _dir, runner, _observer) =
        start_runner_with_clock(ClockBinding::consumer(domain(1), source_ref())).await;

    let publisher = clock::ClockPublisher::for_node(&runner)
        .await
        .expect("asking is not an error");
    assert!(
        publisher.is_none(),
        "binding to a domain grants no authority to supply it"
    );
}

/// A wall-time instance reads its daemon's ticks and nobody else's. A domain
/// hosted on the same machine publishes on the same topic and target, under
/// its own `link_id`, and a wall subscription pinned to the daemon's reserved
/// segment neither routes to it nor carries its instants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_wall_subscription_reads_only_its_daemons_ticks() {
    const WALL_NS: u64 = 1_700_000_000_123_456_789;
    const DOMAIN_NS: u64 = 42_000_000_000;
    let (_router, _dir, runner, observer) = start_runner_with_clock(ClockBinding::Wall).await;

    let mut ticks = clock::subscribe(&runner).await.expect("subscribe");

    assert!(
        !publish_domain_tick(&observer, &domain(1), DOMAIN_NS).await,
        "a wall subscription must not route to a domain's stream"
    );

    wait_for_topic_subscriber(
        &observer,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        TopicId::Clock.name(),
    )
    .await;
    publish_once(
        &observer,
        CORE_NODE,
        SERVER_INSTANCE,
        test_node_target(CORE_NODE),
        TopicId::Clock.name(),
        QoSProfile::SensorData,
        ClockTick::new(WALL_NS).encode().expect("encode tick"),
    )
    .await
    .expect("publish the wall tick");

    // The domain's tick went out first, so a subscription that matched its
    // stream would deliver it before the wall tick published here.
    let tick = tokio::time::timeout(Duration::from_secs(5), ticks.on_next_tick())
        .await
        .expect("a tick arrives within 5 s")
        .expect("on_next_tick should not error")
        .expect("the subscription stays open");
    assert_eq!(
        tick.time(),
        WALL_NS,
        "a wall subscription read a simulated domain's instant"
    );
}
