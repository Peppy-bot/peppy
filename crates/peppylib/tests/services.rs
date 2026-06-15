mod common;

use common::test_node_target;
use peppylib::messaging::{
    MessengerHandle, ProducerRef, SenderTarget, ServiceMessenger, ServiceTarget,
};
use peppylib::types::Payload;
use pmi::ZenohAdapter;
use std::time::Duration;
use tokio::sync::oneshot;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_messenger_communication() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let service_name = "test_service";
    let request_payload = Payload::from_static(b"Hello request");
    let response_payload = Payload::from_static(b"Hello response");

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create client handle");

    // Start the service listener
    let mut service = ServiceMessenger::listen(
        &server_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        service_name,
    )
    .await
    .expect("listen should succeed");

    // No settle sleep: `poll` below does discover-then-pin with a built-in
    // cold-start retry bounded by its timeout, so it waits for the listener's
    // queryable to propagate rather than guessing a fixed delay.

    // Spawn the handler so we can poll concurrently
    let response_clone = response_payload.clone();
    let handler = tokio::spawn(async move {
        service
            .handle_next_request(|_request| async move { Ok(response_clone) })
            .await
            .expect("handle_next_request should succeed");
    });

    // Poll the service as a client
    let response = ServiceMessenger::poll(
        &client_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        service_name,
        ServiceTarget::Producer(&ProducerRef::new(core_node, instance_id)),
        request_payload,
        Duration::from_secs(2),
    )
    .await
    .expect("poll should succeed");

    handler.await.expect("handler task should not panic");

    assert_eq!(response.payload(), &response_payload);
    assert_eq!(response.instance_id(), instance_id);
    assert_eq!(response.core_node(), core_node);
}

/// A single node exposes the *same* service name under two distinct iface
/// scopes (native + a conformed interface). The wire-path scoping must keep
/// them independently addressable: a caller targeting one scope must never see
/// responses from the other.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_iface_scoped_native_and_conformed_do_not_collide() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let node_name = "test_node";
    let service_name = "control";
    let iface_name = "camera";
    let iface_tag = "v1";

    let native_response = Payload::from_static(b"from_native");
    let iface_response = Payload::from_static(b"from_iface");

    let native_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create native handle");
    let iface_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create iface handle");
    let caller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create caller handle");

    let (native_ready_tx, native_ready_rx) = oneshot::channel();
    let mut native_endpoint = ServiceMessenger::listen(
        &native_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        service_name,
    )
    .await
    .expect("native listen should succeed");

    let native_response_clone = native_response.clone();
    let native_handler = tokio::spawn(async move {
        native_ready_tx.send(()).unwrap();
        native_endpoint
            .handle_next_request(|_req| async move { Ok(native_response_clone) })
            .await
            .expect("native handler should succeed");
    });
    native_ready_rx.await.unwrap();

    let (iface_ready_tx, iface_ready_rx) = oneshot::channel();
    let mut iface_endpoint = ServiceMessenger::listen(
        &iface_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        service_name,
    )
    .await
    .expect("iface listen should succeed");

    let iface_response_clone = iface_response.clone();
    let iface_handler = tokio::spawn(async move {
        iface_ready_tx.send(()).unwrap();
        iface_endpoint
            .handle_next_request(|_req| async move { Ok(iface_response_clone) })
            .await
            .expect("iface handler should succeed");
    });
    iface_ready_rx.await.unwrap();

    // No settle sleep: both polls below self-retry on a cold-start miss within
    // their timeout until each scope's queryable propagates.

    // Poll the native scope and assert we get the native response.
    let from_native = ServiceMessenger::poll(
        &caller_handle,
        core_node,
        instance_id,
        test_node_target(node_name),
        service_name,
        ServiceTarget::Producer(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"ping_native"),
        Duration::from_secs(2),
    )
    .await
    .expect("native poll should succeed");
    assert_eq!(
        from_native.payload(),
        &native_response,
        "native scope must receive the native handler's response"
    );

    // Poll the iface scope and assert we get the iface response.
    let from_iface = ServiceMessenger::poll(
        &caller_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, iface_tag).expect("test target"),
        service_name,
        ServiceTarget::Producer(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"ping_iface"),
        Duration::from_secs(2),
    )
    .await
    .expect("iface poll should succeed");
    assert_eq!(
        from_iface.payload(),
        &iface_response,
        "iface scope must receive the iface handler's response"
    );

    native_handler.await.expect("native handler task panicked");
    iface_handler.await.expect("iface handler task panicked");
}

/// Hyphens in `iface_tag` must be normalized to underscores at the wire-format
/// boundary, so a caller that passes `"v2-stable"` and a listener that passes
/// `"v2_stable"` end up on the same wire path.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn service_iface_tag_hyphen_normalized() {
    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let core_node = "test_core";
    let instance_id = "test_instance";
    let service_name = "control";
    let iface_name = "camera";

    let response_payload = Payload::from_static(b"ack");

    let server_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create server handle");
    let client_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("failed to create client handle");

    // Listener uses hyphen.
    let mut endpoint = ServiceMessenger::listen(
        &server_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, "v2-stable").expect("test target"),
        service_name,
    )
    .await
    .expect("listen should succeed");

    let response_clone = response_payload.clone();
    let handler = tokio::spawn(async move {
        endpoint
            .handle_next_request(|_req| async move { Ok(response_clone) })
            .await
            .expect("handler should succeed");
    });

    // No settle sleep: the poll below self-retries on a cold-start miss within
    // its timeout until the listener's queryable propagates.

    // Caller uses underscore. Both should normalize to the same wire segment.
    let response = ServiceMessenger::poll(
        &client_handle,
        core_node,
        instance_id,
        SenderTarget::interface(iface_name, "v2_stable").expect("test target"),
        service_name,
        ServiceTarget::Producer(&ProducerRef::new(core_node, instance_id)),
        Payload::from_static(b"ping"),
        Duration::from_secs(2),
    )
    .await
    .expect("poll should succeed");

    handler.await.expect("handler task panicked");
    assert_eq!(response.payload(), &response_payload);
}

