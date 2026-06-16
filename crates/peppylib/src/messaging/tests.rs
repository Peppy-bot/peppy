// The crate sets `#![deny(unsafe_code)]` in lib.rs. This test module is the one
// place that needs `unsafe`, in exactly two pre-main helpers below: the
// `#[ctor::ctor(unsafe)]` that sets `ZENOH_RUNTIME` before zenoh's lazy global
// runtime initializes, and the `libc` getrlimit/setrlimit FFI that raises the
// fd limit to avoid EMFILE flakes. Both are load-bearing and have no safe
// equivalent. Each helper carries its own `#[allow(unsafe_code)]` so the
// crate-wide deny still guards the rest of this file against accidental unsafe.

use crate::types::Payload;
use config::node::QoSProfile;
use pmi::{MessengerBackend, ZenohAdapter, ZenohdInstance};
use rand::SeedableRng;
use rand::seq::SliceRandom;
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Once};
use std::time::Duration;
use tokio::sync::oneshot;

use crate::error::Error;
use crate::messaging::{
    ActionMessenger, ConsumerFilter, MessengerHandle, ProducerRef, ResultStatus, SenderTarget,
    ServiceMessenger, ServiceTarget, TopicMessenger,
};

/// Builds a node-shaped [`SenderTarget`] with the standard test tag. Panics on
/// invalid names — tests use known-good values only.
fn test_node_target(name: &str) -> SenderTarget {
    SenderTarget::node(name, "v1").expect("test node target")
}

/// Declares a publisher and publishes a single payload. The publisher is the
/// only topic-publish path, so a test that publishes once just declares then
/// publishes; the arguments mirror the old one-shot emit.
#[allow(clippy::too_many_arguments)]
async fn publish_once(
    messenger: &MessengerHandle,
    as_core_node: &str,
    as_instance_id: &str,
    as_target: SenderTarget,
    as_topic_name: &str,
    qos: QoSProfile,
    payload: Payload,
) -> Result<(), Error> {
    let publisher = TopicMessenger::declare_publisher(
        messenger,
        as_core_node,
        as_instance_id,
        as_target,
        None,
        as_topic_name,
        qos,
    )
    .await?;
    publisher.publish(payload).await
}

#[derive(Clone)]
struct ActionClientCase {
    client_id: String,
    goal: Payload,
    goal_response: Payload,
    feedback: Payload,
}

impl ActionClientCase {
    fn new(prefix: &str, idx: usize) -> Self {
        let client_id = format!("{prefix}_{idx}");
        let goal = Payload::from(format!("client={client_id};goal_request={idx}").into_bytes());
        let goal_response =
            Payload::from(format!("client={client_id};goal_response=accepted").into_bytes());
        let feedback =
            Payload::from(format!("client={client_id};feedback=progress-{idx}").into_bytes());

        Self {
            client_id,
            goal,
            goal_response,
            feedback,
        }
    }
}

/// Pre-main: gives zenoh's global Net runtime more worker threads for this
/// test binary. Stock zenoh 1.9.0 can deadlock its routing layer under the
/// peer-session churn these tests generate: a thread holding the routing
/// `ctrl_lock` parks in `block_in_place` waiting on the StartConditions
/// mutex while the Net runtime's single default worker blocks on that same
/// `ctrl_lock`, wedging the mutex queue (fix pending upstream in
/// https://github.com/eclipse-zenoh/zenoh/pull/2637). With more Net workers
/// a free worker can always drain the mutex queue, which un-parks the lock
/// holder; 80 runs of the three churn-heaviest tests reproduced no hang with
/// 4 workers, versus a hang within ~23 runs on the single-worker default.
///
/// Runs before `main` so the variable is set before libtest spawns any
/// thread and before zenoh's lazy global runtimes read it. Spawned zenohd
/// child processes inherit it, which is harmless. An operator-provided
/// `ZENOH_RUNTIME` wins. Remove once the upstream fix ships in a release.
#[allow(unsafe_code)]
#[ctor::ctor(unsafe)]
fn ensure_zenoh_net_runtime_workers() {
    if std::env::var_os("ZENOH_RUNTIME").is_none() {
        // SAFETY: runs pre-main on the only live thread, so no other thread
        // can concurrently read or write the process environment.
        unsafe { std::env::set_var("ZENOH_RUNTIME", "(net: (worker_threads: 4))") };
    }
}

/// Raises the process soft `nofile` limit once per test binary. Each test below
/// spawns an ephemeral zenoh router, and running them in parallel can exhaust
/// file descriptors under the macOS default soft limit of 256, surfacing as
/// flaky `Too many open files` (EMFILE) errors. Bumping the soft limit toward
/// the hard limit removes that ceiling without reducing test parallelism. Best
/// effort: a failed syscall leaves the original limit in place and the real
/// EMFILE error still surfaces.
#[allow(unsafe_code)]
fn ensure_test_fd_limit() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        // 8192 is comfortably above the peak concurrent router count and well
        // under the macOS per-process cap (kern.maxfilesperproc).
        const DESIRED_SOFT: libc::rlim_t = 8192;
        // SAFETY: get/setrlimit operate on a stack-allocated rlimit and report
        // failure through their return code, which we honor.
        unsafe {
            let mut limit = libc::rlimit {
                rlim_cur: 0,
                rlim_max: 0,
            };
            if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
                return;
            }
            let target = DESIRED_SOFT.min(limit.rlim_max);
            if limit.rlim_cur >= target {
                return;
            }
            limit.rlim_cur = target;
            let _ = libc::setrlimit(libc::RLIMIT_NOFILE, &limit);
        }
    });
}

/// Serializes the zenoh router/peer tests in this binary. Running several
/// independent peer meshes at once starves peer-mode gossip discovery (every
/// peer opens listeners and forms links), which makes cold-start delivery flaky;
/// one mesh at a time keeps discovery fast and deterministic. Mirrors pmi's
/// `ZENOH_SERIAL`. The guard is held for each test's lifetime via the field
/// below, so acquiring the context is all a test needs to opt in.
///
/// KNOWN FLAKE: zenoh 1.9.0 can deadlock its routing layer under the
/// peer-session churn these tests generate (see
/// [`ensure_zenoh_net_runtime_workers`], which suppresses the trigger for
/// this binary). If it ever fires anyway, the running test hangs forever and
/// every later test queues on this mutex, so the whole binary looks stuck
/// ("test has been running for over 60 seconds" for several tests at once).
/// It cannot be contained in-process: session teardown needs the deadlocked
/// locks. Kill the run and retry; do not debug peppyos for it.
static ZENOH_SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

struct TestRouterContext {
    instance: ZenohdInstance,
    _serial: tokio::sync::MutexGuard<'static, ()>,
}

impl TestRouterContext {
    async fn start() -> Self {
        let serial = ZENOH_SERIAL.lock().await;
        ensure_test_fd_limit();
        let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
            .await
            .expect("failed to start zenoh router for tests");
        Self {
            instance,
            _serial: serial,
        }
    }

    fn host(&self) -> &str {
        &self.instance.host
    }

    fn port(&self) -> u16 {
        self.instance.port
    }

    fn connection_target(&self) -> (String, u16) {
        (self.instance.host.clone(), self.instance.port)
    }

    async fn messenger(&self) -> MessengerHandle {
        connect_messenger(self.host(), self.port()).await
    }

    async fn shutdown(mut self) {
        self.instance
            .messenger()
            .stop_router()
            .await
            .expect("Failed to shutdown router");
    }
}

