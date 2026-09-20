#![cfg(feature = "build_zenoh")]

use pmi::{
    Messenger, MessengerAdapter, MessengerBackend, RouterId, SubscriberBufferSizes, ZenohAdapter,
    ZenohNetProtocol,
};
use std::net::{TcpListener, TcpStream};

fn unused_ephemeral_port() -> (u16, TcpListener) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("reserve an ephemeral port");
    let port = listener.local_addr().expect("read local address").port();
    (port, listener)
}

fn external_router_adapter(port: u16) -> ZenohAdapter {
    ZenohAdapter::with_external_router(
        &format!("tcp/127.0.0.1:{port}"),
        false,
        SubscriberBufferSizes::default(),
    )
    .expect("build external router adapter")
}

fn managed_router_adapter(port: u16) -> ZenohAdapter {
    ZenohAdapter::with_router(
        ZenohNetProtocol::Tcp,
        "127.0.0.1",
        port,
        false,
        SubscriberBufferSizes::default(),
        Vec::new(),
        None,
        RouterId::generate(),
    )
    .expect("build managed router adapter")
}

#[test]
fn external_endpoint_preserves_an_arbitrary_dial_host_and_port() {
    let adapter = ZenohAdapter::with_external_router(
        "tcp/zenoh-router.internal:17448",
        false,
        SubscriberBufferSizes::default(),
    )
    .expect("accept a hostname endpoint");

    assert_eq!(adapter.client_endpoint(), ("zenoh-router.internal", 17448));
    assert_eq!(
        adapter.client_locator().to_string(),
        "tcp/zenoh-router.internal:17448"
    );

    let messenger = Messenger::new(MessengerAdapter::Zenoh(adapter));
    assert_eq!(
        messenger
            .messaging_locator()
            .expect("Zenoh messenger has a locator")
            .to_string(),
        "tcp/zenoh-router.internal:17448"
    );
}

#[test]
fn external_endpoint_rejects_listen_wildcards_and_non_tcp_transports() {
    for endpoint in ["tcp/0.0.0.0:7448", "tcp/[::]:7448"] {
        let error =
            ZenohAdapter::with_external_router(endpoint, false, SubscriberBufferSizes::default())
                .err()
                .expect("a listen wildcard is not a dial endpoint");
        assert!(
            error.to_string().contains("listen wildcard"),
            "unexpected error for {endpoint}: {error}"
        );
    }

    let error = ZenohAdapter::with_external_router(
        "tls/router.internal:7448",
        false,
        SubscriberBufferSizes::default(),
    )
    .err()
    .expect("external adoption currently requires TCP");
    assert!(
        error.to_string().contains("must use `tcp/`"),
        "unexpected error: {error}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn adopts_a_responsive_external_router_without_owning_it() {
    let router_a = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start operator-managed router A");
    let port = router_a.port;

    let adapter_b = external_router_adapter(port);
    let mut messenger_b = Messenger::new(MessengerAdapter::Zenoh(adapter_b));
    assert!(!messenger_b.router_is_adopted());

    messenger_b
        .start_router()
        .await
        .expect("adopt responsive router A");
    assert!(messenger_b.router_is_adopted());

    messenger_b
        .stop_router()
        .await
        .expect("stopping an adopted router is a no-op");
    assert!(messenger_b.router_is_adopted());
    drop(messenger_b);

    TcpStream::connect(("127.0.0.1", port))
        .expect("operator-managed router A remains available after B is dropped");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn rejects_a_non_zenoh_process_holding_the_router_port() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind non-zenoh listener");
    let port = listener.local_addr().expect("read local address").port();
    let mut adapter = external_router_adapter(port);

    let error = adapter
        .start_router()
        .await
        .expect_err("a raw TCP listener is not a responsive zenoh router");
    assert!(
        error.to_string().contains("not a responsive Zenoh router"),
        "unexpected error: {error}"
    );
    assert!(!adapter.router_is_adopted());

    drop(listener);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fails_loud_when_the_configured_external_router_is_not_running() {
    const MAX_PORT_ATTEMPTS: usize = 32;

    let mut attempt = 0;
    let (port, adapter, start_result, listener) = loop {
        attempt += 1;
        let (port, reservation) = unused_ephemeral_port();
        let mut adapter = external_router_adapter(port);

        drop(reservation);
        let start_result = adapter.start_router().await;

        match TcpListener::bind(("127.0.0.1", port)) {
            Ok(listener) => break (port, adapter, start_result, listener),
            Err(err)
                if err.kind() == std::io::ErrorKind::AddrInUse && attempt < MAX_PORT_ATTEMPTS =>
            {
                continue;
            }
            Err(err) => {
                panic!("could not reclaim sampled port {port} after {attempt} attempts: {err}")
            }
        }
    };

    let error = start_result.expect_err("external mode must not spawn a missing router");
    let message = error.to_string();
    assert!(
        message.contains(&format!(
            "`tcp/127.0.0.1:{port}` is not accepting TCP connections"
        )),
        "unexpected error: {error}"
    );
    assert!(!adapter.router_is_adopted());

    drop(listener);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn managed_router_keeps_the_port_busy_error() {
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("hold managed router port");
    let port = listener.local_addr().expect("read local address").port();
    let mut adapter = managed_router_adapter(port);

    let error = adapter
        .start_router()
        .await
        .expect_err("managed mode must reject a busy port");
    assert!(
        error
            .to_string()
            .contains("Zenoh router port already in use"),
        "unexpected error: {error}"
    );
    assert!(!adapter.router_is_adopted());

    drop(listener);
}