/// Discover-then-pin safety: when a consumer issues a wildcard
/// `ServiceMessenger::poll` (target_instance_id = None) against two producers
/// exposing the same `(name, tag)`, only the discovered producer must run its
/// user handler. The other receives only the discovery probe (filtered
/// server-side before the handler runs) and stays idle.
///
/// Without this property a state-changing service would execute on every
/// matching producer; for actions this would be a real-world safety hazard
/// (multiple robots executing the same goal). The wire layer alone cannot
/// give this guarantee — `QueryTarget::All` broadcasts the request — so it
/// is enforced by `ServiceMessenger::poll`'s discover-then-pin sequence.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn service_from_any_poll_runs_handler_on_winner_only() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let instance = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("failed to start zenoh router for test");
    let (host, port) = (instance.host.clone(), instance.port);

    let producer_node_name = "manipulator";
    let service_name = "abort_safe";
    let producer_a_core = "producer_a_core";
    let producer_a_inst = "producer_a";
    let producer_b_core = "producer_b_core";
    let producer_b_inst = "producer_b";

    struct ProducerSpec {
        core: &'static str,
        inst: &'static str,
        node_name: &'static str,
        service_name: &'static str,
    }

    async fn spawn_producer(
        host: String,
        port: u16,
        spec: ProducerSpec,
        handler_count: Arc<AtomicUsize>,
        ready: oneshot::Sender<()>,
        mut shutdown: tokio::sync::watch::Receiver<bool>,
    ) -> tokio::task::JoinHandle<()> {
        let handle = MessengerHandle::from_host_port(&host, port)
            .await
            .expect("connect");
        tokio::spawn(async move {
            let mut endpoint = ServiceMessenger::listen(
                &handle,
                spec.core,
                spec.inst,
                test_node_target(spec.node_name),
                spec.service_name,
            )
            .await
            .expect("listen should succeed");
            ready.send(()).expect("ready signal");

            // The winner handles the request; the loser only ever sees the
            // (server-filtered) discovery probe and is released by `shutdown`
            // once the winner has been serviced. An explicit signal rather than
            // a fixed timeout keeps the test deterministic regardless of how
            // long peer-mode discovery takes to settle.
            tokio::select! {
                res = endpoint.handle_next_request(|_req| {
                    let counter = Arc::clone(&handler_count);
                    async move {
                        counter.fetch_add(1, Ordering::SeqCst);
                        Ok(Payload::from(spec.inst.as_bytes().to_vec()))
                    }
                }) => {
                    let _ = res;
                }
                _ = shutdown.changed() => {}
            }
        })
    }

    let handler_a = Arc::new(AtomicUsize::new(0));
    let handler_b = Arc::new(AtomicUsize::new(0));
    let (ready_a_tx, ready_a_rx) = oneshot::channel();
    let (ready_b_tx, ready_b_rx) = oneshot::channel();
    let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);

    let task_a = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: producer_a_core,
            inst: producer_a_inst,
            node_name: producer_node_name,
            service_name,
        },
        Arc::clone(&handler_a),
        ready_a_tx,
        shutdown_rx.clone(),
    )
    .await;
    let task_b = spawn_producer(
        host.clone(),
        port,
        ProducerSpec {
            core: producer_b_core,
            inst: producer_b_inst,
            node_name: producer_node_name,
            service_name,
        },
        Arc::clone(&handler_b),
        ready_b_tx,
        shutdown_rx,
    )
    .await;

    ready_a_rx.await.expect("producer A ready");
    ready_b_rx.await.expect("producer B ready");

    let caller_handle = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("caller connect");

    // poll performs a from_any discover-then-pin internally. In peer mode
    // discover_producer re-probes within its budget until the producers'
    // queryables propagate to this freshly-connected caller, so no external
    // readiness gate is needed — this exercises that cold-start retry directly.
    let response = ServiceMessenger::poll(
        &caller_handle,
        "caller_core",
        "caller_inst",
        test_node_target(producer_node_name),
        service_name,
        ServiceTarget::Any, // wildcard target producer
        Payload::from_static(b"go"),
        Duration::from_secs(5),
    )
    .await
    .expect("wildcard poll should succeed");

    // Winner has been serviced; release the loser from its request wait.
    shutdown_tx.send(true).expect("signal producers to stop");

    let winner_inst = response.instance_id().to_string();
    assert!(
        winner_inst == producer_a_inst || winner_inst == producer_b_inst,
        "response identity must come from one of the producers, got {winner_inst:?}",
    );

    task_a.await.expect("producer A task panicked");
    task_b.await.expect("producer B task panicked");

    let (winner_count, loser_count) = if winner_inst == producer_a_inst {
        (
            handler_a.load(Ordering::SeqCst),
            handler_b.load(Ordering::SeqCst),
        )
    } else {
        (
            handler_b.load(Ordering::SeqCst),
            handler_a.load(Ordering::SeqCst),
        )
    };
    assert_eq!(
        winner_count, 1,
        "winning producer ({winner_inst}) should run its user handler exactly once",
    );
    assert_eq!(
        loser_count, 0,
        "losing producer must NOT run its user handler — discovery pins to the winner first",
    );
}