async fn connect_messenger(host: &str, port: u16) -> MessengerHandle {
    const MAX_RETRIES: u32 = 5;
    const RETRY_DELAY: Duration = Duration::from_millis(200);

    let mut last_error = None;
    for attempt in 0..MAX_RETRIES {
        match MessengerHandle::from_host_port(host, port).await {
            Ok(handle) => return handle,
            Err(error) => {
                last_error = Some(error);
                if attempt + 1 < MAX_RETRIES {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
            }
        }
    }

    panic!(
        "failed to connect messenger to {host}:{port} after {MAX_RETRIES} attempts: {:?}",
        last_error.unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_no_from_instance_id() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";
    let payload = Payload::from_static(b"A message");

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_handle = router.messenger().await;
    let subscriber_instance_id = "subscriber_instance";
    let mut subscription = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        true, // from_any pattern
        topic,
        &ConsumerFilter::Any,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    let emitter_core_node = "core_node_emit";
    let emitter_instance_id = "emitter_instance";
    let emitter_handle = router.messenger().await;
    // Deterministically wait for the subscriber before the first publish so it
    // is not dropped during peer-mode discovery propagation.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle,
        emitter_core_node,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");
    publish_once(
        &emitter_handle,
        emitter_core_node,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    assert_eq!(received.instance_id(), emitter_instance_id);
    assert_eq!(received.core_node(), emitter_core_node);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_with_from_instance_id() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";

    // Use the same core_node for both emitters to isolate instance_id filtering
    let emitter_core_node = "core_node_emit";

    // The messages emitted from this instance_id will never be received by any subscriber
    let emitter_instance_id1 = "emitter_instance1";

    // The messages emitted from this instance_id will be received by a subscriber
    let emitter_instance_id2 = "emitter_instance2";

    let payload = Payload::from_static(b"A message");

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_handle = router.messenger().await;

    let subscriber_instance_id1 = "subscriber_instance1";
    let filter1 = ConsumerFilter::Pin(ProducerRef::new(emitter_core_node, emitter_instance_id1));
    let mut subscription1 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id1,
        Some(test_node_target(node_name)),
        false,
        topic,
        &filter1,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Only this subscriber will receive a message
    let subscriber_instance_id2 = "subscriber_instance2";
    let filter2 = ConsumerFilter::Pin(ProducerRef::new(emitter_core_node, emitter_instance_id2));
    let mut subscription2 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node,
        subscriber_instance_id2,
        Some(test_node_target(node_name)),
        false,
        topic,
        &filter2,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    let emitter_handle1 = router.messenger().await;
    // Deterministically wait for the matching subscriber (subscription2) before
    // the first publish so it is not dropped during peer-mode discovery.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle1,
        emitter_core_node,
        emitter_instance_id2,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");
    publish_once(
        &emitter_handle1,
        emitter_core_node,
        emitter_instance_id2,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription2.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    // The first subscriber should never receive a message
    let timeout_result =
        tokio::time::timeout(Duration::from_secs(2), subscription1.on_next_message()).await;
    assert!(
        timeout_result.is_err(),
        "subscription1 should not receive any message"
    );

    // Only receive from emitter with instance_id2
    assert_eq!(received.core_node(), emitter_core_node);
    assert_eq!(received.instance_id(), emitter_instance_id2);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_publish_subscribe_with_from_core_node() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";

    // The messages emitted from this one will never be received by any subscriber
    let emitter_core_node1 = "core_node_emit1";
    let emitter_instance_id = "emitter_instance";

    // The messages emitted from this one will be received by a subscriber
    let emitter_core_node2 = "core_node_emit2";

    let payload = Payload::from_static(b"A message");

    // Same instance_id for every subscriber
    let subscriber_instance_id = "subscriber_instance";
    let subscriber_handle = router.messenger().await;

    let subscriber_core_node1 = "core_node_subscribe1";
    let filter_core1 =
        ConsumerFilter::Pin(ProducerRef::new(emitter_core_node1, emitter_instance_id));
    let mut subscription1 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node1,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        false,
        topic,
        &filter_core1,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    // Only this subscriber will receive a message
    let subscriber_core_node2 = "core_node_subscribe2";
    let filter_core2 =
        ConsumerFilter::Pin(ProducerRef::new(emitter_core_node2, emitter_instance_id));
    let mut subscription2 = TopicMessenger::subscribe(
        &subscriber_handle,
        subscriber_core_node2,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        false,
        topic,
        &filter_core2,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    let emitter_handle1 = router.messenger().await;
    // Deterministically wait for the matching subscriber (subscription2) before
    // the first publish so it is not dropped during peer-mode discovery.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle1,
        emitter_core_node2,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");
    publish_once(
        &emitter_handle1,
        emitter_core_node2,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        qos,
        payload.clone(),
    )
    .await
    .expect("Should send the payload");

    let received = tokio::time::timeout(Duration::from_secs(2), subscription2.on_next_message())
        .await
        .expect("Timed out waiting for published message")
        .expect("Should receive the published message");

    // The first subscriber should never receive a message
    let timeout_result =
        tokio::time::timeout(Duration::from_secs(2), subscription1.on_next_message()).await;
    assert!(
        timeout_result.is_err(),
        "subscription1 should not receive any message"
    );

    // Only receive from emitter 2
    assert_eq!(received.core_node(), emitter_core_node2);
    assert_eq!(received.instance_id(), emitter_instance_id);
    assert_eq!(received.payload(), &payload);

    router.shutdown().await;
}

/// `ConsumerFilter::OnlyFrom(set)` resolves at the messaging layer to a
/// wire wildcard + an in-process accept set. The subscriber must only
/// surface messages from producers whose `instance_id` is in the set,
/// while wire-level reception happens for every matching `(name, tag)`
/// publisher.
///
/// Spec coverage: `FromAnyBound` consumer slot under the new dispatch
/// model — `from_any: true` slot bound via free-form keys to producers
/// P1 and P2. A third producer P3 of the same `(name, tag)` must not
/// reach the consumer.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn consumer_filter_only_from_set_admits_listed_producers_and_drops_others() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";
    let core = "shared_core";

    let p1 = "cam_p1";
    let p2 = "cam_p2";
    let p3 = "cam_p3";

    let subscriber_handle = router.messenger().await;
    let filter =
        ConsumerFilter::OnlyFrom(vec![ProducerRef::new(core, p1), ProducerRef::new(core, p2)]);
    let mut sub = TopicMessenger::subscribe(
        &subscriber_handle,
        core,
        "consumer_inst",
        Some(test_node_target(node_name)),
        true,
        topic,
        &filter,
        qos.clone(),
    )
    .await
    .expect("subscribe should succeed");

    let emitter_handle = router.messenger().await;
    // Deterministically wait until the subscriber is known to this fresh emitter
    // peer before publishing, so the first emits are not dropped during peer-mode
    // discovery propagation. The from_any subscriber matches any producer, so
    // waiting on one producer's key expression confirms it is reachable.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle,
        core,
        p1,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");

    for (producer, body) in [
        (p1, b"from-p1".as_ref()),
        (p3, b"from-p3"),
        (p2, b"from-p2"),
    ] {
        publish_once(
            &emitter_handle,
            core,
            producer,
            test_node_target(node_name),
            topic,
            qos.clone(),
            Payload::from(body.to_vec()),
        )
        .await
        .expect("emit should succeed");
    }

    // Collect every message that arrives within a generous budget; the
    // local accept set must drop P3 silently.
    let mut got: Vec<(String, Vec<u8>)> = Vec::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    while tokio::time::Instant::now() < deadline {
        match tokio::time::timeout(Duration::from_millis(250), sub.on_next_message()).await {
            Ok(Some(msg)) => got.push((
                msg.instance_id().to_string(),
                msg.payload().as_ref().to_vec(),
            )),
            Ok(None) => break,
            Err(_) => break,
        }
        if got.len() >= 2 {
            break;
        }
    }
    got.sort();

    assert_eq!(
        got,
        vec![
            (p1.to_string(), b"from-p1".to_vec()),
            (p2.to_string(), b"from-p2".to_vec()),
        ],
        "OnlyFrom must accept only listed producers; P3 must be dropped",
    );

    router.shutdown().await;
}

/// `ConsumerFilter::AnyExcept(set)` is the unbound `from_any` slot's
/// wire wildcard with an in-process reject set populated from sibling
/// claims. The subscriber must drop messages from producers in the set
/// (claimed by pinned or from_any-explicit siblings) while still
/// receiving from any other matching producer.
///
/// Spec coverage: Statement 3 precedence — pinned-bound or
/// from_any-explicit siblings preempt unbound from_any per `(name,
/// tag)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn consumer_filter_any_except_drops_excluded_and_admits_rest() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "uvc_camera";
    let topic = "video_stream";
    let core = "shared_core";

    // P1 is claimed by a pinned sibling (simulated here as the
    // exclusion set on the unbound from_any consumer). P2 is unclaimed
    // and must reach the wildcard fallback.
    let claimed = "cam_pinned_claim";
    let unclaimed = "cam_unclaimed";

    let subscriber_handle = router.messenger().await;
    let filter = ConsumerFilter::AnyExcept(vec![ProducerRef::new(core, claimed)]);
    let mut sub = TopicMessenger::subscribe(
        &subscriber_handle,
        core,
        "consumer_inst",
        Some(test_node_target(node_name)),
        true,
        topic,
        &filter,
        qos.clone(),
    )
    .await
    .expect("subscribe should succeed");

    let emitter_handle = router.messenger().await;
    // Wait until the subscriber is known to this fresh emitter peer before
    // publishing (peer-mode discovery is not instantaneous). The from_any
    // subscription matches any producer, so waiting on one suffices.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle,
        core,
        unclaimed,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");

    for (producer, body) in [
        (claimed, b"from-claimed".as_ref()),
        (unclaimed, b"from-unclaimed"),
    ] {
        publish_once(
            &emitter_handle,
            core,
            producer,
            test_node_target(node_name),
            topic,
            qos.clone(),
            Payload::from(body.to_vec()),
        )
        .await
        .expect("emit should succeed");
    }

    let received = tokio::time::timeout(Duration::from_secs(2), sub.on_next_message())
        .await
        .expect("at least one message should reach the wildcard fallback")
        .expect("subscription should not close");
    assert_eq!(received.instance_id(), unclaimed);
    assert_eq!(received.payload().as_ref(), b"from-unclaimed");

    // No further message should arrive — the claimed producer's emit is
    // dropped by the in-process filter.
    let extra = tokio::time::timeout(Duration::from_millis(500), sub.on_next_message()).await;
    assert!(
        extra.is_err(),
        "claimed producer must not reach unbound from_any; got: {extra:?}",
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn topic_publish_reliable_5000hz_messages() {
    let router = TestRouterContext::start().await;

    let node_name = "uvc_camera";
    let topic = "video_stream";
    let qos = QoSProfile::Reliable;

    let sender_handle = router.messenger().await;
    let receiver_handle = router.messenger().await;

    let subscriber_core_node = "core_node_subscribe";
    let subscriber_instance_id = "subscriber_instance";
    let mut subscription = TopicMessenger::subscribe(
        &receiver_handle,
        subscriber_core_node,
        subscriber_instance_id,
        Some(test_node_target(node_name)),
        true,
        topic,
        &ConsumerFilter::Any,
        qos.clone(),
    )
    .await
    .expect("Should subscribe to the topic");

    let message_count = 5000;
    let emitter_core_node = "emitter_core_node";
    let emitter_instance_id = "emitter_instance";
    let mut message_ids: Vec<u32> = (0..message_count as u32).collect();
    // Fixed seed so a failure reproduces the same id ordering run to run, while
    // still exercising out-of-order payload contents.
    let mut rng = rand::rngs::StdRng::seed_from_u64(0xC0FFEE);
    message_ids.shuffle(&mut rng);

    // Deterministically wait for the subscriber before the publish loop so the
    // first messages are not dropped during peer-mode discovery propagation.
    TopicMessenger::wait_for_subscriber(
        &sender_handle,
        emitter_core_node,
        emitter_instance_id,
        test_node_target(node_name),
        topic,
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");

    // Drain concurrently with publishing. Peer mode removes the router buffer
    // that used to sit between publisher and subscriber, so publishing all
    // messages before draining would block a Reliable (Block-congestion)
    // publisher once the subscriber's bounded channel fills. A real stream is
    // drained continuously, so emit in a background task while the main task
    // receives.
    let emit_ids = message_ids.clone();
    let emitter = tokio::spawn(async move {
        for &message_id in &emit_ids {
            let payload = Payload::from(message_id.to_le_bytes().to_vec());
            publish_once(
                &sender_handle,
                emitter_core_node,
                emitter_instance_id,
                test_node_target(node_name),
                topic,
                qos.clone(),
                payload,
            )
            .await
            .expect("Should send the payload");
        }
    });

    // Identity check runs once on the first received message — the wire-format
    // contract is pinned in `pmi::wire::zenoh_format::tests`, so this loop only needs
    // to verify peppylib-level addressing and ordering.
    let mut received_ids: Vec<u32> = Vec::with_capacity(message_count);
    let mut identity_checked = false;
    for _ in 0..message_count {
        let message = tokio::time::timeout(Duration::from_secs(2), subscription.on_next_message())
            .await
            .expect("Timed out waiting for a message")
            .expect("Subscription closed before receiving all messages");

        if !identity_checked {
            assert_eq!(message.core_node(), emitter_core_node);
            assert_eq!(message.instance_id(), emitter_instance_id);
            identity_checked = true;
        }

        let payload = message.payload();
        let payload_bytes = payload.as_ref();
        assert_eq!(
            payload_bytes.len(),
            std::mem::size_of::<u32>(),
            "Payload should encode the message index"
        );

        let mut id_bytes = [0u8; std::mem::size_of::<u32>()];
        id_bytes.copy_from_slice(payload_bytes.as_ref());
        let received_id = u32::from_le_bytes(id_bytes);

        received_ids.push(received_id);
    }

    emitter.await.expect("emitter task should not panic");

    assert_eq!(
        received_ids.len(),
        message_count,
        "should receive exactly {} messages",
        message_count
    );

    let mut expected_sorted = message_ids.clone();
    expected_sorted.sort_unstable();
    received_ids.sort_unstable();

    assert_eq!(
        received_ids, expected_sorted,
        "should receive every published message exactly once"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_communication_poll_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx1, service_ready_rx1) = oneshot::channel();
    let (service_ready_tx2, service_ready_rx2) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    let listener_core_node1 = "listener_core_node1";
    let listener_instance_id1 = "listener_instance1";
    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task1 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node1,
            listener_instance_id1,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx1.send(()).unwrap();
            // The handler may or may not be invoked depending on which
            // listener wins the discovery probe race; both outcomes are
            // valid. The `call_count == 1` assertion at the end verifies
            // that exactly one of the two listener handlers ran.
            let _ = tokio::time::timeout(service_wait_timeout, handler).await;

            Ok::<(), Error>(())
        })
    };

    // Second listener with the same service shape. Discovery sends a probe
    // to both listeners; the probe is auto-replied in the request loop
    // before the user handler runs, so the winner is whichever probe reply
    // reaches the caller first — a race with no inherent ordering.
    // Whichever listener loses, its user handler simply never executes.
    let listener_core_node2 = "listener_core_node2";
    let listener_instance_id2 = "listener_instance2";
    let service_task2 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node2,
            listener_instance_id2,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx2.send(()).unwrap();
            let _ = tokio::time::timeout(service_wait_timeout, handler).await;

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx1)
        .await
        .expect("service 1 should signal readiness before timeout")
        .expect("service 1 should signal readiness");

    tokio::time::timeout(service_ready_timeout, service_ready_rx2)
        .await
        .expect("service 2 should signal readiness before timeout")
        .expect("service 2 should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any, // Fully wildcard: we don't pin any target producer
            request_payload.clone(),
            Duration::from_secs(2),
        )
        .await
        .expect("caller should receive response");

        // Discovery picks whichever listener replies to the probe first;
        // that is a wire-level race with no inherent ordering, so either
        // listener is a valid winner. We assert the response matches the
        // winning listener's identity and that exactly one user handler
        // ran (see `call_count` check below).
        let winning_core_node = if response.instance_id() == listener_instance_id1 {
            listener_core_node1
        } else if response.instance_id() == listener_instance_id2 {
            listener_core_node2
        } else {
            panic!(
                "response should come from one of the two listeners, got instance_id={}",
                response.instance_id()
            );
        };
        assert_eq!(response.core_node(), winning_core_node);
        assert_eq!(response.payload(), &response_payload);
    }

    tokio::time::timeout(service_task_timeout, service_task1)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, service_task2)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Only the fastest responder ran its user handler. `ServiceMessenger::poll`'s
    // discover-then-pin sequence sends a lightweight probe first (filtered
    // server-side before the user handler runs), then dispatches the real
    // request pinned to the first responding producer. Without discovery,
    // both producers' handlers would have run.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "only the discovered producer should run the user handler",
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_communication_poll_specific_instance_id() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx1, service_ready_rx1) = oneshot::channel();
    let (service_ready_tx2, service_ready_rx2) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_secs(1);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    // This listener is not our target
    let listener_core_node1 = "listener_core_node1";
    let listener_instance_id1 = "listener_instance1";
    let service_task1 = {
        let service_expose_handle = router.messenger().await;
        // This listener is not supposed to receive any message
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node1,
            listener_instance_id1,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        tokio::spawn(async move {
            service_ready_tx1.send(()).unwrap();

            let outcome = tokio::time::timeout(
                service_wait_timeout,
                service.handle_next_request(|_request| async {
                    Ok(Payload::from_static(b"unexpected response"))
                }),
            )
            .await;

            if outcome.is_err() {
                return Ok(()); // Timeout is expected - no request should be received
            }
            outcome.unwrap().map_or_else(Err, |handled| {
                panic!("non-targeted service should not receive the request (handled={handled})")
            })
        })
    };

    // Creates a second listener with a different ID (emulates a second instance). This is our target
    let listener_core_node2 = "listener_core_node2";
    let listener_instance_id2 = "listener_instance2";
    let service_task2 = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node2,
            listener_instance_id2,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let request_payload = request_payload.clone();
        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    // This second service instance is a bit slow for processing, but since it's been targeted, it's gonna be the one that responds
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    Ok(response_payload)
                }
            });

            service_ready_tx2.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler)
                .await
                .expect("service handler timed out");
            let handled = handled.expect("service should receive exactly one request");

            assert!(
                handled,
                "service subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx1)
        .await
        .expect("service 1 should signal readiness before timeout")
        .expect("service 1 should signal readiness");
    tokio::time::timeout(service_ready_timeout, service_ready_rx2)
        .await
        .expect("service 2 should signal readiness before timeout")
        .expect("service 2 should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            // We pin listener 2's producer as the target
            ServiceTarget::Producer(&ProducerRef::new(
                listener_core_node2,
                listener_instance_id2,
            )),
            request_payload.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("caller should receive response");

        // Listener instance 2 is supposed to have responded since it's the target
        assert_eq!(response.instance_id(), listener_instance_id2);
        assert_eq!(response.core_node(), listener_core_node2);
        assert_eq!(response.payload(), &response_payload);
    }

    tokio::time::timeout(service_task_timeout, service_task1)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    tokio::time::timeout(service_task_timeout, service_task2)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Ensure the service callback was called exactly once (otherwise that means both services received the request)
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "service callback should have been called exactly once"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_wrong_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|_request| {
                let response_payload = Payload::from_static(b"ack");
                async move {
                    // This closure should never be called in this test since
                    // we're targeting the wrong node
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler).await;

            // The service is targeted at the wrong node, so the user handler
            // must never run. A correct run either leaves the handler parked
            // until `service_wait_timeout` fires (`Err(Elapsed)`) or finds the
            // request stream already closed (`Ok(false)`); both mean no request
            // was delivered. Only `Ok(true)` — a request actually reaching the
            // handler — is a failure. `call_count == 0` below is the
            // authoritative guarantee that nothing was processed.
            match handled {
                Err(_) | Ok(Ok(false)) | Ok(Err(_)) => {}
                Ok(Ok(true)) => {
                    panic!("service handler processed a request despite the wrong target")
                }
            }

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx)
        .await
        .expect("service should signal readiness before timeout")
        .expect("service should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;
        let err = {
            let result = ServiceMessenger::poll(
                &caller_handle,
                CALLER_CORE_NODE,
                CALLER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_service_name,
                // Use a wrong instance_id here (the core_node is the real one)
                ServiceTarget::Producer(&ProducerRef::new(listener_core_node, "wrong_node")),
                request_payload.clone(),
                Duration::from_secs(1),
            )
            .await;

            let Err(err) = result else {
                panic!("service call should fail when targeting the wrong node");
            };

            err
        };

        let Error::ServiceUnreachable {
            instance_id: err_instance_id,
            service_name: err_service_name,
        } = &err
        else {
            panic!(
                "expected ServiceUnreachable error, received unexpected error: {:?}",
                err
            );
        };

        assert_eq!(err_instance_id.as_deref(), Some("wrong_node"));
        assert_eq!(err_service_name.as_str(), listener_service_name);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "service should not be called when targeting the wrong instance"
        );
    }

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Authoritative check that the user handler never ran — independent of
    // whether the listener timed out or its stream closed first. Asserting only
    // after the service task has joined guarantees the handler future is fully
    // resolved, so a late increment cannot slip past this check.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "service callback should not have been called"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_wrong_core_node() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");

    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let handler = service.handle_next_request(|_request| {
                let response_payload = Payload::from_static(b"ack");
                async move {
                    // This closure should never be called in this test since
                    // we're targeting the wrong node
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            service_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(service_wait_timeout, handler).await;

            // The service is targeted at the wrong node, so the user handler
            // must never run. A correct run either leaves the handler parked
            // until `service_wait_timeout` fires (`Err(Elapsed)`) or finds the
            // request stream already closed (`Ok(false)`); both mean no request
            // was delivered. Only `Ok(true)` — a request actually reaching the
            // handler — is a failure. `call_count == 0` below is the
            // authoritative guarantee that nothing was processed.
            match handled {
                Err(_) | Ok(Ok(false)) | Ok(Err(_)) => {}
                Ok(Ok(true)) => {
                    panic!("service handler processed a request despite the wrong target")
                }
            }

            Ok::<(), Error>(())
        })
    };

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        tokio::time::timeout(service_ready_timeout, service_ready_rx)
            .await
            .expect("service should signal readiness before timeout")
            .expect("service should signal readiness");

        // No settle sleep: the poll below self-retries on a cold-start miss.
        let caller_handle = router.messenger().await;
        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            // Pin a producer on the wrong core_node (the instance_id is the real one)
            ServiceTarget::Producer(&ProducerRef::new("wrong_core_node", listener_instance_id)),
            request_payload.clone(),
            Duration::from_millis(200),
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when targeting the wrong core node");
        };

        err
    };

    let Error::ServiceUnreachable {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = &err
    else {
        panic!(
            "expected ServiceUnreachable error, received unexpected error: {:?}",
            err
        );
    };

    // The pinned target's instance_id travels in the error
    assert_eq!(err_instance_id.as_deref(), Some(listener_instance_id));
    assert_eq!(err_service_name.as_str(), listener_service_name);

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    // Authoritative check that the user handler never ran for the wrong core
    // node — independent of whether the listener timed out or its stream
    // closed first.
    assert_eq!(
        call_count.load(Ordering::SeqCst),
        0,
        "service callback should not have been called"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

/// `ServiceTarget::CoreNode` scopes discovery to one core node: with two
/// listeners exposing the SAME service shape (same node name + tag + service)
/// on DIFFERENT core nodes, a core-node-scoped poll must always be answered
/// by the listener on that core node — the foreign one can never win the
/// discovery probe because the scoped selector never matches its queryable.
/// With `ServiceTarget::Any` this would be a wire-level race either listener
/// could win (see `service_communication_poll_no_instance_id_target`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_communication_poll_core_node_scoped() {
    let router = TestRouterContext::start().await;

    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let foreign_call_count = Arc::new(AtomicUsize::new(0));

    // The foreign listener is registered FIRST so that, without scoping, it
    // would be at least as likely to win the discovery race as the scoped one.
    let foreign_core_node = "foreign_core_node";
    let foreign_instance_id = "foreign_instance";
    let foreign_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            foreign_core_node,
            foreign_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("foreign service should start");

        let foreign_call_count = Arc::clone(&foreign_call_count);
        tokio::spawn(async move {
            service
                .handle_requests(|_request| {
                    foreign_call_count.fetch_add(1, Ordering::SeqCst);
                    async move { Ok(Payload::from_static(b"foreign")) }
                })
                .await
        })
    };

    let scoped_core_node = "scoped_core_node";
    let scoped_instance_id = "scoped_instance";
    let scoped_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            scoped_core_node,
            scoped_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("scoped service should start");

        tokio::spawn(async move {
            service
                .handle_requests(|_request| async move { Ok(Payload::from_static(b"scoped")) })
                .await
        })
    };

    // Each iteration runs a fresh discover-then-pin sequence; without the
    // core-node scope the foreign listener would win some of these races.
    let caller_handle = router.messenger().await;
    for i in 0..10 {
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::CoreNode(scoped_core_node),
            request_payload.clone(),
            Duration::from_secs(2),
        )
        .await
        .expect("scoped poll should receive a response");

        assert_eq!(
            response.core_node(),
            scoped_core_node,
            "scoped poll #{i} was answered by a foreign core node"
        );
        assert_eq!(response.instance_id(), scoped_instance_id);
        assert_eq!(response.payload(), &Payload::from_static(b"scoped"));
    }

    assert_eq!(
        foreign_call_count.load(Ordering::SeqCst),
        0,
        "the foreign core node's handler must never see a scoped request"
    );

    foreign_task.abort();
    scoped_task.abort();
    tokio::time::timeout(Duration::from_secs(2), router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_service_not_started() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let caller_handle = router.messenger().await;

        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any,
            Payload::from_static(b"enable=true"),
            Duration::from_secs(1),
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when service is not started");
        };

        err
    };

    let Error::ServiceUnreachable {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = err
    else {
        panic!(
            "expected ServiceUnreachable error, received unexpected error: {:?}",
            err
        );
    };

    assert_eq!(err_instance_id, None);
    assert_eq!(err_service_name, listener_service_name);

    router.shutdown().await;
}

