//! Loopback integration: a real Streamable HTTP endpoint on `127.0.0.1`
//! driven by the real rmcp client under MCP `2026-07-28`.
//!
//! The Peppy side is stubbed: tool handlers are closures and snapshots are
//! fed through the ingest directly, so this exercises the whole protocol
//! surface without a messaging mesh. Freshness runs on a manual clock the
//! test advances; every wait is on a response or a notification. Every walk
//! runs against each endpoint of the two-exposure set.
//!
//! Protocol-shaped details (discovery, negotiation, method gating,
//! notification scoping) are pinned separately in `conformance.rs`, and
//! what one endpoint must not see of another in `isolation.rs`.

mod support;

use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CancelTaskParams, ErrorCode,
    ReadResourceRequestParams, ServerNotification, SubscriptionFilter, TaskStatus, object,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::{
    Client, FRAME_URI, GUARD, STATUS_URI, confirmation_accept, connect, connect_with_tasks,
    poll_task_until, protocol_error, sample_rgb8_frame, start_set,
};

const MS: u64 = 1_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_client_walks_the_catalog_snapshots_and_tools() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let client = connect(&endpoint.url).await;

        // The catalog matches the bundle and carries the caching hints.
        let tools = client.list_tools(None).await.expect("tools/list answers");
        let mut tool_names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
        tool_names.sort_unstable();
        assert_eq!(
            tool_names,
            [
                "front_camera.info",
                "front_camera.set_brightness",
                "recorder.record_episode"
            ]
        );
        assert_eq!(tools.ttl_ms, Some(3_600_000));
        assert_eq!(tools.cache_scope, Some(CacheScope::Private));
        let info_tool = tools
            .tools
            .iter()
            .find(|tool| tool.name.as_ref() == "front_camera.info")
            .expect("info tool is listed");
        assert_eq!(
            info_tool
                .annotations
                .as_ref()
                .and_then(|a| a.read_only_hint),
            Some(true)
        );
        assert!(info_tool.output_schema.is_some());

        let resources = client
            .list_resources(None)
            .await
            .expect("resources/list answers");
        let mut resource_uris: Vec<_> = resources
            .resources
            .iter()
            .map(|resource| resource.uri.as_str())
            .collect();
        resource_uris.sort_unstable();
        assert_eq!(resource_uris, [FRAME_URI, STATUS_URI]);
        assert_eq!(resources.ttl_ms, Some(3_600_000));
        assert_eq!(resources.cache_scope, Some(CacheScope::Private));

        // Before any publish, reads report the resource unavailable. The refusal
        // is deliberately an internal error: an empty snapshot store is a
        // server-side condition, not a client mistake.
        let error = protocol_error(
            client
                .read_resource(ReadResourceRequestParams::new(STATUS_URI))
                .await
                .expect_err("nothing published yet"),
        );
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(
            error.message.contains("unavailable"),
            "got {}",
            error.message
        );

        // A resource outside the exposure is refused. Under 2026-07-28 the SDK
        // remaps resource-not-found onto invalid-params (SEP-2164).
        let error = protocol_error(
            client
                .read_resource(ReadResourceRequestParams::new("peppy://resource/absent"))
                .await
                .expect_err("absent resources are refused"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        // Subscribe, then publish: the notification names the updated resource.
        let mut subscription = client
            .listen(
                SubscriptionFilter::builder()
                    .resource_subscription(STATUS_URI)
                    .build(),
            )
            .await
            .expect("subscriptions/listen is accepted");

        let ingest = endpoint
            .server
            .ingest("front_camera.status")
            .expect("resource exists");
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");

        let notification = tokio::time::timeout(GUARD, subscription.next())
            .await
            .expect("a notification arrives")
            .expect("the subscription stream is healthy")
            .expect("the stream did not end");
        match notification {
            ServerNotification::ResourceUpdatedNotification(updated) => {
                assert_eq!(updated.params.uri, STATUS_URI);
            }
            other => panic!("expected a resource-updated notification, got {other:?}"),
        }
        subscription.cancel().await.expect("subscription cancels");

        // The published snapshot serves with the remaining freshness as ttlMs.
        let read = client
            .read_resource(ReadResourceRequestParams::new(STATUS_URI))
            .await
            .expect("fresh snapshot serves");
        assert_eq!(read.ttl_ms, Some(2000));
        assert_eq!(read.cache_scope, Some(CacheScope::Private));
        let contents = read.contents.first().expect("one content item");
        let text = match contents {
            rmcp::model::ResourceContents::TextResourceContents {
                text,
                mime_type,
                uri,
                ..
            } => {
                assert_eq!(uri, STATUS_URI);
                assert_eq!(mime_type.as_deref(), Some("application/json"));
                text
            }
            other => panic!("expected text contents, got {other:?}"),
        };
        assert_eq!(text, "{\"battery\":87}");

        // An rgb8 frame published through the ingest serves as JPEG, with the
        // same remaining-freshness hint as any other snapshot.
        let frame_ingest = endpoint
            .server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let token = frame_ingest.admit().expect("gate open");
        frame_ingest
            .publish(token, sample_rgb8_frame())
            .expect("frame publishes");
        let read = client
            .read_resource(ReadResourceRequestParams::new(FRAME_URI))
            .await
            .expect("frame snapshot serves");
        assert_eq!(read.ttl_ms, Some(2000));
        assert_eq!(read.cache_scope, Some(CacheScope::Private));
        let rmcp::model::ResourceContents::TextResourceContents { text, .. } =
            read.contents.first().expect("one content item")
        else {
            panic!("expected text contents");
        };
        let snapshot: Value = serde_json::from_str(text).expect("snapshot is JSON");
        assert_eq!(snapshot["encoding"], "mjpeg");
        assert_eq!(snapshot["width"], 8);
        let jpeg = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            snapshot["frame"].as_str().expect("frame is base64"),
        )
        .expect("frame decodes");
        assert_eq!(&jpeg[..2], &[0xFF, 0xD8], "JPEG magic bytes");

        // Tool calls round-trip structured output against the derived schemas.
        let called = client
            .call_tool(CallToolRequestParams::new("front_camera.info"))
            .await
            .expect("read-only tool answers");
        assert_ne!(called.is_error, Some(true));
        assert_eq!(
            called.structured_content,
            Some(endpoint.expected.info.clone()),
            "the tool resolves to this endpoint's own bridge"
        );

        let called = client
            .call_tool(
                CallToolRequestParams::new("front_camera.set_brightness")
                    .with_arguments(object(json!({ "value": 12 }))),
            )
            .await
            .expect("mutating tool answers");
        assert_eq!(called.structured_content, Some(json!({ "applied": true })));

        // A value outside the reflected restrict bounds is rejected before the
        // bridge runs.
        let error = protocol_error(
            client
                .call_tool(
                    CallToolRequestParams::new("front_camera.set_brightness")
                        .with_arguments(object(json!({ "value": 65 }))),
                )
                .await
                .expect_err("65 is outside the restrict bounds"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        // A name absent from the exposure is rejected.
        let error = protocol_error(
            client
                .call_tool(CallToolRequestParams::new("front_camera.set_gain"))
                .await
                .expect_err("set_gain is not exposed"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

        // A bridge failure comes back as a readable tool error, not a protocol
        // error.
        let called = client
            .call_tool(
                CallToolRequestParams::new("front_camera.set_brightness")
                    .with_arguments(object(json!({ "value": 13 }))),
            )
            .await
            .expect("bridge failures are tool errors");
        assert_eq!(called.is_error, Some(true));

        client.cancel().await.expect("client disconnects");

        // Once the injected clock passes max_age_ms, a fresh connection reads
        // the snapshot as stale. Staleness is the same deliberate internal-error
        // refusal as unavailability.
        endpoint.nanos.store(2_500 * MS, Ordering::SeqCst);
        let late_client = connect(&endpoint.url).await;
        let error = protocol_error(
            late_client
                .read_resource(ReadResourceRequestParams::new(STATUS_URI))
                .await
                .expect_err("2500 ms old is stale"),
        );
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("stale"), "got {}", error.message);
        late_client.cancel().await.expect("client disconnects");
    }

    set.stop().await;
}

async fn start_record_episode(client: &Client, episode_name: &str) -> String {
    let response = client
        .call_tool_once(
            CallToolRequestParams::new("recorder.record_episode")
                .with_arguments(object(json!({ "episode_name": episode_name }))),
        )
        .await
        .expect("the task-backed tool answers");
    let CallToolResponse::Task(created) = response else {
        panic!("expected a task handle, got {response:?}");
    };
    assert_eq!(created.task.status, TaskStatus::Working);
    assert_eq!(
        created.task.ttl_ms,
        Some(601000),
        "the advertised TTL clears the 600000 ms whole-goal deadline, so the runtime's own \
         deadline error wins over the SDK's TTL sweep"
    );
    created.task.task_id
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_client_drives_action_backed_tasks() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        // A client that does not declare the tasks capability never receives a
        // task handle: the call is refused with the required capability.
        let plain_client = connect(&endpoint.url).await;
        let error = protocol_error(
            plain_client
                .call_tool_once(
                    CallToolRequestParams::new("recorder.record_episode")
                        .with_arguments(object(json!({ "episode_name": "demo" }))),
                )
                .await
                .expect_err("the tasks capability is required"),
        );
        assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
        plain_client.cancel().await.expect("client disconnects");

        let client = connect_with_tasks(&endpoint.url).await;
        let info = client.peer_info().expect("discovery ran");
        assert!(
            info.capabilities.supports_tasks(),
            "the exposure advertises the tasks extension"
        );

        // Confirmation walk: the task parks in input_required with the
        // confirmation elicitation; accepting through tasks/update releases the
        // goal, feedback drives the status message, and the Peppy result
        // completes the task with structured output.
        let task_id = start_record_episode(&client, "demo").await;
        let parked = poll_task_until(&client, &task_id, "input_required", |task| {
            task.status() == TaskStatus::InputRequired
        })
        .await;
        let rmcp::model::TaskPayload::InputRequired { input_requests } = parked.payload else {
            panic!("expected input_required, got {:?}", parked.payload);
        };
        assert!(input_requests.contains_key("confirmation"));

        client
            .update_task(confirmation_accept(&task_id))
            .await
            .expect("the confirmation is delivered");
        let completed = poll_task_until(&client, &task_id, "a terminal status", |task| {
            task.status().is_terminal()
        })
        .await;
        assert_eq!(completed.status(), TaskStatus::Completed);
        assert_eq!(
            completed.task.status_message.as_deref(),
            Some("recording `demo`"),
            "feedback reports as the status message"
        );
        let rmcp::model::TaskPayload::Completed { result } = completed.payload else {
            panic!("expected a completed payload");
        };
        assert_eq!(result["structuredContent"], json!({ "frames": 120 }));

        // Cancellation walk: tasks/cancel is forwarded cooperatively and the
        // Peppy cancelled result settles the task as cancelled.
        let task_id = start_record_episode(&client, "wait_for_cancel").await;
        poll_task_until(&client, &task_id, "input_required", |task| {
            task.status() == TaskStatus::InputRequired
        })
        .await;
        client
            .update_task(confirmation_accept(&task_id))
            .await
            .expect("the confirmation is delivered");
        poll_task_until(&client, &task_id, "the goal running", |task| {
            task.task.status_message.as_deref() == Some("recording `wait_for_cancel`")
        })
        .await;
        client
            .cancel_task(CancelTaskParams::new(&*task_id))
            .await
            .expect("tasks/cancel acknowledges");
        let cancelled = poll_task_until(&client, &task_id, "a terminal status", |task| {
            task.status().is_terminal()
        })
        .await;
        assert_eq!(cancelled.status(), TaskStatus::Cancelled);

        // Reconnect walk: task handles outlive sessions, so a client that
        // disconnects mid-goal can reconnect and keep driving the same handle.
        let task_id = start_record_episode(&client, "wait_for_cancel").await;
        poll_task_until(&client, &task_id, "input_required", |task| {
            task.status() == TaskStatus::InputRequired
        })
        .await;
        client.cancel().await.expect("client disconnects mid-task");

        let reconnected = connect_with_tasks(&endpoint.url).await;
        reconnected
            .update_task(confirmation_accept(&task_id))
            .await
            .expect("the reconnected client confirms the same handle");
        poll_task_until(&reconnected, &task_id, "the goal running", |task| {
            task.task.status_message.as_deref() == Some("recording `wait_for_cancel`")
        })
        .await;
        reconnected
            .cancel_task(CancelTaskParams::new(&*task_id))
            .await
            .expect("tasks/cancel acknowledges");
        let cancelled = poll_task_until(&reconnected, &task_id, "a terminal status", |task| {
            task.status().is_terminal()
        })
        .await;
        assert_eq!(cancelled.status(), TaskStatus::Cancelled);
        reconnected.cancel().await.expect("client disconnects");
    }

    set.stop().await;
}
