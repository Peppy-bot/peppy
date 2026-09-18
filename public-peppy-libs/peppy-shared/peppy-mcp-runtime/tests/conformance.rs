//! MCP `2026-07-28` protocol conformance over real Streamable HTTP.
//!
//! The capability walks live in `loopback.rs`; this suite pins the
//! protocol-shaped contract the design's conformance list names: what
//! `server/discover` advertises, how protocol-version negotiation is
//! bounded, how the tasks extension methods are gated, and how
//! `subscriptions/listen` filters narrow and scope notifications. Every
//! test runs its assertions against each endpoint of the two-exposure set.

mod support;

use rmcp::model::{
    CacheScope, ClientInfo, ErrorCode, GetTaskParams, ProtocolVersion, RequestMetaObject,
    ServerNotification, SubscriptionFilter,
};
use rmcp::service::ClientInitializeError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::json;
use support::{
    FRAME_URI, GUARD, STATUS_URI, confirmation_accept, connect, connect_with_tasks, protocol_error,
    sample_rgb8_frame, start_set, start_task_less_set,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovery_advertises_versions_capabilities_and_caching() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let client = connect(&endpoint.url).await;

        let discovered = client
            .discover(RequestMetaObject(Default::default()))
            .await
            .expect("server/discover answers");

        assert_eq!(
            discovered.supported_versions,
            vec![ProtocolVersion::V_2026_07_28],
            "the runtime implements exactly the revision the exposure was published for"
        );
        assert_eq!(
            discovered.ttl_ms, 3_600_000,
            "discovery is catalog-shaped and carries the catalog TTL"
        );
        assert_eq!(discovered.cache_scope, CacheScope::Private);
        assert_eq!(
            discovered.instructions.as_deref(),
            Some(endpoint.expected.instructions),
            "each endpoint advertises its own instructions"
        );

        let implementation = discovered
            .server_info()
            .expect("the server identity rides in the result _meta");
        assert_eq!(implementation.name, "camera_and_recording");
        assert_eq!(implementation.version, endpoint.expected.tag);
        assert_eq!(
            implementation.title.as_deref(),
            Some(endpoint.expected.title)
        );

        let resources = discovered
            .capabilities
            .resources
            .as_ref()
            .expect("resources capability");
        assert_eq!(resources.subscribe, Some(true));
        assert_eq!(resources.list_changed, Some(true));
        let tools = discovered
            .capabilities
            .tools
            .as_ref()
            .expect("tools capability");
        assert_eq!(tools.list_changed, Some(true));
        assert!(
            discovered.capabilities.supports_tasks(),
            "a task-bearing exposure advertises the tasks extension"
        );

        let negotiated = client.peer_info().expect("discovery ran");
        assert_eq!(negotiated.protocol_version, ProtocolVersion::V_2026_07_28);

        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_initialize_client_is_refused_with_the_supported_versions_listed() {
    let set = start_set().await;
    let http = reqwest::Client::new();
    for endpoint in &set.endpoints {
        // An `initialize` handshake selects the legacy session lifecycle
        // whatever version it names. The endpoint serves the stateless
        // 2026-07-28 lifecycle only (SEP-2567, legacy session mode off), so
        // the handshake is refused outright with the supported versions
        // listed, pointing the client at the discover flow.
        let response = http
            .post(&endpoint.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": ProtocolVersion::LATEST,
                    "capabilities": {},
                    "clientInfo": { "name": "conformance", "version": "0" },
                },
            }))
            .send()
            .await
            .expect("the listener answers");
        // Legacy handshakes keep HTTP 200 and carry the JSON-RPC error as the
        // single event of an SSE stream.
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok())
                .is_some_and(|value| value.starts_with("text/event-stream")),
            "the refusal is streamed"
        );
        let stream = response.text().await.expect("the event stream ends");
        let events: Vec<serde_json::Value> = stream
            .lines()
            .filter_map(|line| line.strip_prefix("data: "))
            .map(|data| serde_json::from_str(data).expect("a JSON-RPC message"))
            .collect();
        let [body] = events.as_slice() else {
            panic!("expected exactly one JSON-RPC message, got {stream}");
        };
        assert_eq!(
            body["error"]["code"],
            json!(ErrorCode::UNSUPPORTED_PROTOCOL_VERSION.0)
        );
        assert_eq!(
            body["error"]["data"]["requested"],
            json!(ProtocolVersion::LATEST)
        );
        assert_eq!(
            body["error"]["data"]["supported"],
            json!([ProtocolVersion::V_2026_07_28])
        );

        // The SDK's initialize lifecycle runs that same handshake and cannot
        // reach a session either.
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint.url.clone()),
        );
        ClientInfo::default()
            .serve_with_lifecycle(transport, ClientLifecycleMode::Initialize)
            .await
            .expect_err("no legacy session exists to initialize");
    }
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_preferring_only_older_versions_cannot_connect() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let transport = StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(endpoint.url.clone()),
        );
        let refused = ClientInfo::default()
            .serve_with_lifecycle(
                transport,
                ClientLifecycleMode::Discover {
                    preferred_versions: vec![ProtocolVersion::V_2025_11_25],
                },
            )
            .await
            .expect_err("no shared protocol version exists");

        let ClientInitializeError::NoCompatibleProtocolVersion {
            server_supported, ..
        } = refused
        else {
            panic!("expected a version mismatch, got {refused:?}");
        };
        assert_eq!(server_supported, vec![ProtocolVersion::V_2026_07_28]);
    }
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn tasks_methods_on_a_task_less_exposure_are_method_not_found() {
    let set = start_task_less_set().await;
    for endpoint in &set.endpoints {
        let client = connect_with_tasks(&endpoint.url).await;

        let info = client.peer_info().expect("discovery ran");
        assert!(
            !info.capabilities.supports_tasks(),
            "an exposure without actions does not advertise the tasks extension"
        );

        // The gate is per method: a server that does not advertise the
        // extension refuses every `tasks/*` request, even from a client that
        // declared the capability.
        let error = protocol_error(
            client
                .get_task(GetTaskParams::new("any"))
                .await
                .expect_err("tasks/get is not served"),
        );
        assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);

        let error = protocol_error(
            client
                .update_task(confirmation_accept("any"))
                .await
                .expect_err("tasks/update is not served"),
        );
        assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);

        let error = protocol_error(
            client
                .cancel_task(rmcp::model::CancelTaskParams::new("any"))
                .await
                .expect_err("tasks/cancel is not served"),
        );
        assert_eq!(error.code, ErrorCode::METHOD_NOT_FOUND);

        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn task_methods_with_an_unknown_id_are_invalid_params() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let client = connect_with_tasks(&endpoint.url).await;

        let error = protocol_error(
            client
                .get_task(GetTaskParams::new("never-created"))
                .await
                .expect_err("the handle does not exist"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        let error = protocol_error(
            client
                .update_task(confirmation_accept("never-created"))
                .await
                .expect_err("the handle does not exist"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        let error = protocol_error(
            client
                .cancel_task(rmcp::model::CancelTaskParams::new("never-created"))
                .await
                .expect_err("the handle does not exist"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn subscription_filters_narrow_to_capabilities_and_scope_notifications() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let client = connect(&endpoint.url).await;

        // The server acknowledges only what its capabilities support: prompts
        // are not served at all, while tool-list changes and resource
        // subscriptions are.
        let mut subscription = client
            .listen(
                SubscriptionFilter::builder()
                    .prompts_list_changed()
                    .tools_list_changed()
                    .resource_subscription(STATUS_URI)
                    .build(),
            )
            .await
            .expect("subscriptions/listen is accepted");
        let acknowledged = subscription.acknowledged();
        assert_eq!(
            acknowledged.prompts_list_changed, None,
            "no prompts capability, so the request is narrowed"
        );
        assert_eq!(acknowledged.tools_list_changed, Some(true));
        assert_eq!(
            acknowledged.resource_subscriptions.as_deref(),
            Some(&[STATUS_URI.to_string()][..])
        );

        // Notifications are scoped to the subscribed resource: a publish on the
        // frame must not notify this stream, so the first notification after
        // both publishes names the status resource.
        let frame_ingest = endpoint
            .server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let token = frame_ingest.admit().expect("gate open");
        frame_ingest
            .publish(token, sample_rgb8_frame())
            .expect("frame publishes");

        let status_ingest = endpoint
            .server
            .ingest("front_camera.status")
            .expect("resource exists");
        let token = status_ingest.admit().expect("gate open");
        status_ingest
            .publish(token, json!({ "battery": 42 }))
            .expect("status publishes");

        let notification = tokio::time::timeout(GUARD, subscription.next())
            .await
            .expect("a notification arrives")
            .expect("the subscription stream is healthy")
            .expect("the stream did not end");
        match notification {
            ServerNotification::ResourceUpdatedNotification(updated) => {
                assert_eq!(
                    updated.params.uri, STATUS_URI,
                    "the frame publish must not leak into a status-only subscription"
                );
                assert_ne!(updated.params.uri, FRAME_URI);
            }
            other => panic!("expected a resource-updated notification, got {other:?}"),
        }
        subscription.cancel().await.expect("subscription cancels");

        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
}