/// A benchmark "sized probe" must round-trip a real-sized response (the producer
/// honors the requested size) while still NOT invoking the user handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn sized_probe_gets_sized_reply_without_running_the_handler() {
    let router = TestRouterContext::start().await;

    let listener_node_name = "camera";
    let listener_service_name = "video_stream_info";
    let listener_core_node = "listener_core_node";
    let listener_instance_id = "listener_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let call_count = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = oneshot::channel();
    let wait = Duration::from_millis(1000);

    let service_task = {
        let handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);
        tokio::spawn(async move {
            let handler = service.handle_next_request(|_request| {
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from_static(b"real-response"))
                }
            });
            ready_tx.send(()).unwrap();
            // A probe is auto-answered inside the request loop, so the handler
            // never fires — it parks until the timeout.
            let _ = tokio::time::timeout(wait, handler).await;
            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(Duration::from_secs(1), ready_rx)
        .await
        .expect("service should signal readiness")
        .expect("service should signal readiness");

    {
        let caller = router.messenger().await;
        let (_elapsed, response_bytes) = ServiceMessenger::probe_latency(
            &caller,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any,
            Duration::from_secs(5),
            128, // request_size
            256, // response_size
        )
        .await
        .expect("sized probe should round-trip");

        // The producer auto-answered with exactly the requested response size...
        assert_eq!(
            response_bytes, 256,
            "producer should honor the requested response size"
        );
        // ...and the user handler never ran (it was a probe, not a request).
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            0,
            "sized probe must not invoke the user handler"
        );
    }

    let _ = tokio::time::timeout(wait + Duration::from_millis(500), service_task).await;
    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_fails_service_timeouts() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    // Gate that holds back the *second* request's response. The listener ACKs
    // that request (which tells the caller it is reachable) and then parks on
    // this gate, emitting no response until the main task fires it — which it
    // does only after observing the `ServiceTimeout`. The response is absent
    // for the entire failure budget, so the caller deterministically times out
    // waiting for it; the budget only needs to outlast a single ACK round-trip.
    let (release_response_tx, release_response_rx) = oneshot::channel::<()>();
    let service_ready_timeout = Duration::from_secs(1);
    // Safety nets only: they bound how long the listener task may run if the
    // test itself wedges (a request never arrives, or the release gate is never
    // fired). Sized well above any real round-trip; correctness does not depend
    // on their exact value.
    let service_op_timeout = Duration::from_secs(10);
    let service_task_timeout = Duration::from_secs(15);

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task = {
        let service_expose_handle = router.messenger().await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let response_payload = response_payload.clone();
        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            service_ready_tx.send(()).unwrap();

            // First request: reply immediately so the success poll completes in
            // a single round-trip.
            {
                let response_payload = response_payload.clone();
                let call_count = Arc::clone(&call_count);
                let handled = tokio::time::timeout(
                    service_op_timeout,
                    service.handle_next_request(move |request| async move {
                        assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                        assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(response_payload)
                    }),
                )
                .await
                .expect("first service handler hung")
                .expect("first service request errored");
                assert!(handled, "service subscription closed before first request");
            }

            // Second request: the framework auto-ACKs the moment the request
            // arrives — that ACK is what makes the caller classify the outcome
            // as `ServiceTimeout` rather than `ServiceUnreachable`. The handler
            // then parks on the release gate and emits no response until the
            // main task fires it, after the timeout has been observed.
            {
                let response_payload = response_payload.clone();
                let call_count = Arc::clone(&call_count);
                let handled = tokio::time::timeout(
                    service_op_timeout,
                    service.handle_next_request(move |request| async move {
                        assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                        assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        // Park until the main task confirms it saw the timeout.
                        let _ = release_response_rx.await;
                        Ok(response_payload)
                    }),
                )
                .await
                .expect("second service handler hung")
                .expect("second service request errored");
                assert!(handled, "service subscription closed before second request");
            }

            Ok::<(), Error>(())
        })
    };

    tokio::time::timeout(service_ready_timeout, service_ready_rx)
        .await
        .expect("service should signal readiness before timeout")
        .expect("service should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    let err = {
        let request_payload = Payload::from_static(b"enable=true");
        // Both polls run wildcard discover-then-pin, so each budget covers a
        // probe round-trip plus the real request. The success handler replies
        // immediately, so the success poll completes well inside its budget;
        // the failure handler's response is gated off, so the failure poll runs
        // to its deadline and reports `ServiceTimeout`. The failure budget is
        // the test's wall-clock cost for the timeout case, kept modest while
        // still well above a single ACK round-trip.
        let caller_success_timeout = Duration::from_secs(5);
        let caller_failure_timeout = Duration::from_millis(1000);

        let caller_handle = router.messenger().await;

        let success_response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any,
            request_payload.clone(),
            caller_success_timeout,
        )
        .await
        .expect("caller should receive response before timeout");
        assert_eq!(success_response.payload(), response_payload);
        assert_eq!(
            call_count.load(Ordering::SeqCst),
            1,
            "service should have processed the successful request exactly once"
        );

        let result = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any,
            request_payload,
            caller_failure_timeout,
        )
        .await;

        let Err(err) = result else {
            panic!("service call should fail when response exceeds timeout");
        };

        err
    };

    let Error::ServiceTimeout {
        instance_id: err_instance_id,
        service_name: err_service_name,
    } = &err
    else {
        panic!(
            "expected ServiceTimeout error for timeout, received: {:?}",
            err
        );
    };

    assert_eq!(
        err_instance_id.as_deref(),
        Some(listener_instance_id),
        "discover-then-pin resolves the wildcard target before the real poll, \
         so the timeout error carries the discovered instance_id",
    );
    assert_eq!(err_service_name.as_str(), listener_service_name);

    // The timeout has been observed; release the parked handler so the listener
    // task can finish and we can confirm both requests were actually processed.
    let _ = release_response_tx.send(());

    tokio::time::timeout(service_task_timeout, service_task)
        .await
        .expect("service task should finish within timeout")
        .expect("service task panicked")
        .expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        2,
        "service should have processed both requests"
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn service_handle_request_processes_multiple_messages() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller instance
    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let expected_requests = 500;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (shutdown_tx, shutdown_rx) = oneshot::channel();
    let (service_ready_tx, service_ready_rx) = oneshot::channel();
    let host = host.clone();

    let service_task = {
        let service_expose_handle = connect_messenger(&host, port).await;
        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            service_ready_tx.send(()).unwrap();

            tokio::select! {
                result = service.handle_requests(|request| {
                    let call_count = Arc::clone(&call_count);
                    async move {
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message().payload())
                    }
                }) => result,
                _ = shutdown_rx => Ok(()),
            }
        })
    };

    service_ready_rx
        .await
        .expect("service should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        for i in 0..expected_requests {
            let request_payload = Payload::from(format!("enable=true;request={i}").into_bytes());
            let response = ServiceMessenger::poll(
                &caller_handle,
                CALLER_CORE_NODE,
                CALLER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_service_name,
                ServiceTarget::Producer(&ProducerRef::new(
                    listener_core_node,
                    listener_instance_id,
                )),
                request_payload.clone(),
                Duration::from_secs(5),
            )
            .await
            .expect("caller should receive response");
            assert_eq!(
                response.payload(),
                request_payload,
                "response should match the originating request payload"
            );
        }
    }

    shutdown_tx.send(()).unwrap();

    let service_result = service_task.await.expect("service task panicked");
    service_result.expect("service task returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        expected_requests,
        "service should process all requests"
    );

    router.shutdown().await;
}

