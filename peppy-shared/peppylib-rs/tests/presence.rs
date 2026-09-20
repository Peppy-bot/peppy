mod common;

use std::sync::Arc;
use std::time::Duration;

use peppylib::{CoreNodePresence, CoreNodePresenceMessenger, LivelinessEvent, MessengerHandle};
use pmi::ZenohAdapter;

use common::get_client_server;

const TIMEOUT: Duration = Duration::from_secs(1);

#[tokio::test]
async fn presence_facade_declares_watches_and_lists_with_name_filtering() {
    let (client, shared_messenger) = get_client_server().await;
    let observer = MessengerHandle::from_shared(Arc::clone(&shared_messenger));

    let first = CoreNodePresenceMessenger::declare(&client.caller_handle, "core-a", "instance-1")
        .await
        .expect("declare first presence");
    let watch = CoreNodePresenceMessenger::watch(&observer, "core-a")
        .await
        .expect("watch core-a");

    assert_eq!(
        tokio::time::timeout(TIMEOUT, watch.rx.recv_async())
            .await
            .expect("history event timeout")
            .expect("history event channel"),
        LivelinessEvent::Alive(CoreNodePresence::new("core-a", "instance-1"))
    );

    let second = CoreNodePresenceMessenger::declare(&client.caller_handle, "core-a", "instance-2")
        .await
        .expect("declare colliding presence");
    let other = CoreNodePresenceMessenger::declare(&client.caller_handle, "core-b", "instance-3")
        .await
        .expect("declare other presence");

    assert_eq!(
        tokio::time::timeout(TIMEOUT, watch.rx.recv_async())
            .await
            .expect("live event timeout")
            .expect("live event channel"),
        LivelinessEvent::Alive(CoreNodePresence::new("core-a", "instance-2"))
    );

    let mut all = CoreNodePresenceMessenger::list_live(&observer, None, TIMEOUT)
        .await
        .expect("list all presence");
    all.sort();
    assert_eq!(
        all,
        vec![
            CoreNodePresence::new("core-a", "instance-1"),
            CoreNodePresence::new("core-a", "instance-2"),
            CoreNodePresence::new("core-b", "instance-3"),
        ]
    );

    let core_a = CoreNodePresenceMessenger::list_live(&observer, Some("core-a"), TIMEOUT)
        .await
        .expect("list core-a presence");
    assert_eq!(core_a.len(), 2);
    assert!(core_a.iter().all(|presence| presence.core_node == "core-a"));

    drop(first);
    assert_eq!(
        tokio::time::timeout(TIMEOUT, watch.rx.recv_async())
            .await
            .expect("gone event timeout")
            .expect("gone event channel"),
        LivelinessEvent::Gone(CoreNodePresence::new("core-a", "instance-1"))
    );

    drop(second);
    drop(other);
}

#[tokio::test]
async fn presence_facade_rejects_wildcard_identity_segments() {
    let (client, _shared_messenger) = get_client_server().await;

    assert!(
        CoreNodePresenceMessenger::declare(&client.caller_handle, "*", "instance-1")
            .await
            .is_err()
    );
    assert!(
        CoreNodePresenceMessenger::list_live(&client.caller_handle, Some("_"), TIMEOUT)
            .await
            .is_err()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn presence_facade_round_trips_over_zenoh() {
    let router = ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start zenoh router");
    let first_handle = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("connect first daemon");
    let second_handle = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("connect second daemon");
    let observer = MessengerHandle::connect(&router.host, router.port)
        .await
        .expect("connect observer");

    let watch = CoreNodePresenceMessenger::watch(&observer, "core-a")
        .await
        .expect("watch core-a");
    let first = CoreNodePresenceMessenger::declare(&first_handle, "core-a", "instance-1")
        .await
        .expect("declare first daemon");
    let second = CoreNodePresenceMessenger::declare(&second_handle, "core-b", "instance-2")
        .await
        .expect("declare second daemon");

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), watch.rx.recv_async())
            .await
            .expect("alive event timeout")
            .expect("alive event channel"),
        LivelinessEvent::Alive(CoreNodePresence::new("core-a", "instance-1"))
    );

    let both = wait_for_presence_count(&observer, 2).await;
    assert!(both.iter().any(|presence| presence.core_node == "core-a"));
    assert!(both.iter().any(|presence| presence.core_node == "core-b"));

    drop(first);
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), watch.rx.recv_async())
            .await
            .expect("gone event timeout")
            .expect("gone event channel"),
        LivelinessEvent::Gone(CoreNodePresence::new("core-a", "instance-1"))
    );

    let remaining = wait_for_presence_count(&observer, 1).await;
    assert_eq!(
        remaining,
        vec![CoreNodePresence::new("core-b", "instance-2")]
    );

    drop(second);
}

async fn wait_for_presence_count(
    messenger: &MessengerHandle,
    expected: usize,
) -> Vec<CoreNodePresence> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let presence = CoreNodePresenceMessenger::list_live(messenger, None, TIMEOUT)
            .await
            .expect("list presence");
        if presence.len() == expected {
            return presence;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "presence count did not become {expected}; last value: {presence:?}"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
