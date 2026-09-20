//! What one endpoint of a set must never see of another, and what the
//! listener must refuse besides its endpoints.
//!
//! The fixture set serves two tags of one exposure with identical public
//! names, which is the sharpest case: every name, URI, subscription, and
//! task handle exists on both endpoints and must still resolve to only one.

mod support;

use peppy_mcp_runtime::ActionExit;
use rmcp::model::{
    CallToolRequestParams, CallToolResponse, CancelTaskParams, ErrorCode, GetTaskParams,
    ReadResourceRequestParams, ServerNotification, SubscriptionFilter, TaskStatus, object,
};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use support::{
    FRAME_URI, GUARD, STATUS_URI, connect, connect_with_tasks, fixture_bundle, fixture_exposures,
    fixture_server, poll_task_until, protocol_error, sample_rgb8_frame, serve_set, start_set,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_public_names_resolve_to_their_own_endpoint() {
    let set = start_set().await;
    let [first, second] = &set.endpoints[..] else {
        panic!("the fixture set has two endpoints");
    };
    assert_ne!(first.url, second.url);
    assert_eq!(first.path(), "/camera_and_recording/v1/mcp");
    assert_eq!(second.path(), "/camera_and_recording/v2/mcp");

    let first_client = connect(&first.url).await;
    let second_client = connect(&second.url).await;

    // Both catalogs list the same names; each name calls its own bridge.
    for (endpoint, client) in [(first, &first_client), (second, &second_client)] {
        let tools = client.list_tools(None).await.expect("tools/list answers");
        let mut names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
        names.sort_unstable();
        assert_eq!(
            names,
            [
                "front_camera.info",
                "front_camera.set_brightness",
                "recorder.record_episode"
            ]
        );
        let called = client
            .call_tool(CallToolRequestParams::new("front_camera.info"))
            .await
            .expect("the tool answers");
        assert_eq!(
            called.structured_content,
            Some(endpoint.expected.info.clone()),
            "`front_camera.info` on {} is that endpoint's bridge",
            endpoint.path()
        );
    }

    // A snapshot published on one endpoint is unknown to the other, under
    // the very same URI.
    let ingest = first
        .server
        .ingest("front_camera.status")
        .expect("resource exists");
    let token = ingest.admit().expect("gate open");
    ingest
        .publish(token, json!({ "battery": 87 }))
        .expect("publishes");

    let read = first_client
        .read_resource(ReadResourceRequestParams::new(STATUS_URI))
        .await
        .expect("the publishing endpoint serves its snapshot");
    let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
        read.contents.first().expect("one content item")
    else {
        panic!("expected text contents");
    };
    assert_eq!(text, "{\"battery\":87}");

    let error = protocol_error(
        second_client
            .read_resource(ReadResourceRequestParams::new(STATUS_URI))
            .await
            .expect_err("nothing was published on the other endpoint"),
    );
    assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
    assert!(
        error.message.contains("unavailable"),
        "got {}",
        error.message
    );

    first_client.cancel().await.expect("client disconnects");
    second_client.cancel().await.expect("client disconnects");
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_subscription_never_sees_another_endpoints_publishes() {
    let set = start_set().await;
    let [first, second] = &set.endpoints[..] else {
        panic!("the fixture set has two endpoints");
    };
    let client = connect(&first.url).await;

    // Subscribe on the first endpoint to both of its resources, then publish
    // the status on the second endpoint and the frame on the first. The
    // first notification on this stream must name the frame: were the other
    // endpoint's status publish to leak, it would arrive first under the
    // status URI, which the subscription would accept.
    let mut subscription = client
        .listen(
            SubscriptionFilter::builder()
                .resource_subscription(STATUS_URI)
                .resource_subscription(FRAME_URI)
                .build(),
        )
        .await
        .expect("subscriptions/listen is accepted");

    let other_status = second
        .server
        .ingest("front_camera.status")
        .expect("resource exists");
    let token = other_status.admit().expect("gate open");
    other_status
        .publish(token, json!({ "battery": 1 }))
        .expect("publishes on the other endpoint");

    let own_frame = first
        .server
        .ingest("front_camera.latest_frame")
        .expect("resource exists");
    let token = own_frame.admit().expect("gate open");
    own_frame
        .publish(token, sample_rgb8_frame())
        .expect("publishes on the subscribed endpoint");

    let notification = tokio::time::timeout(GUARD, subscription.next())
        .await
        .expect("a notification arrives")
        .expect("the subscription stream is healthy")
        .expect("the stream did not end");
    match notification {
        ServerNotification::ResourceUpdatedNotification(updated) => {
            assert_eq!(
                updated.params.uri, FRAME_URI,
                "the other endpoint's status publish must not reach this subscription"
            );
        }
        other => panic!("expected a resource-updated notification, got {other:?}"),
    }
    subscription.cancel().await.expect("subscription cancels");

    client.cancel().await.expect("client disconnects");
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_task_handle_is_unknown_to_the_other_endpoint() {
    let set = start_set().await;
    let [first, second] = &set.endpoints[..] else {
        panic!("the fixture set has two endpoints");
    };
    let first_client = connect_with_tasks(&first.url).await;
    let second_client = connect_with_tasks(&second.url).await;

    let response = first_client
        .call_tool_once(
            CallToolRequestParams::new("recorder.record_episode")
                .with_arguments(object(json!({ "episode_name": "demo" }))),
        )
        .await
        .expect("the task-backed tool answers");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    let task_id = created.task.task_id;

    // The handle polls on the endpoint that created it and nowhere else,
    // for every task method.
    first_client
        .get_task(GetTaskParams::new(&*task_id))
        .await
        .expect("the creating endpoint knows the handle");
    let error = protocol_error(
        second_client
            .get_task(GetTaskParams::new(&*task_id))
            .await
            .expect_err("the other endpoint does not know the handle"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    let error = protocol_error(
        second_client
            .update_task(support::confirmation_accept(&task_id))
            .await
            .expect_err("the other endpoint cannot confirm it"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    let error = protocol_error(
        second_client
            .cancel_task(CancelTaskParams::new(&*task_id))
            .await
            .expect_err("the other endpoint cannot cancel it"),
    );
    assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

    // The failed attempts changed nothing: the task is still parked on its
    // own endpoint and settles there.
    first_client
        .cancel_task(CancelTaskParams::new(&*task_id))
        .await
        .expect("the creating endpoint cancels it");
    let settled = poll_task_until(&first_client, &task_id, "a terminal status", |task| {
        task.status().is_terminal()
    })
    .await;
    assert_eq!(settled.status(), TaskStatus::Cancelled);

    first_client.cancel().await.expect("client disconnects");
    second_client.cancel().await.expect("client disconnects");
    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_the_exposures_endpoints_are_served() {
    let set = start_set().await;
    let http = reqwest::Client::new();

    // Anything but an endpoint path is refused, a bare `/mcp` and the
    // exposure paths without their `/mcp` suffix included, with no hint of
    // what is served.
    for path in [
        "/",
        "/mcp",
        "/camera_and_recording",
        "/camera_and_recording/v1",
        "/camera_and_recording/v3/mcp",
        "/other_exposure/v1/mcp",
        "/camera_and_recording/v1/mcp/extra",
    ] {
        for method in [reqwest::Method::GET, reqwest::Method::POST] {
            let response = http
                .request(method.clone(), set.url(path))
                .header("content-type", "application/json")
                .body("{}")
                .send()
                .await
                .expect("the listener answers");
            assert_eq!(
                response.status(),
                reqwest::StatusCode::NOT_FOUND,
                "{method} {path} must not be served"
            );
        }
    }

    // The endpoints themselves answer the protocol.
    for endpoint in &set.endpoints {
        let client = connect(&endpoint.url).await;
        let info = client.peer_info().expect("discovery ran");
        let implementation = info
            .server_info
            .as_ref()
            .expect("the server identity is advertised");
        assert_eq!(
            implementation.version,
            endpoint.expected.tag,
            "{} serves its own exposure",
            endpoint.path()
        );
        client.cancel().await.expect("client disconnects");
    }

    set.stop().await;
}

/// Sets its flag when dropped, which is how a parked goal reports that the
/// runtime aborted it.
struct DropFlag(Arc<AtomicBool>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stopping_the_set_aborts_running_tasks_on_every_endpoint() {
    let mut servers = Vec::new();
    let mut aborted = Vec::new();
    for expected in fixture_exposures() {
        let flag = Arc::new(AtomicBool::new(false));
        aborted.push(Arc::clone(&flag));
        let (builder, nanos) = fixture_server(&expected, fixture_bundle(&expected));
        let server = builder
            .with_task(
                "recorder.record_episode",
                move |_input: Value, _context: peppy_mcp_runtime::ActionContext| {
                    let guard = DropFlag(Arc::clone(&flag));
                    async move {
                        let _guard = guard;
                        std::future::pending::<Result<Value, ActionExit>>().await
                    }
                },
            )
            .build()
            .expect("bundle and handlers agree");
        servers.push((expected, server, nanos));
    }
    let set = serve_set(servers).await;

    // Start a goal on each endpoint and confirm it, so both bridges are
    // parked in their (never-ending) operation.
    let mut clients = Vec::new();
    for endpoint in &set.endpoints {
        let client = connect_with_tasks(&endpoint.url).await;
        let response = client
            .call_tool_once(
                CallToolRequestParams::new("recorder.record_episode")
                    .with_arguments(object(json!({ "episode_name": "forever" }))),
            )
            .await
            .expect("the task-backed tool answers");
        let CallToolResponse::Task(created) = response else {
            panic!("expected a task handle, got {response:?}");
        };
        let task_id = created.task.task_id;
        poll_task_until(&client, &task_id, "parked for confirmation", |task| {
            task.status() == TaskStatus::InputRequired
        })
        .await;
        client
            .update_task(support::confirmation_accept(&task_id))
            .await
            .expect("the confirmation is delivered");
        poll_task_until(&client, &task_id, "running the goal", |task| {
            task.status() == TaskStatus::Working
        })
        .await;
        clients.push(client);
    }
    for flag in &aborted {
        assert!(!flag.load(Ordering::SeqCst), "the goals are still running");
    }

    for client in clients {
        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
    for (index, flag) in aborted.iter().enumerate() {
        assert!(
            flag.load(Ordering::SeqCst),
            "stopping the set aborts the goal parked on endpoint {index}"
        );
    }
}