/// Ensures a unique request returns its unique response
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn single_service_communication_multiple_polls_and_callers() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";
    let listener_instance_id = "listener_instance";
    let listener_core_node = "listener_core_node";

    // Caller core node (shared by all callers)
    const CALLER_CORE_NODE: &str = "caller_core_node";

    // Peer-mode sessions are heavier than the old client sessions: each caller
    // opens its own peer that forms direct links and discovers via gossip, so
    // many fresh peers connecting at once is far more load than the client/router
    // star. Keep the concurrency modest; this still exercises many unique
    // concurrent request/response pairs across independent caller sessions.
    let caller_count = 20;
    let requests_per_caller = 5;
    let total_requests = caller_count * requests_per_caller;
    let call_count = Arc::new(AtomicUsize::new(0));

    let (service_ready_tx, service_ready_rx) = oneshot::channel();

    // The exposed service has its own dedicated scope (emulates running on its own instance)
    let service_task: tokio::task::JoinHandle<Result<(), Error>> = {
        let service_expose_handle = router.messenger().await;

        let mut service = ServiceMessenger::listen(
            &service_expose_handle,
            listener_core_node,
            listener_instance_id,
            test_node_target(listener_node_name),
            listener_service_name,
        )
        .await
        .expect("service should start");

        let call_count = Arc::clone(&call_count);

        tokio::spawn(async move {
            let mut in_flight = Vec::with_capacity(total_requests);
            service_ready_tx.send(()).unwrap();

            for _ in 0..total_requests {
                let call_count = Arc::clone(&call_count);

                let handle = service
                    .spawn_next_request_handler(move |request| async move {
                        assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                        call_count.fetch_add(1, Ordering::SeqCst);
                        Ok(request.message().payload())
                    })
                    .await
                    .expect("service should receive expected number of requests")
                    .expect("service subscription closed before handling request");

                in_flight.push(handle);
            }

            for handle in in_flight {
                handle
                    .await
                    .expect("service handler task panicked")
                    .expect("service handler task returned error");
            }

            Ok(())
        })
    };

    service_ready_rx
        .await
        .expect("service should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let mut expected_payloads = HashMap::with_capacity(total_requests);
        let mut caller_requests = Vec::with_capacity(caller_count);

        for caller_idx in 0..caller_count {
            let caller_name = format!("vision_pipeline_{caller_idx}");
            let mut requests = Vec::with_capacity(requests_per_caller);
            for request_idx in 0..requests_per_caller {
                let payload = Payload::from(
                    format!("caller={caller_name};request={request_idx}").into_bytes(),
                );
                expected_payloads.insert((caller_name.clone(), request_idx), payload.clone());
                requests.push((request_idx, payload));
            }
            caller_requests.push((caller_name, requests));
        }

        let mut rng = rand::rng();
        caller_requests.shuffle(&mut rng);

        let mut handles = Vec::with_capacity(caller_count);
        for (caller_id, mut requests) in caller_requests {
            requests.shuffle(&mut rng);
            let host = host.clone();
            let poll_service = tokio::spawn(async move {
                let caller_handle = connect_messenger(&host, port).await;

                let mut caller_results = Vec::with_capacity(requests.len());
                for (request_idx, request_payload) in requests {
                    let response = ServiceMessenger::poll(
                        &caller_handle,
                        CALLER_CORE_NODE,
                        &caller_id,
                        test_node_target(listener_node_name),
                        listener_service_name,
                        ServiceTarget::Producer(&ProducerRef::new(
                            listener_core_node,
                            listener_instance_id,
                        )),
                        request_payload.clone(),
                        Duration::from_secs(5),
                    )
                    .await
                    .expect("caller should receive response");

                    caller_results.push((
                        caller_id.clone(),
                        request_idx,
                        request_payload.clone(),
                        response,
                    ));
                }

                caller_results
            });
            handles.push(poll_service);
        }

        let mut results = Vec::with_capacity(total_requests);
        for handle in handles {
            let mut caller_results = handle.await.expect("poll_service task should not panic");
            results.append(&mut caller_results);
        }

        for (caller_id, request_idx, request_payload, response) in &results {
            let expected_payload = expected_payloads
                .remove(&(caller_id.clone(), *request_idx))
                .expect("expected payload should exist for caller/request pair");

            assert_eq!(
                request_payload, &expected_payload,
                "stored request payload should match expected value for `{caller_id}` request {request_idx}"
            );
            assert_eq!(
                response.payload(),
                expected_payload,
                "response for `{caller_id}` request {request_idx} should match the originating request payload"
            );
        }

        assert!(
            expected_payloads.is_empty(),
            "all expected caller/request pairs should have been validated"
        );
    };

    service_task
        .await
        .expect("service task panicked")
        .expect("service task returned error");

    let actual_count = call_count.load(Ordering::SeqCst);
    assert_eq!(
        actual_count, total_requests,
        "service should have been called {total_requests} times"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_no_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let result_payload = Payload::from_static(b"done");

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // The factory unwraps the envelope and declares a per-goal
            // publisher in one async call via declare_from_wire.
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(expected_goal_response_payload)
                }
            });

            // Create the result handler future
            let result_handler = action.result_service.handle_next_request(move |request| {
                let response_payload = result_payload_server.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    // Result requests now carry the goal_id envelope (empty body).
                    let request_payload = request.message().payload();
                    let (goal_id, body) = super::unwrap_goal_payload(request_payload.as_ref())
                        .expect("result request must carry a goal_id envelope");
                    assert!(!goal_id.is_empty(), "result request must carry a goal_id");
                    assert!(body.is_empty(), "result request body must be empty");

                    // This test drives the result service directly, so it frames
                    // the reply with the engine's result-outcome envelope itself.
                    Ok(super::actions::wrap_result_outcome(
                        ResultStatus::Completed,
                        response_payload.as_ref(),
                    ))
                }
            });

            // Signal ready after handler is set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            feedback_publisher
                .publish(
                    crate::messaging::NonEmptyPayload::try_new(feedback_payload_server.clone())
                        .expect("test feedback payload is non-empty"),
                )
                .await
                .expect("action should publish feedback");

            let handled_result = tokio::time::timeout(Duration::from_secs(5), result_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_result,
                "result subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_action_name,
            None, // No target producer
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE);
        assert_eq!(
            goal_handle.goal_response().instance_id(),
            LISTENER_INSTANCE_ID
        );
        assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .on_next_feedback()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.payload(), &feedback_payload);
        assert_eq!(feedback_message.core_node(), LISTENER_CORE_NODE);
        assert_eq!(feedback_message.instance_id(), LISTENER_INSTANCE_ID);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::request_result(
            &caller_handle,
            &goal_handle,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.status, ResultStatus::Completed);
        assert_eq!(result_response.body, result_payload);
        assert_eq!(result_response.core_node, LISTENER_CORE_NODE);
        assert_eq!(result_response.instance_id, LISTENER_INSTANCE_ID);
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_with_instance_id_target() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "controller";
    let listener_action_name = "move_right_arm";

    const LISTENER_CORE_NODE1: &str = "listener_core_node1";
    const LISTENER_INSTANCE_ID1: &str = "listener_instance1";

    const LISTENER_CORE_NODE2: &str = "listener_core_node2";
    const LISTENER_INSTANCE_ID2: &str = "listener_instance2";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let result_payload = Payload::from_static(b"done");

    let call_count = Arc::new(AtomicUsize::new(0));

    // Launch a background task that plays the role of the action server.
    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    // This listener should not receive any message
    let server_task1 = {
        let expected_goal_response_payload = goal_response_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE1,
                LISTENER_INSTANCE_ID1,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let call_count = Arc::clone(&call_count);
            let call_count_for_closure = Arc::clone(&call_count);

            // Create the goal handler future first (this sets up the subscription)
            let goal_handler =
                action
                    .goal_service
                    .handle_next_request(move |_request| async move {
                        // This should never be reached
                        call_count_for_closure.fetch_add(1, Ordering::SeqCst);
                        Ok(expected_goal_response_payload)
                    });

            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler).await;

            assert!(
                handled_goal.is_err(),
                "server_task1 should not receive a goal request - timeout expected"
            );
            assert_eq!(
                call_count.load(Ordering::SeqCst),
                0,
                "goal handler should not have been called"
            );
            Ok::<(), Error>(())
        })
    };

    let server_task2 = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let result_payload_server = result_payload.clone();

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE2,
                LISTENER_INSTANCE_ID2,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // The factory unwraps the envelope and declares a per-goal
            // publisher in one async call via declare_from_wire.
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    Ok(expected_goal_response_payload)
                }
            });

            // Create the result handler future
            let result_handler = action.result_service.handle_next_request(move |request| {
                let response_payload = result_payload_server.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    // Result requests now carry the goal_id envelope (empty body).
                    let request_payload = request.message().payload();
                    let (goal_id, body) = super::unwrap_goal_payload(request_payload.as_ref())
                        .expect("result request must carry a goal_id envelope");
                    assert!(!goal_id.is_empty(), "result request must carry a goal_id");
                    assert!(body.is_empty(), "result request body must be empty");

                    // This test drives the result service directly, so it frames
                    // the reply with the engine's result-outcome envelope itself.
                    Ok(super::actions::wrap_result_outcome(
                        ResultStatus::Completed,
                        response_payload.as_ref(),
                    ))
                }
            });

            // Signal ready after handler is set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            feedback_publisher
                .publish(
                    crate::messaging::NonEmptyPayload::try_new(feedback_payload_server.clone())
                        .expect("test feedback payload is non-empty"),
                )
                .await
                .expect("action should publish feedback");

            let handled_result = tokio::time::timeout(Duration::from_secs(5), result_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_result,
                "result subscription closed before handling request"
            );

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    // The caller node has its own scope (emulates a separate node running on a different instance)
    {
        let caller_handle = router.messenger().await;

        let mut goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_action_name,
            Some(&ProducerRef::new(
                LISTENER_CORE_NODE2,
                LISTENER_INSTANCE_ID2,
            )),
            goal_payload,
            QoSProfile::Reliable,
            Duration::from_millis(1000),
        )
        .await
        .expect("caller should send goal");

        assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE2);
        assert_eq!(
            goal_handle.goal_response().instance_id(),
            LISTENER_INSTANCE_ID2
        );
        assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

        // Consume one feedback update from the action server.
        let feedback_message = goal_handle
            .on_next_feedback()
            .await
            .expect("caller should receive feedback");

        assert_eq!(feedback_message.payload(), &feedback_payload);
        assert_eq!(feedback_message.core_node(), LISTENER_CORE_NODE2);
        assert_eq!(feedback_message.instance_id(), LISTENER_INSTANCE_ID2);

        // Finally, request the result using the same handle and ensure the server replies.
        let result_response = ActionMessenger::request_result(
            &caller_handle,
            &goal_handle,
            Duration::from_millis(500),
        )
        .await
        .expect("caller should receive result");

        assert_eq!(result_response.status, ResultStatus::Completed);
        assert_eq!(result_response.body, result_payload);
        assert_eq!(result_response.core_node, LISTENER_CORE_NODE2);
        assert_eq!(result_response.instance_id, LISTENER_INSTANCE_ID2);
    }

    server_task1
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    server_task2
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_communication_goal_cancelled() {
    let router = TestRouterContext::start().await;

    // Listener instance
    let listener_node_name = "camera";
    let listener_action_name = "enable_camera";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let goal_payload = Payload::from_static(b"arm=right;pos=1,2,3");
    let goal_response_payload = Payload::from_static(b"accepted");
    let feedback_payload = Payload::from_static(b"progress=50");
    let cancel_response_payload = Payload::from_static(b"cancelled");

    let goal_call_count = Arc::new(AtomicUsize::new(0));
    let cancel_call_count = Arc::new(AtomicUsize::new(0));

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    let server_task = {
        let expected_goal_payload = goal_payload.clone();
        let expected_goal_response_payload = goal_response_payload.clone();
        let feedback_payload_server = feedback_payload.clone();
        let cancel_response_payload_server = cancel_response_payload.clone();
        let goal_call_count = Arc::clone(&goal_call_count);
        let cancel_call_count = Arc::clone(&cancel_call_count);

        let action_handle = router.messenger().await;

        tokio::spawn(async move {
            let mut action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let (publisher_tx, publisher_rx) =
                tokio::sync::oneshot::channel::<crate::messaging::ActionFeedbackPublisher>();
            let publisher_tx = std::sync::Mutex::new(Some(publisher_tx));
            let factory_for_handler = action.feedback_publisher_factory.clone();

            // Create the goal handler future first (this sets up the subscription)
            let goal_handler = action.goal_service.handle_next_request(move |request| {
                let goal_call_count = Arc::clone(&goal_call_count);
                let factory = factory_for_handler.clone();
                let publisher_tx = std::sync::Mutex::new(publisher_tx.lock().unwrap().take());
                async move {
                    let declared = factory
                        .declare_from_wire("_", request.message().payload().into_inner())
                        .await
                        .expect("declare from wire");
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(declared.user_payload, expected_goal_payload.as_ref());
                    if let Some(tx) = publisher_tx.lock().unwrap().take() {
                        let _ = tx.send(declared.publisher);
                    }
                    goal_call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(expected_goal_response_payload)
                }
            });

            // Signal ready after handlers are set up
            action_ready_tx.send(()).unwrap();

            // From this point on, wait for the client to send a goal request
            let handled_goal = tokio::time::timeout(Duration::from_secs(5), goal_handler)
                .await
                .expect("timed out waiting for goal request")
                .expect("action should receive goal request");

            assert!(
                handled_goal,
                "goal subscription closed before handling request"
            );

            let stop_feedback = Arc::new(tokio::sync::Notify::new());
            let feedback_publisher = publisher_rx
                .await
                .expect("server should have captured publisher");
            let feedback_task = {
                let stop_feedback = Arc::clone(&stop_feedback);
                let feedback_publisher = feedback_publisher.clone();
                let feedback_payload = feedback_payload_server.clone();
                tokio::spawn(async move {
                    let mut ticker = tokio::time::interval(Duration::from_millis(50));
                    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    let stop_notified = stop_feedback.notified();
                    tokio::pin!(stop_notified);
                    loop {
                        tokio::select! {
                            biased;
                            _ = stop_notified.as_mut() => break,
                            _ = ticker.tick() => {
                                feedback_publisher
                                    .publish(
                                        crate::messaging::NonEmptyPayload::try_new(
                                            feedback_payload.clone(),
                                        )
                                        .expect("test feedback payload is non-empty"),
                                    )
                                    .await?;
                            }
                        }
                    }
                    Ok::<(), Error>(())
                })
            };

            let (cancel_context, cancel_responder) = tokio::time::timeout(
                Duration::from_secs(5),
                action.cancel_service.recv_next_request(),
            )
            .await
            .expect("timed out waiting for cancel request")
            .expect("action should receive cancel request")
            .expect("cancel subscription should not be closed");

            assert_eq!(cancel_context.message().core_node(), CALLER_CORE_NODE);
            assert_eq!(cancel_context.message().instance_id(), CALLER_INSTANCE_ID);
            // Cancel requests now carry the goal_id envelope (empty body).
            let cancel_payload = cancel_context.message().payload();
            let (goal_id, body) = super::unwrap_goal_payload(cancel_payload.as_ref())
                .expect("cancel request must carry a goal_id envelope");
            assert!(!goal_id.is_empty(), "cancel request must carry a goal_id");
            assert!(body.is_empty(), "cancel request body must be empty");

            cancel_call_count.fetch_add(1, Ordering::SeqCst);

            // Stop feedback publication before acknowledging cancellation to reduce
            // flakiness caused by in-flight feedback after cancellation.
            stop_feedback.notify_waiters();
            feedback_task.await.expect("feedback loop task panicked")?;

            cancel_responder
                .respond(cancel_response_payload_server)
                .await?;

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    let caller_handle = router.messenger().await;

    let mut goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        CALLER_CORE_NODE,
        CALLER_INSTANCE_ID,
        test_node_target(listener_node_name),
        listener_action_name,
        Some(&ProducerRef::new(LISTENER_CORE_NODE, LISTENER_INSTANCE_ID)),
        goal_payload,
        QoSProfile::Reliable,
        Duration::from_millis(1000),
    )
    .await
    .expect("caller should send goal");

    assert_eq!(goal_handle.goal_response().core_node(), LISTENER_CORE_NODE);
    assert_eq!(
        goal_handle.goal_response().instance_id(),
        LISTENER_INSTANCE_ID
    );
    assert_eq!(goal_handle.goal_response().payload(), goal_response_payload);

    let first_feedback = goal_handle
        .on_next_feedback()
        .await
        .expect("caller should receive initial feedback");

    assert_eq!(first_feedback.payload(), &feedback_payload);
    assert_eq!(first_feedback.core_node(), LISTENER_CORE_NODE);
    assert_eq!(first_feedback.instance_id(), LISTENER_INSTANCE_ID);

    let second_feedback =
        tokio::time::timeout(Duration::from_secs(1), goal_handle.on_next_feedback())
            .await
            .expect("feedback stream should continue delivering updates before cancellation")
            .expect("feedback stream closed unexpectedly before cancellation");

    assert_eq!(second_feedback.payload(), &feedback_payload);
    assert_eq!(second_feedback.core_node(), LISTENER_CORE_NODE);
    assert_eq!(second_feedback.instance_id(), LISTENER_INSTANCE_ID);

    let cancel_response =
        ActionMessenger::cancel_goal(&caller_handle, &goal_handle, Duration::from_millis(500))
            .await
            .expect("caller should receive cancel acknowledgement");

    assert_eq!(cancel_response.payload(), cancel_response_payload);
    assert_eq!(cancel_response.core_node(), LISTENER_CORE_NODE);
    assert_eq!(cancel_response.instance_id(), LISTENER_INSTANCE_ID);

    // Check that feedback eventually goes quiet after cancellation; allow a short window for
    // buffered/in-flight feedback messages to be drained.
    let quiet_for = Duration::from_millis(200);
    let overall_timeout = Duration::from_secs(2);
    let start = tokio::time::Instant::now();
    let mut quiet_deadline = start + quiet_for;

    loop {
        let now = tokio::time::Instant::now();
        if now >= quiet_deadline {
            break;
        }
        if now.duration_since(start) >= overall_timeout {
            panic!(
                "feedback did not stop within {:?} after cancellation",
                overall_timeout
            );
        }

        let remaining = quiet_deadline
            .checked_duration_since(now)
            .unwrap_or_default();

        match tokio::time::timeout(remaining, goal_handle.on_next_feedback()).await {
            Ok(Ok(_)) => {
                quiet_deadline = tokio::time::Instant::now() + quiet_for;
            }
            Ok(Err(Error::ActionFeedbackChannelClosed)) => break,
            Ok(Err(err)) => panic!("unexpected feedback error after cancellation: {err:?}"),
            Err(_) => break,
        }
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    assert_eq!(
        goal_call_count.load(Ordering::SeqCst),
        1,
        "goal handler should have been called exactly once"
    );
    assert_eq!(
        cancel_call_count.load(Ordering::SeqCst),
        1,
        "cancel handler should have been called exactly once"
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn single_action_communication_multiple_polls() {
    let router = TestRouterContext::start().await;
    let (host, port) = router.connection_target();

    // Listener instance
    let listener_node_name = "camera";
    let listener_action_name = "enable_camera";
    const LISTENER_CORE_NODE: &str = "listener_core_node";
    const LISTENER_INSTANCE_ID: &str = "listener_instance";

    // Caller instance
    const CALLER_CORE_NODE: &str = "caller_core_node";
    let caller_prefix = "the_brain";

    const CLIENT_COUNT: usize = 8;
    let cases: Vec<_> = (0..CLIENT_COUNT)
        .map(|idx| ActionClientCase::new(caller_prefix, idx))
        .collect();
    let cases = Arc::new(cases);

    let (action_ready_tx, action_ready_rx) = oneshot::channel();

    // Launch a background task that plays the role of the action server.
    let server_task = {
        let action_handle = router.messenger().await;
        let action_ready_tx = Some(action_ready_tx);
        let cases = Arc::clone(&cases);

        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &action_handle,
                LISTENER_CORE_NODE,
                LISTENER_INSTANCE_ID,
                test_node_target(listener_node_name),
                listener_action_name,
            )
            .await
            .expect("action should start");

            let crate::messaging::ActionCreation {
                mut goal_service,
                cancel_service: _,
                feedback_publisher_factory,
                mut result_service,
                liveliness_token: _liveliness_token,
            } = action;
            let feedback_publisher_factory = Arc::new(feedback_publisher_factory);

            if let Some(tx) = action_ready_tx {
                let _ = tx.send(());
            }

            let client_total = cases.len();

            let mut goal_handlers = Vec::with_capacity(client_total);
            for _ in 0..client_total {
                let cases = Arc::clone(&cases);
                let factory = Arc::clone(&feedback_publisher_factory);

                let handler = goal_service
                    .spawn_next_request_handler(move |request| {
                        let cases = Arc::clone(&cases);
                        let factory = Arc::clone(&factory);

                        async move {
                            let declared = factory
                                .declare_from_wire("_", request.message().payload().into_inner())
                                .await
                                .expect("declare from wire");
                            let payload_str = std::str::from_utf8(&declared.user_payload)
                                .expect("goal payload should be valid UTF-8");

                            let client_id = payload_str
                                .split(';')
                                .find_map(|part| part.strip_prefix("client="))
                                .expect("goal payload should contain client identifier")
                                .to_string();

                            let case = cases
                                .iter()
                                .find(|case| case.client_id == client_id)
                                .unwrap_or_else(|| {
                                    panic!(
                                        "goal handler received unexpected client id `{client_id}`"
                                    )
                                });

                            assert_eq!(
                                declared.user_payload,
                                case.goal.as_ref(),
                                "goal payload for `{client_id}` should match expected value"
                            );

                            declared
                                .publisher
                                .publish(
                                    crate::messaging::NonEmptyPayload::try_new(
                                        case.feedback.clone(),
                                    )
                                    .expect("test case feedback payload is non-empty"),
                                )
                                .await?;

                            Ok(case.goal_response.clone())
                        }
                    })
                    .await
                    .expect("action should spawn goal handler")
                    .expect("goal subscription closed before handling request");

                goal_handlers.push(handler);
            }

            for handler in goal_handlers {
                handler
                    .await
                    .expect("goal handler task panicked")
                    .expect("goal handler returned error");
            }

            let mut result_handlers = Vec::with_capacity(client_total);
            for _ in 0..client_total {
                let handler = result_service
                    .spawn_next_request_handler(move |request| async move {
                        // Result requests now carry the goal_id envelope (empty body).
                        let request_payload = request.message().payload();
                        let (goal_id, body) = super::unwrap_goal_payload(request_payload.as_ref())
                            .expect("result request must carry a goal_id envelope");
                        assert!(!goal_id.is_empty(), "result request must carry a goal_id");
                        assert!(body.is_empty(), "result request body must be empty");

                        // Driven directly: frame the reply like the engine does.
                        Ok(super::actions::wrap_result_outcome(
                            ResultStatus::Completed,
                            b"result=done",
                        ))
                    })
                    .await
                    .expect("action should spawn result handler")
                    .expect("result subscription closed before handling request");

                result_handlers.push(handler);
            }

            for handler in result_handlers {
                handler
                    .await
                    .expect("result handler task panicked")
                    .expect("result handler returned error");
            }

            Ok::<(), Error>(())
        })
    };

    action_ready_rx
        .await
        .expect("action server should signal readiness");

    let total_clients = cases.len();
    let mut shuffled_cases = cases.as_ref().clone();
    let mut rng = rand::rng();
    shuffled_cases.shuffle(&mut rng);

    let mut client_handles = Vec::with_capacity(total_clients);
    for case in shuffled_cases {
        let host = host.clone();
        let feedback_search_limit = total_clients;

        let handle = tokio::spawn(async move {
            let caller_handle = connect_messenger(&host, port).await;

            let mut goal_handle = ActionMessenger::send_goal(
                &caller_handle,
                CALLER_CORE_NODE,
                &case.client_id,
                test_node_target(listener_node_name),
                listener_action_name,
                None,
                case.goal.clone(),
                QoSProfile::Reliable,
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should send goal");

            assert_eq!(
                goal_handle.goal_response().payload(),
                case.goal_response.clone(),
                "goal response should match expected payload for `{}`",
                case.client_id
            );

            let mut feedback_matched = false;
            for _ in 0..feedback_search_limit {
                let feedback_message = goal_handle
                    .on_next_feedback()
                    .await
                    .expect("caller should receive feedback message");

                if feedback_message.payload() == case.feedback {
                    feedback_matched = true;
                    break;
                }
            }

            assert!(
                feedback_matched,
                "caller `{}` should observe its corresponding feedback payload",
                case.client_id
            );

            let result_response = ActionMessenger::request_result(
                &caller_handle,
                &goal_handle,
                Duration::from_millis(1000),
            )
            .await
            .expect("caller should receive result response");

            assert_eq!(result_response.status, ResultStatus::Completed);
            assert_eq!(
                result_response.body,
                Payload::from_static(b"result=done"),
                "result response should match expected payload for `{}`",
                case.client_id
            );
        });

        client_handles.push(handle);
    }

    for handle in client_handles {
        handle.await.expect("caller task should not panic");
    }

    server_task
        .await
        .expect("action handler task panicked")
        .expect("action handler returned error");

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn topic_duplicate_from_any_subscription_is_rejected() {
    // The wire-level dedupe for wildcard topic subscribers (the
    // primary/secondary attachment plus the sibling-exclusion filter)
    // depends on the manifest validator's "at most one from_any consumer
    // per (name, tag)" invariant. Anything that bypasses the validator
    // (a test, a tooling integration, a future runtime-deps feature) can
    // install state that violates it and corrupt aggregator state with
    // silent duplicate deliveries.
    //
    // This asserts the runtime guard at `MessengerHandle::subscribe`
    // catches the violation: two from_any topic subscriptions on the same
    // `(name, tag)` cannot coexist. After dropping the first the slot is
    // released so a later subscription succeeds, and the surviving
    // from_any subscription still receives exactly one delivery per
    // multi-link emit (proving the existing dedupe still works under the
    // enforced invariant).
    let router = TestRouterContext::start().await;
    let subscriber_handle = router.messenger().await;

    let target = || SenderTarget::interface("depth_camera", "v1").expect("iface target");

    let sub_one = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_one",
        Some(target()),
        true,
        "frames",
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("first from_any subscribe should succeed");

    let second = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_two",
        Some(target()),
        true,
        "frames",
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await;
    match second {
        Err(Error::DuplicateFromAnyConsumer { ref name, ref tag })
            if name == "depth_camera" && tag == "v1" => {}
        Err(other) => panic!("unexpected error rejecting second from_any: {other:?}"),
        Ok(_) => panic!("second from_any subscribe must be rejected, got Ok"),
    }

    // A pinned subscription on the same (name, tag) is unaffected — only
    // from_any subs take the slot.
    let pinned_filter = ConsumerFilter::Pin(ProducerRef::new("pub_core", "left_emitter"));
    let _sub_pinned = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_pinned",
        Some(target()),
        false,
        "frames",
        &pinned_filter,
        QoSProfile::Reliable,
    )
    .await
    .expect("pinned subscribe on same (name, tag) must coexist with from_any");

    // Drop the first from_any sub; its guard releases the slot and the
    // next from_any subscribe should succeed.
    drop(sub_one);

    let mut sub_three = TopicMessenger::subscribe(
        &subscriber_handle,
        "sub_core",
        "sub_inst_three",
        Some(target()),
        true,
        "frames",
        &ConsumerFilter::Any,
        QoSProfile::Reliable,
    )
    .await
    .expect("from_any subscribe should succeed after the first guard dropped");

    let emitter_handle = router.messenger().await;
    // Deterministically wait for the surviving from_any subscriber before the
    // publish so it is not dropped during peer-mode discovery propagation.
    TopicMessenger::wait_for_subscriber(
        &emitter_handle,
        "pub_core",
        "pub_inst",
        target(),
        "frames",
        Duration::from_secs(5),
    )
    .await
    .expect("subscriber should become reachable");
    publish_once(
        &emitter_handle,
        "pub_core",
        "pub_inst",
        target(),
        "frames",
        QoSProfile::Reliable,
        Payload::from_static(b"frame-0"),
    )
    .await
    .expect("emit should succeed");

    // Exactly one delivery on the surviving from_any sub. The degenerate
    // sibling map intentionally fails to claim either bound link_id, so
    // the existing dedupe (primary/secondary attachment) must do the work
    // alone — and the runtime guard ensures it isn't asked to do more.
    let first = tokio::time::timeout(Duration::from_secs(2), sub_three.on_next_message())
        .await
        .expect("from_any subscriber should not time out")
        .expect("from_any subscriber should receive a message");
    assert_eq!(first.payload().as_ref(), b"frame-0");
    let dup = tokio::time::timeout(Duration::from_millis(300), sub_three.on_next_message()).await;
    assert!(
        dup.is_err(),
        "from_any subscriber must not receive a duplicate (got {:?})",
        dup.ok().flatten().map(|m| m.payload().as_ref().to_vec())
    );

    router.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn action_from_any_send_goal_runs_handler_on_winner_only() {
    // Two producer processes expose the same action and a consumer sends
    // a wildcard goal. With discover-then-pin, only ONE producer's goal
    // handler must run — the one that responds first to the discovery
    // probe. The loser sees the probe (filtered internally, no handler
    // invocation) but never sees the real goal. The subsequent
    // `cancel_goal` also targets only the winner because the wire sender
    // was pinned at discovery time.
    let router = TestRouterContext::start().await;

    let server_a_core = "server_a_core";
    let server_a_inst = "server_a_inst";
    let server_b_core = "server_b_core";
    let server_b_inst = "server_b_inst";
    let action_target = SenderTarget::interface("manipulator", "v1").expect("iface target");
    let action_name = "abort_safe";

    struct ProducerSpec {
        core: &'static str,
        inst: &'static str,
        target: SenderTarget,
        action_name: &'static str,
    }

    struct ProducerCounters {
        goal: Arc<AtomicUsize>,
        cancel: Arc<AtomicUsize>,
    }

    async fn spawn_producer(
        router: &TestRouterContext,
        spec: ProducerSpec,
        counters: ProducerCounters,
        ready: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = router.messenger().await;
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                spec.core,
                spec.inst,
                spec.target,
                spec.action_name,
            )
            .await
            .expect("expose should succeed");

            let mut goal_service = action.goal_service;
            let mut cancel_service = action.cancel_service;
            ready.send(()).expect("ready");

            // The loser must time out here; the winner returns immediately.
            match tokio::time::timeout(Duration::from_millis(800), goal_service.recv_next_request())
                .await
            {
                Ok(Ok(Some((_ctx, goal_responder)))) => {
                    counters.goal.fetch_add(1, Ordering::SeqCst);
                    goal_responder
                        .respond(Payload::from(spec.inst.as_bytes().to_vec()))
                        .await
                        .expect("goal respond");
                }
                _ => {
                    // No goal arrived within the budget; producer must be
                    // the loser of the discovery race.
                    return;
                }
            }

            // Only the winner reaches this point. Wait for the cancel
            // that send_goal's pinned sender will direct here.
            if let Ok(Ok(Some((_ctx, responder)))) = tokio::time::timeout(
                Duration::from_millis(800),
                cancel_service.recv_next_request(),
            )
            .await
            {
                counters.cancel.fetch_add(1, Ordering::SeqCst);
                let _ = responder.respond(Payload::from_static(b"cancelled")).await;
            }
        })
    }

    let goal_a = Arc::new(AtomicUsize::new(0));
    let goal_b = Arc::new(AtomicUsize::new(0));
    let cancel_a = Arc::new(AtomicUsize::new(0));
    let cancel_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();

    let task_a = spawn_producer(
        &router,
        ProducerSpec {
            core: server_a_core,
            inst: server_a_inst,
            target: action_target.clone(),
            action_name,
        },
        ProducerCounters {
            goal: Arc::clone(&goal_a),
            cancel: Arc::clone(&cancel_a),
        },
        ready_a_tx,
    )
    .await;
    let task_b = spawn_producer(
        &router,
        ProducerSpec {
            core: server_b_core,
            inst: server_b_inst,
            target: action_target.clone(),
            action_name,
        },
        ProducerCounters {
            goal: Arc::clone(&goal_b),
            cancel: Arc::clone(&cancel_b),
        },
        ready_b_tx,
    )
    .await;

    ready_a_rx.await.expect("server A ready");
    ready_b_rx.await.expect("server B ready");

    let caller_handle = router.messenger().await;
    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        action_target,
        action_name,
        None, // wildcard target producer
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    let winner_inst = goal_handle.goal_response().instance_id().to_string();
    let winner_core = goal_handle.goal_response().core_node().to_string();
    assert!(
        winner_inst == server_a_inst || winner_inst == server_b_inst,
        "goal_response identity must come from one of the producers, got {winner_inst:?}",
    );
    assert!(
        winner_core == server_a_core || winner_core == server_b_core,
        "goal_response core_node must come from one of the producers, got {winner_core:?}",
    );

    let _ = ActionMessenger::cancel_goal(&caller_handle, &goal_handle, Duration::from_secs(1))
        .await
        .expect("cancel_goal should reach the latched producer");

    task_a.await.expect("server A task panicked");
    task_b.await.expect("server B task panicked");

    let (winner_goal, loser_goal, winner_cancel, loser_cancel) = if winner_inst == server_a_inst {
        (
            goal_a.load(Ordering::SeqCst),
            goal_b.load(Ordering::SeqCst),
            cancel_a.load(Ordering::SeqCst),
            cancel_b.load(Ordering::SeqCst),
        )
    } else {
        (
            goal_b.load(Ordering::SeqCst),
            goal_a.load(Ordering::SeqCst),
            cancel_b.load(Ordering::SeqCst),
            cancel_a.load(Ordering::SeqCst),
        )
    };

    assert_eq!(
        winner_goal, 1,
        "winning producer ({winner_inst}) should have run its goal handler exactly once",
    );
    assert_eq!(
        loser_goal, 0,
        "losing producer must NOT run its goal handler — discovery pins to the winner before the real goal is sent",
    );
    assert_eq!(
        winner_cancel, 1,
        "winning producer should have received the cancel",
    );
    assert_eq!(
        loser_cancel, 0,
        "losing producer must NOT receive the cancel — sender was pinned at discovery time",
    );

    router.shutdown().await;
}

/// A FULL-wildcard `poll` (`target: None`) issued against two producers
/// that share an `instance_id` but live on different `core_node`s must
/// run discover-then-pin so exactly ONE handler runs. The pre-`ProducerRef`
/// half-pinned shape (core_node wildcard + instance_id pin) is
/// unrepresentable now that producer identity travels as a full
/// `(core_node, instance_id)` pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_communication_poll_full_wildcard_discovers() {
    let router = TestRouterContext::start().await;

    let listener_node_name = "camera";
    let listener_service_name = "enable_camera";

    // Both listeners share the SAME instance_id but differ on core_node.
    // Without discovery, a full-wildcard query would match both and both
    // handlers would run.
    let shared_instance_id = "shared_inst";
    let listener_core_node1 = "listener_core_node_a";
    let listener_core_node2 = "listener_core_node_b";

    const CALLER_INSTANCE_ID: &str = "caller_instance";
    const CALLER_CORE_NODE: &str = "caller_core_node";

    let request_payload = Payload::from_static(b"enable=true");
    let response_payload = Payload::from_static(b"ack");
    let call_count = Arc::new(AtomicUsize::new(0));

    let (ready_tx1, ready_rx1) = oneshot::channel();
    let (ready_tx2, ready_rx2) = oneshot::channel();
    let service_wait_timeout = Duration::from_millis(1500);
    let service_task_timeout = service_wait_timeout + Duration::from_millis(500);
    let service_ready_timeout = Duration::from_secs(1);

    let spawn_listener = |handle: MessengerHandle,
                          ready_tx: oneshot::Sender<()>,
                          core_node: &'static str,
                          response_payload: Payload,
                          call_count: Arc<AtomicUsize>| {
        let request_payload = request_payload.clone();
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                core_node,
                shared_instance_id,
                test_node_target(listener_node_name),
                listener_service_name,
            )
            .await
            .expect("service should start");

            let handler = service.handle_next_request(|request| {
                let response_payload = response_payload.clone();
                async move {
                    assert_eq!(request.message().core_node(), CALLER_CORE_NODE);
                    assert_eq!(request.message().instance_id(), CALLER_INSTANCE_ID);
                    assert_eq!(request.message().payload(), &request_payload);
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(response_payload)
                }
            });

            ready_tx.send(()).unwrap();
            // Either listener may win the discovery race; whichever
            // loses simply times out without invoking the handler.
            let _ = tokio::time::timeout(service_wait_timeout, handler).await;
            Ok::<(), Error>(())
        })
    };

    let task1 = spawn_listener(
        router.messenger().await,
        ready_tx1,
        listener_core_node1,
        response_payload.clone(),
        Arc::clone(&call_count),
    );
    let task2 = spawn_listener(
        router.messenger().await,
        ready_tx2,
        listener_core_node2,
        response_payload.clone(),
        Arc::clone(&call_count),
    );

    tokio::time::timeout(service_ready_timeout, ready_rx1)
        .await
        .expect("service 1 should signal readiness before timeout")
        .expect("service 1 should signal readiness");
    tokio::time::timeout(service_ready_timeout, ready_rx2)
        .await
        .expect("service 2 should signal readiness before timeout")
        .expect("service 2 should signal readiness");

    {
        let caller_handle = router.messenger().await;
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(listener_node_name),
            listener_service_name,
            ServiceTarget::Any, // full-wildcard target — must trigger discovery
            request_payload.clone(),
            Duration::from_secs(1),
        )
        .await
        .expect("caller should receive response");

        assert_eq!(response.instance_id(), shared_instance_id);
        assert!(
            response.core_node() == listener_core_node1
                || response.core_node() == listener_core_node2,
            "response must come from one of the listeners, got {:?}",
            response.core_node(),
        );
        assert_eq!(response.payload(), &response_payload);
    }

    tokio::time::timeout(service_task_timeout, task1)
        .await
        .expect("service task 1 should finish within timeout")
        .expect("service task 1 panicked")
        .expect("service task 1 returned error");
    tokio::time::timeout(service_task_timeout, task2)
        .await
        .expect("service task 2 should finish within timeout")
        .expect("service task 2 panicked")
        .expect("service task 2 returned error");

    assert_eq!(
        call_count.load(Ordering::SeqCst),
        1,
        "exactly one listener handler must run — discover-then-pin must \
         pin to one producer for a full-wildcard call",
    );

    tokio::time::timeout(service_task_timeout, router.shutdown())
        .await
        .expect("router shutdown timed out");
}

/// Action mirror of [`service_communication_poll_full_wildcard_discovers`]:
/// a FULL-wildcard `send_goal` (`target: None`) against two producers
/// sharing an `instance_id` on different `core_node`s must run
/// discover-then-pin so exactly ONE goal handler runs — both executing
/// would violate the safety contract for actions with side effects. The
/// pre-`ProducerRef` half-pinned shape (core_node wildcard + instance_id
/// pin) is unrepresentable now that producer identity travels as a full
/// `(core_node, instance_id)` pair.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_send_goal_full_wildcard_discovers() {
    let router = TestRouterContext::start().await;

    let action_target = SenderTarget::interface("manipulator", "v1").expect("iface target");
    let action_name = "abort_safe";

    // Same instance_id on both servers; different core_nodes.
    let shared_inst = "shared_inst";
    let server_a_core = "server_a_core";
    let server_b_core = "server_b_core";

    struct ProducerSpec {
        core: &'static str,
        inst: &'static str,
        target: SenderTarget,
        action_name: &'static str,
    }

    async fn spawn_producer(
        router: &TestRouterContext,
        spec: ProducerSpec,
        goal_count: Arc<AtomicUsize>,
        ready: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = router.messenger().await;
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                spec.core,
                spec.inst,
                spec.target,
                spec.action_name,
            )
            .await
            .expect("expose should succeed");

            let mut goal_service = action.goal_service;
            ready.send(()).expect("ready");

            // The loser of the discovery race never sees a real goal and
            // simply times out below.
            if let Ok(Ok(Some((_ctx, goal_responder)))) =
                tokio::time::timeout(Duration::from_millis(800), goal_service.recv_next_request())
                    .await
            {
                goal_count.fetch_add(1, Ordering::SeqCst);
                goal_responder
                    .respond(Payload::from(spec.core.as_bytes().to_vec()))
                    .await
                    .expect("goal respond");
            }
        })
    }

    let goal_a = Arc::new(AtomicUsize::new(0));
    let goal_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();

    let task_a = spawn_producer(
        &router,
        ProducerSpec {
            core: server_a_core,
            inst: shared_inst,
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_a),
        ready_a_tx,
    )
    .await;
    let task_b = spawn_producer(
        &router,
        ProducerSpec {
            core: server_b_core,
            inst: shared_inst,
            target: action_target.clone(),
            action_name,
        },
        Arc::clone(&goal_b),
        ready_b_tx,
    )
    .await;

    ready_a_rx.await.expect("server A ready");
    ready_b_rx.await.expect("server B ready");

    let caller_handle = router.messenger().await;
    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        action_target,
        action_name,
        None, // full-wildcard target — must trigger discovery
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(2),
    )
    .await
    .expect("send_goal should succeed");

    assert_eq!(goal_handle.goal_response().instance_id(), shared_inst);
    let winner_core = goal_handle.goal_response().core_node().to_string();
    assert!(
        winner_core == server_a_core || winner_core == server_b_core,
        "goal_response core_node must come from one of the producers, got {winner_core:?}",
    );

    task_a.await.expect("server A task panicked");
    task_b.await.expect("server B task panicked");

    let total = goal_a.load(Ordering::SeqCst) + goal_b.load(Ordering::SeqCst);
    assert_eq!(
        total,
        1,
        "exactly one producer must run its goal handler — discover-then-pin \
         must pin to one producer for a full-wildcard call \
         (a={}, b={})",
        goal_a.load(Ordering::SeqCst),
        goal_b.load(Ordering::SeqCst),
    );

    router.shutdown().await;
}

/// Matrix corner from the bimanual field failure: same core_node, two
/// instances of one node (same `(name, tag)`), distinct instance_ids,
/// caller pinned to each instance in turn. Both producers idle: each
/// pinned poll must reach exactly the pinned listener and never its
/// sibling.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_poll_same_core_distinct_instances_pinned_routes_to_pinned() {
    let router = TestRouterContext::start().await;

    let node_name = "openarm_mujoco";
    let service_name = "set_arm_mode";
    let shared_core = "core_same";
    let left_inst = "left_arm_inst";
    let right_inst = "right_arm_inst";

    const CALLER_CORE_NODE: &str = "caller_core_node";
    const CALLER_INSTANCE_ID: &str = "caller_instance";

    let spawn_listener = |handle: MessengerHandle,
                          inst: &'static str,
                          ready_tx: oneshot::Sender<()>,
                          call_count: Arc<AtomicUsize>| {
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                shared_core,
                inst,
                test_node_target(node_name),
                service_name,
            )
            .await
            .expect("service should start");

            let handler = service.handle_next_request(|_request| {
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from(inst.as_bytes().to_vec()))
                }
            });

            ready_tx.send(()).unwrap();
            // The sibling of the pinned listener must never see a request
            // and simply times out here without running its handler.
            let _ = tokio::time::timeout(Duration::from_secs(5), handler).await;
            Ok::<(), Error>(())
        })
    };

    let left_count = Arc::new(AtomicUsize::new(0));
    let right_count = Arc::new(AtomicUsize::new(0));
    let (ready_left_tx, ready_left_rx) = oneshot::channel();
    let (ready_right_tx, ready_right_rx) = oneshot::channel();

    let left_task = spawn_listener(
        router.messenger().await,
        left_inst,
        ready_left_tx,
        Arc::clone(&left_count),
    );
    let right_task = spawn_listener(
        router.messenger().await,
        right_inst,
        ready_right_tx,
        Arc::clone(&right_count),
    );

    ready_left_rx.await.expect("left listener ready");
    ready_right_rx.await.expect("right listener ready");

    let caller_handle = router.messenger().await;
    for pinned_inst in [left_inst, right_inst] {
        let response = ServiceMessenger::poll(
            &caller_handle,
            CALLER_CORE_NODE,
            CALLER_INSTANCE_ID,
            test_node_target(node_name),
            service_name,
            // fully pinned: no discovery probe is issued
            ServiceTarget::Producer(&ProducerRef::new(shared_core, pinned_inst)),
            Payload::from_static(b"mode=position"),
            Duration::from_secs(2),
        )
        .await
        .unwrap_or_else(|e| panic!("pinned poll to {pinned_inst} should succeed: {e}"));

        assert_eq!(response.instance_id(), pinned_inst);
        assert_eq!(response.core_node(), shared_core);
        assert_eq!(response.payload().as_ref(), pinned_inst.as_bytes());
    }

    left_task
        .await
        .expect("left task panicked")
        .expect("left task errored");
    right_task
        .await
        .expect("right task panicked")
        .expect("right task errored");

    assert_eq!(
        left_count.load(Ordering::SeqCst),
        1,
        "left handler runs once"
    );
    assert_eq!(
        right_count.load(Ordering::SeqCst),
        1,
        "right handler runs once"
    );

    router.shutdown().await;
}

/// Busy-producer variant of the matrix corner (the field failure's
/// mechanism): the pinned producer exists and is alive, but its task is
/// not parked in the service recv loop for a window longer than
/// `DISCOVERY_TIMEOUT` (exactly like a generated node whose accept-loop
/// is executing user work between `recv` parks). A pinned poll with a
/// caller budget much larger than the busy window must still succeed —
/// the call may not die at a discovery cliff the caller never asked for.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn service_poll_same_core_pinned_producer_busy_waits_caller_budget() {
    let router = TestRouterContext::start().await;

    let node_name = "openarm_mujoco";
    let service_name = "set_arm_mode";
    let shared_core = "core_same";
    let left_inst = "left_arm_inst";

    // Busy window > DISCOVERY_TIMEOUT (2s), caller budget >> busy window.
    let busy_window = Duration::from_secs(4);
    let caller_budget = Duration::from_secs(10);

    let call_count = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = oneshot::channel();

    let producer_task = {
        let handle = router.messenger().await;
        let call_count = Arc::clone(&call_count);
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                shared_core,
                left_inst,
                test_node_target(node_name),
                service_name,
            )
            .await
            .expect("service should start");

            ready_tx.send(()).unwrap();
            // Busy: queryable is declared, but nobody is parked in the
            // recv loop, so nothing answers wire traffic in-process.
            tokio::time::sleep(busy_window).await;

            let handler = service.handle_next_request(|_request| {
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from_static(b"ack"))
                }
            });
            let _ = tokio::time::timeout(Duration::from_secs(8), handler).await;
            Ok::<(), Error>(())
        })
    };

    ready_rx.await.expect("listener ready");

    let caller_handle = router.messenger().await;
    let started = std::time::Instant::now();
    let result = ServiceMessenger::poll(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        service_name,
        // fully pinned: no discovery probe is issued
        ServiceTarget::Producer(&ProducerRef::new(shared_core, left_inst)),
        Payload::from_static(b"mode=position"),
        caller_budget,
    )
    .await;
    let elapsed = started.elapsed();

    let response = result.unwrap_or_else(|e| {
        panic!(
            "pinned poll must survive a busy producer and use the caller's \
             own budget; failed after {elapsed:?}: {e}"
        )
    });
    assert_eq!(response.instance_id(), left_inst);
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "handler runs once");

    producer_task
        .await
        .expect("producer task panicked")
        .expect("producer task errored");

    router.shutdown().await;
}

/// Action mirror of
/// [`service_poll_same_core_distinct_instances_pinned_routes_to_pinned`]:
/// two idle instances of one node on one core_node; a pinned `send_goal`
/// to each must run exactly that instance's goal handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_send_goal_same_core_distinct_instances_pinned_routes_to_pinned() {
    let router = TestRouterContext::start().await;

    let node_name = "openarm_mujoco";
    let action_name = "move_arm_joints";
    let shared_core = "core_same";
    let left_inst = "left_arm_inst";
    let right_inst = "right_arm_inst";

    async fn spawn_arm(
        router: &TestRouterContext,
        inst: &'static str,
        target: SenderTarget,
        action_name: &'static str,
        goal_count: Arc<AtomicUsize>,
        ready: oneshot::Sender<()>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = router.messenger().await;
        tokio::spawn(async move {
            let action = ActionMessenger::expose(&handle, "core_same", inst, target, action_name)
                .await
                .expect("expose should succeed");
            let mut goal_service = action.goal_service;
            ready.send(()).expect("ready");

            // The sibling of the pinned instance must never receive a goal;
            // it just times out here.
            if let Ok(Ok(Some((_ctx, goal_responder)))) =
                tokio::time::timeout(Duration::from_secs(5), goal_service.recv_next_request()).await
            {
                goal_count.fetch_add(1, Ordering::SeqCst);
                goal_responder
                    .respond(Payload::from(inst.as_bytes().to_vec()))
                    .await
                    .expect("goal respond");
            }
        })
    }

    let left_count = Arc::new(AtomicUsize::new(0));
    let right_count = Arc::new(AtomicUsize::new(0));
    let (ready_left_tx, ready_left_rx) = oneshot::channel();
    let (ready_right_tx, ready_right_rx) = oneshot::channel();

    let left_task = spawn_arm(
        &router,
        left_inst,
        test_node_target(node_name),
        action_name,
        Arc::clone(&left_count),
        ready_left_tx,
    )
    .await;
    let right_task = spawn_arm(
        &router,
        right_inst,
        test_node_target(node_name),
        action_name,
        Arc::clone(&right_count),
        ready_right_tx,
    )
    .await;

    ready_left_rx.await.expect("left arm ready");
    ready_right_rx.await.expect("right arm ready");

    let caller_handle = router.messenger().await;
    for pinned_inst in [left_inst, right_inst] {
        let goal_handle = ActionMessenger::send_goal(
            &caller_handle,
            "caller_core",
            "caller_inst",
            test_node_target(node_name),
            action_name,
            // fully pinned: no discovery probe is issued
            Some(&ProducerRef::new(shared_core, pinned_inst)),
            Payload::from_static(b"go"),
            QoSProfile::Reliable,
            Duration::from_secs(2),
        )
        .await
        .unwrap_or_else(|e| panic!("pinned send_goal to {pinned_inst} should succeed: {e}"));

        assert_eq!(goal_handle.goal_response().instance_id(), pinned_inst);
        assert_eq!(goal_handle.goal_response().core_node(), shared_core);
    }

    left_task.await.expect("left arm task panicked");
    right_task.await.expect("right arm task panicked");

    assert_eq!(
        left_count.load(Ordering::SeqCst),
        1,
        "left goal handler runs once"
    );
    assert_eq!(
        right_count.load(Ordering::SeqCst),
        1,
        "right goal handler runs once"
    );

    router.shutdown().await;
}

/// Action mirror of
/// [`service_poll_same_core_pinned_producer_busy_waits_caller_budget`] —
/// this is the exact shape of the bimanual `fire_goal` timeout from the
/// field: the pinned arm is alive but mid-work (not parked in
/// `recv_next_goal`), and the pinned goal must wait out the caller's own
/// `goal_timeout` rather than failing at a 2s discovery cliff.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn action_send_goal_same_core_pinned_producer_busy_waits_caller_budget() {
    let router = TestRouterContext::start().await;

    let node_name = "openarm_mujoco";
    let action_name = "move_arm_joints";
    let shared_core = "core_same";
    let left_inst = "left_arm_inst";

    let busy_window = Duration::from_secs(4);
    let caller_budget = Duration::from_secs(10);

    let goal_count = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = oneshot::channel();

    let producer_task = {
        let handle = router.messenger().await;
        let goal_count = Arc::clone(&goal_count);
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                shared_core,
                left_inst,
                test_node_target(node_name),
                action_name,
            )
            .await
            .expect("expose should succeed");
            let mut goal_service = action.goal_service;
            ready_tx.send(()).unwrap();

            // Busy: exactly what a generated accept-loop looks like while
            // user code executes a goal — the queryable exists, but nothing
            // is parked in the goal recv loop.
            tokio::time::sleep(busy_window).await;

            if let Ok(Ok(Some((_ctx, goal_responder)))) =
                tokio::time::timeout(Duration::from_secs(8), goal_service.recv_next_request()).await
            {
                goal_count.fetch_add(1, Ordering::SeqCst);
                goal_responder
                    .respond(Payload::from_static(b"done"))
                    .await
                    .expect("goal respond");
            }
        })
    };

    ready_rx.await.expect("arm ready");

    let caller_handle = router.messenger().await;
    let started = std::time::Instant::now();
    let result = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core",
        "caller_inst",
        test_node_target(node_name),
        action_name,
        // fully pinned: no discovery probe is issued
        Some(&ProducerRef::new(shared_core, left_inst)),
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        caller_budget,
    )
    .await;
    let elapsed = started.elapsed();

    let goal_handle = result.unwrap_or_else(|e| {
        panic!(
            "pinned send_goal must survive a busy producer and use the \
             caller's own goal budget; failed after {elapsed:?}: {e}"
        )
    });
    assert_eq!(goal_handle.goal_response().instance_id(), left_inst);
    assert_eq!(
        goal_count.load(Ordering::SeqCst),
        1,
        "goal handler runs once"
    );

    producer_task.await.expect("producer task panicked");

    router.shutdown().await;
}

/// Acceptance criterion 1, observed on the wire rather than by reading
/// code: a fully pinned `ServiceMessenger::poll` and
/// `ActionMessenger::send_goal` issue ZERO discovery probes, while a
/// `None`-target call issues at least one.
///
/// Mechanism: a raw zenoh client session declares a `**` queryable that
/// never replies. Every peppy service query is sent with
/// `QueryTarget::All` + `ConsolidationMode::None`, so each query is
/// delivered to every matching queryable — replying or not — and the
/// watcher observes exactly the queries the caller puts on the wire.
/// Dropping the query without replying finalizes it for the watcher, so
/// the watched calls are not delayed. The mandatory query attachment is
/// `[0x03 magic, kind]` with kind `0x00` = UserRequest, `0x01` = Probe
/// (see `pmi::wire::zenoh_format::ServiceQueryAttachment`), which is what
/// lets the watcher discriminate probes wire-level.
///
/// The watcher seeing the pinned calls' UserRequests (asserted `>= 1`
/// each) is what makes the zero-probe assertion meaningful: it proves the
/// watcher observes this traffic at all. The control listener under a
/// different unique name proves the same watcher counts discovery probes
/// when a wildcard call does issue them.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn pinned_calls_issue_zero_probes() {
    let router = TestRouterContext::start().await;

    // Unique names so the watcher's substring match can never collide with
    // traffic from another endpoint in this test (or stray keyexprs).
    let node_name = "probefree_node";
    let pinned_service_name = "probefree_svc";
    let pinned_action_name = "probefree_act";
    let control_service_name = "probefree_ctrl";

    let producer_core = "watch_core";
    let producer_inst = "watch_inst";

    // Per-name wire counters, bumped by the watcher queryable. Probe and
    // user-request kinds are tracked separately per unique name so the
    // control call's probes cannot pollute the pinned-call assertions.
    let svc_probes = Arc::new(AtomicUsize::new(0));
    let svc_user_requests = Arc::new(AtomicUsize::new(0));
    let act_probes = Arc::new(AtomicUsize::new(0));
    let act_user_requests = Arc::new(AtomicUsize::new(0));
    let ctrl_probes = Arc::new(AtomicUsize::new(0));
    let ctrl_user_requests = Arc::new(AtomicUsize::new(0));
    // Any matching query with a missing/short/unknown attachment lands
    // here; asserted 0 so a wire-format drift can't silently turn the
    // probe counters into undercounts.
    let malformed_attachments = Arc::new(AtomicUsize::new(0));

    // Producers first (each in its own scope, like the same_core tests),
    // so the pinned calls have someone to answer them.
    let svc_call_count = Arc::new(AtomicUsize::new(0));
    let (svc_ready_tx, svc_ready_rx) = oneshot::channel();
    let service_task = {
        let handle = router.messenger().await;
        let svc_call_count = Arc::clone(&svc_call_count);
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                producer_core,
                producer_inst,
                test_node_target(node_name),
                pinned_service_name,
            )
            .await
            .expect("pinned service should start");

            let handler = service.handle_next_request(|_request| {
                let svc_call_count = Arc::clone(&svc_call_count);
                async move {
                    svc_call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from_static(b"ack"))
                }
            });

            svc_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .expect("pinned service should receive the pinned request")
                .expect("pinned service request errored");
            assert!(
                handled,
                "service subscription closed before handling request"
            );
            Ok::<(), Error>(())
        })
    };

    let act_goal_count = Arc::new(AtomicUsize::new(0));
    let (act_ready_tx, act_ready_rx) = oneshot::channel();
    let action_task = {
        let handle = router.messenger().await;
        let act_goal_count = Arc::clone(&act_goal_count);
        tokio::spawn(async move {
            let action = ActionMessenger::expose(
                &handle,
                producer_core,
                producer_inst,
                test_node_target(node_name),
                pinned_action_name,
            )
            .await
            .expect("expose should succeed");
            let mut goal_service = action.goal_service;
            act_ready_tx.send(()).expect("ready");

            let (_ctx, goal_responder) =
                tokio::time::timeout(Duration::from_secs(10), goal_service.recv_next_request())
                    .await
                    .expect("timed out waiting for the pinned goal")
                    .expect("goal recv errored")
                    .expect("goal subscription closed before the pinned goal");
            act_goal_count.fetch_add(1, Ordering::SeqCst);
            goal_responder
                .respond(Payload::from_static(b"accepted"))
                .await
                .expect("goal respond");
        })
    };

    svc_ready_rx.await.expect("pinned service ready");
    act_ready_rx.await.expect("pinned action ready");
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Raw zenoh watcher session: a plain client against the test router,
    // mirroring pmi's discovery model (multicast scouting off) so it can
    // never scout a stray zenoh process outside this test.
    let mut config = zenoh::Config::default();
    config
        .insert_json5("mode", "\"client\"")
        .expect("set watcher mode");
    config
        .insert_json5(
            "connect/endpoints",
            &format!("[\"tcp/{}:{}\"]", router.host(), router.port()),
        )
        .expect("set watcher endpoints");
    config
        .insert_json5("scouting/multicast/enabled", "false")
        .expect("disable watcher multicast scouting");
    let watcher_session = zenoh::open(config).await.expect("watcher session");

    let watcher_queryable = {
        let svc_probes = Arc::clone(&svc_probes);
        let svc_user_requests = Arc::clone(&svc_user_requests);
        let act_probes = Arc::clone(&act_probes);
        let act_user_requests = Arc::clone(&act_user_requests);
        let ctrl_probes = Arc::clone(&ctrl_probes);
        let ctrl_user_requests = Arc::clone(&ctrl_user_requests);
        let malformed_attachments = Arc::clone(&malformed_attachments);
        watcher_session
            .declare_queryable("**")
            .callback(move |query| {
                let key = query.key_expr().as_str();
                let counters = if key.contains(pinned_service_name) {
                    Some((&svc_probes, &svc_user_requests))
                } else if key.contains(pinned_action_name) {
                    Some((&act_probes, &act_user_requests))
                } else if key.contains(control_service_name) {
                    Some((&ctrl_probes, &ctrl_user_requests))
                } else {
                    None
                };
                let Some((probes, user_requests)) = counters else {
                    return;
                };
                // `[0x03 magic, kind]`: 0x00 = UserRequest, 0x01 = Probe
                // (crates/pmi-internal/src/wire/zenoh_format.rs).
                match query.attachment().map(|a| a.to_bytes()).as_deref() {
                    Some([0x03, 0x00]) => {
                        user_requests.fetch_add(1, Ordering::SeqCst);
                    }
                    Some([0x03, 0x01]) => {
                        probes.fetch_add(1, Ordering::SeqCst);
                    }
                    _ => {
                        malformed_attachments.fetch_add(1, Ordering::SeqCst);
                    }
                }
                // Never reply: dropping the query finalizes it for the
                // watcher without contributing a response.
            })
            .await
            .expect("watcher queryable")
    };

    // Open the caller before the settle so the fresh peer has time to learn
    // both the producers' queryables and the watcher's `**` interest —
    // otherwise its first queries would not be routed to the watcher and
    // the meaningfulness guard below would fail.
    let caller_handle = router.messenger().await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    let pinned = ProducerRef::new(producer_core, producer_inst);

    let response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        pinned_service_name,
        // fully pinned: must issue zero discovery probes
        ServiceTarget::Producer(&pinned),
        Payload::from_static(b"enable=true"),
        Duration::from_secs(5),
    )
    .await
    .expect("pinned poll should succeed");
    assert_eq!(response.core_node(), producer_core);
    assert_eq!(response.instance_id(), producer_inst);

    let goal_handle = ActionMessenger::send_goal(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        pinned_action_name,
        // fully pinned: must issue zero discovery probes
        Some(&pinned),
        Payload::from_static(b"go"),
        QoSProfile::Reliable,
        Duration::from_secs(5),
    )
    .await
    .expect("pinned send_goal should succeed");
    assert_eq!(goal_handle.goal_response().core_node(), producer_core);
    assert_eq!(goal_handle.goal_response().instance_id(), producer_inst);

    // Wire delivery to the watcher is asynchronous; give it a moment
    // before reading the counters.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Meaningfulness guard first: the watcher must have observed the real
    // (UserRequest) queries of both pinned calls, otherwise "zero probes"
    // would be vacuously true of a blind watcher.
    assert!(
        svc_user_requests.load(Ordering::SeqCst) >= 1,
        "watcher must observe the pinned service query on the wire",
    );
    assert!(
        act_user_requests.load(Ordering::SeqCst) >= 1,
        "watcher must observe the pinned goal query on the wire",
    );
    assert_eq!(
        svc_probes.load(Ordering::SeqCst),
        0,
        "a fully pinned poll must not issue a discovery probe",
    );
    assert_eq!(
        act_probes.load(Ordering::SeqCst),
        0,
        "a fully pinned send_goal must not issue a discovery probe",
    );

    // Control: the same watcher must count discovery probes when a
    // `None`-target call does issue them, proving the zero-probe counters
    // above would have caught a probe had one been sent.
    let (ctrl_ready_tx, ctrl_ready_rx) = oneshot::channel();
    let control_task = {
        let handle = router.messenger().await;
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                "ctrl_core",
                "ctrl_inst",
                test_node_target(node_name),
                control_service_name,
            )
            .await
            .expect("control service should start");

            let handler = service
                .handle_next_request(|_request| async move { Ok(Payload::from_static(b"ack")) });

            ctrl_ready_tx.send(()).unwrap();
            let handled = tokio::time::timeout(Duration::from_secs(10), handler)
                .await
                .expect("control service should receive the discovered request")
                .expect("control service request errored");
            assert!(
                handled,
                "control subscription closed before handling request"
            );
            Ok::<(), Error>(())
        })
    };

    ctrl_ready_rx.await.expect("control service ready");
    tokio::time::sleep(Duration::from_millis(100)).await;

    let control_response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        control_service_name,
        ServiceTarget::Any, // full wildcard: discover-then-pin must probe
        Payload::from_static(b"enable=true"),
        Duration::from_secs(5),
    )
    .await
    .expect("control wildcard poll should succeed");
    assert_eq!(control_response.core_node(), "ctrl_core");
    assert_eq!(control_response.instance_id(), "ctrl_inst");

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        ctrl_probes.load(Ordering::SeqCst) >= 1,
        "a None-target poll must issue at least one discovery probe \
         (got {} probes / {} user requests)",
        ctrl_probes.load(Ordering::SeqCst),
        ctrl_user_requests.load(Ordering::SeqCst),
    );
    assert_eq!(
        malformed_attachments.load(Ordering::SeqCst),
        0,
        "every observed service query must carry a well-formed attachment",
    );

    service_task
        .await
        .expect("service task panicked")
        .expect("service task errored");
    action_task.await.expect("action task panicked");
    control_task
        .await
        .expect("control task panicked")
        .expect("control task errored");

    assert_eq!(
        svc_call_count.load(Ordering::SeqCst),
        1,
        "pinned service handler runs once"
    );
    assert_eq!(
        act_goal_count.load(Ordering::SeqCst),
        1,
        "pinned goal handler runs once"
    );

    drop(watcher_queryable);
    watcher_session
        .close()
        .await
        .expect("watcher session close");

    router.shutdown().await;
}

/// Acceptance criterion 2 — the cross-core same-`instance_id` leak is
/// closed: topic subscriptions pin and filter on the full
/// `(core_node, instance_id)` pair, never on `instance_id` alone. Two
/// producers share `instance_id` "inst1" on different core_nodes:
///
/// - `Pin` pins both wire slots, so the same-instance_id producer on the
///   other core never matches on the wire;
/// - a multi-producer `OnlyFrom` rides a wire wildcard plus an in-process
///   allow set that must compare full pairs;
/// - `AnyExcept` rides a wire wildcard plus an in-process deny set that
///   must reject only the listed pair — pre-fix, comparing `instance_id`
///   alone also dropped the same-instance_id producer on the other core.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn topic_pinned_and_filtered_subscriptions_compare_full_pairs() {
    let router = TestRouterContext::start().await;

    let qos = QoSProfile::Reliable;
    let node_name = "pair_node";
    let topic = "pair_topic";
    let shared_inst = "inst1";
    let core_a = "core_a";
    let core_b = "core_b";
    let payload_a = Payload::from_static(b"from_core_a");
    let payload_b = Payload::from_static(b"from_core_b");

    // Subscribers first so the emit loops publish into live subscriptions.
    // Each subscriber gets its own messenger handle: the wildcard filters
    // are from_any slots, and one messenger allows at most one live
    // from_any subscription per producer pin and `(name, tag)`.
    let pin_handle = router.messenger().await;
    let mut pin_sub = TopicMessenger::subscribe(
        &pin_handle,
        "sub_pin_core",
        "sub_pin_inst",
        Some(test_node_target(node_name)),
        false,
        topic,
        &ConsumerFilter::Pin(ProducerRef::new(core_a, shared_inst)),
        qos.clone(),
    )
    .await
    .expect("pin subscribe should succeed");

    // Two-entry set forces the wire-wildcard + in-process allow-set path
    // (a single-entry set would collapse to a wire pin); the second entry
    // names a producer that never publishes.
    let only_from_handle = router.messenger().await;
    let only_from_filter = ConsumerFilter::OnlyFrom(vec![
        ProducerRef::new(core_a, shared_inst),
        ProducerRef::new(core_a, "inst2"),
    ]);
    let mut only_from_sub = TopicMessenger::subscribe(
        &only_from_handle,
        "sub_only_core",
        "sub_only_inst",
        Some(test_node_target(node_name)),
        true,
        topic,
        &only_from_filter,
        qos.clone(),
    )
    .await
    .expect("only_from subscribe should succeed");

    let any_except_handle = router.messenger().await;
    let any_except_filter = ConsumerFilter::AnyExcept(vec![ProducerRef::new(core_a, shared_inst)]);
    let mut any_except_sub = TopicMessenger::subscribe(
        &any_except_handle,
        "sub_except_core",
        "sub_except_inst",
        Some(test_node_target(node_name)),
        true,
        topic,
        &any_except_filter,
        qos.clone(),
    )
    .await
    .expect("any_except subscribe should succeed");

    // Emit loops: each producer publishes every 50ms until stopped, so the
    // assertions below never depend on a single publish surviving
    // peer-mode discovery propagation.
    let stop_emitters = Arc::new(tokio::sync::Notify::new());
    let spawn_emitter = |handle: MessengerHandle, core: &'static str, payload: Payload| {
        let stop = Arc::clone(&stop_emitters);
        let qos = qos.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_millis(50));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let stop_notified = stop.notified();
            tokio::pin!(stop_notified);
            loop {
                tokio::select! {
                    biased;
                    _ = stop_notified.as_mut() => break,
                    _ = ticker.tick() => {
                        publish_once(
                            &handle,
                            core,
                            shared_inst,
                            test_node_target(node_name),
                            topic,
                            qos.clone(),
                            payload.clone(),
                        )
                        .await
                        .expect("emit should succeed");
                    }
                }
            }
        })
    };
    let emitter_a = spawn_emitter(router.messenger().await, core_a, payload_a.clone());
    let emitter_b = spawn_emitter(router.messenger().await, core_b, payload_b.clone());

    // Pin: both wire slots are pinned to (core_a, inst1) — every delivered
    // message must carry that pair even though core_b publishes the same
    // instance_id on the same topic throughout.
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(5), pin_sub.on_next_message())
            .await
            .expect("pinned subscriber timed out waiting for a message")
            .expect("pinned subscription closed");
        assert_eq!(
            msg.core_node(),
            core_a,
            "Pin must reject the same-instance_id producer on another core",
        );
        assert_eq!(msg.instance_id(), shared_inst);
        assert_eq!(msg.payload(), &payload_a);
    }

    // OnlyFrom: the wire wildcard receives both producers; the in-process
    // allow set must admit only the listed (core_a, inst1) pair.
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(5), only_from_sub.on_next_message())
            .await
            .expect("only_from subscriber timed out waiting for a message")
            .expect("only_from subscription closed");
        assert_eq!(
            msg.core_node(),
            core_a,
            "OnlyFrom must compare full pairs, not instance_id alone",
        );
        assert_eq!(msg.instance_id(), shared_inst);
        assert_eq!(msg.payload(), &payload_a);
    }

    // AnyExcept: the deny set lists (core_a, inst1); the same-instance_id
    // producer on core_b must NOT be rejected, while core_a's must be.
    for _ in 0..3 {
        let msg = tokio::time::timeout(Duration::from_secs(5), any_except_sub.on_next_message())
            .await
            .expect("any_except subscriber timed out waiting for a message")
            .expect("any_except subscription closed");
        assert_eq!(
            msg.core_node(),
            core_b,
            "AnyExcept must reject only the listed pair — a shared \
             instance_id on another core must pass",
        );
        assert_eq!(msg.instance_id(), shared_inst);
        assert_eq!(msg.payload(), &payload_b);
    }

    // Stop the emit loops before tearing the router down so no emit races
    // the shutdown.
    stop_emitters.notify_waiters();
    emitter_a.await.expect("emitter A panicked");
    emitter_b.await.expect("emitter B panicked");

    router.shutdown().await;
}

/// Discovery-hardening at the peppylib level: a producer that is alive but
/// NOT parked in its service recv loop (busy in user code, exactly like a
/// generated accept-loop between `recv` parks) must still answer probes
/// within the probe budget, because the pmi transport adapter answers
/// `ServiceQueryKind::Probe` queries in its dispatch callback — never the
/// endpoint recv loop. The busy window (3s) exceeds both `PROBE_TIMEOUT`
/// (500ms) and `DISCOVERY_TIMEOUT` (2s): if probes were answered by the
/// recv loop, both probe calls below would starve inside the window.
#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn probes_answered_while_pinned_producer_is_busy() {
    let router = TestRouterContext::start().await;

    let node_name = "busy_probe_node";
    let service_name = "busy_probe_svc";
    let busy_core = "busy_core";
    let busy_inst = "busy_inst";

    let busy_window = Duration::from_secs(3);

    let call_count = Arc::new(AtomicUsize::new(0));
    let (ready_tx, ready_rx) = oneshot::channel();

    let producer_task = {
        let handle = router.messenger().await;
        let call_count = Arc::clone(&call_count);
        tokio::spawn(async move {
            let mut service = ServiceMessenger::listen(
                &handle,
                busy_core,
                busy_inst,
                test_node_target(node_name),
                service_name,
            )
            .await
            .expect("service should start");

            ready_tx.send(()).unwrap();
            // Busy: the queryable is declared, but nobody is parked in the
            // recv loop — only the adapter can answer wire traffic.
            tokio::time::sleep(busy_window).await;

            let handler = service.handle_next_request(|_request| {
                let call_count = Arc::clone(&call_count);
                async move {
                    call_count.fetch_add(1, Ordering::SeqCst);
                    Ok(Payload::from_static(b"ack"))
                }
            });
            let handled = tokio::time::timeout(Duration::from_secs(8), handler)
                .await
                .expect("producer should receive the pinned request after waking")
                .expect("producer request errored");
            assert!(
                handled,
                "service subscription closed before handling request"
            );
            Ok::<(), Error>(())
        })
    };

    ready_rx.await.expect("producer ready");
    // Open the caller before the probe delay so the fresh peer has the
    // window to learn the producer's queryable; then probe ~200ms into the
    // busy window so the producer is provably parked in user code.
    let caller_handle = router.messenger().await;
    tokio::time::sleep(Duration::from_millis(200)).await;

    let started = std::time::Instant::now();
    let reachable = ServiceMessenger::is_reachable(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        service_name,
        ServiceTarget::Any,
    )
    .await
    .expect("is_reachable should not error");
    let elapsed = started.elapsed();
    assert!(
        reachable,
        "adapter must answer the probe while the producer is busy",
    );
    assert!(
        elapsed < Duration::from_millis(1500),
        "probe must resolve within the probe budget, not the busy window; \
         took {elapsed:?}",
    );

    let (_latency, response_bytes) = ServiceMessenger::probe_latency(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        service_name,
        ServiceTarget::Any,
        Duration::from_secs(2),
        64,   // request_size
        4096, // response_size
    )
    .await
    .expect("sized probe should round-trip while the producer is busy");
    assert_eq!(
        response_bytes, 4096,
        "adapter must serve the benchmark-sized probe while the producer is busy",
    );

    // The pinned user request still reaches the handler once the producer
    // wakes — the adapter's probe path and the user path stay independent.
    let response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core_node",
        "caller_instance",
        test_node_target(node_name),
        service_name,
        ServiceTarget::Producer(&ProducerRef::new(busy_core, busy_inst)),
        Payload::from_static(b"enable=true"),
        Duration::from_secs(10),
    )
    .await
    .expect("pinned poll should succeed once the producer wakes");
    assert_eq!(response.core_node(), busy_core);
    assert_eq!(response.instance_id(), busy_inst);
    assert_eq!(call_count.load(Ordering::SeqCst), 1, "handler runs once");

    producer_task
        .await
        .expect("producer task panicked")
        .expect("producer task errored");

    router.shutdown().await;
}
