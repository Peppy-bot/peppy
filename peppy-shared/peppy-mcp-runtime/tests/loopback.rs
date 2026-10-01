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
    ReadResourceRequestParams, ResourceContents, ServerNotification, SubscriptionFilter,
    TaskStatus, object,
};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use support::{
    Client, FRAME_URI, GUARD, STATUS_URI, Streaming, assert_ended_cancelled, confirmation_accept,
    connect, connect_logging_progress, connect_named, connect_with_tasks, fixture_bundle,
    fixture_exposures, fixture_server, poll_task_until, protocol_error, sample_rgb8_frame,
    serve_set, start_set,
};

const MS: u64 = 1_000_000;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_real_client_walks_the_catalog_snapshots_and_tools() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let client = connect_named(&endpoint.url, "loopback-test", "1.0").await;

        // The catalog matches the bundle and carries the caching hints.
        let tools = client.list_tools(None).await.expect("tools/list answers");
        let mut tool_names: Vec<_> = tools.tools.iter().map(|tool| tool.name.as_ref()).collect();
        tool_names.sort_unstable();
        assert_eq!(
            tool_names,
            [
                "camera.recent_calls",
                "front_camera.info",
                "front_camera.look",
                "front_camera.set_brightness",
                "recorder.record_episode",
                "recorder.replay_episode"
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
        let look_tool = tools
            .tools
            .iter()
            .find(|tool| tool.name.as_ref() == "front_camera.look")
            .expect("the picture tool is listed");
        let look_annotations = look_tool.annotations.as_ref().expect("annotations set");
        assert_eq!(look_annotations.read_only_hint, Some(true));
        assert_eq!(look_annotations.destructive_hint, Some(false));
        assert!(look_tool.output_schema.is_some());

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
        // A resource is listed under the MIME type of its blob, and one
        // without a representation under its document's.
        let listed_mime_type = |uri: &str| {
            resources
                .resources
                .iter()
                .find(|resource| resource.uri == uri)
                .and_then(|resource| resource.mime_type.as_deref())
        };
        assert_eq!(listed_mime_type(FRAME_URI), Some("image/jpeg"));
        assert_eq!(listed_mime_type(STATUS_URI), Some("application/json"));
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

        // The status streams on: a read answers with the message that
        // arrives after it, with the remaining freshness as ttlMs.
        let status = Streaming::of(ingest, json!({ "battery": 87 }));
        let read = client
            .read_resource(ReadResourceRequestParams::new(STATUS_URI))
            .await
            .expect("fresh snapshot serves");
        assert_eq!(read.ttl_ms, Some(2000));
        assert_eq!(read.cache_scope, Some(CacheScope::Private));
        let [document] = read.contents.as_slice() else {
            panic!(
                "a resource without a representation serves its document alone, got {:?}",
                read.contents
            );
        };
        assert_eq!(
            json_document(document, STATUS_URI),
            json!({ "battery": 87 })
        );

        // The picture tool answers inside the call from the same snapshot
        // store: before the first frame it is a tool error in the words of
        // the read.
        let looked = client
            .call_tool(CallToolRequestParams::new("front_camera.look"))
            .await
            .expect("the picture tool answers");
        assert_eq!(looked.is_error, Some(true));
        let unavailable = protocol_error(
            client
                .read_resource(ReadResourceRequestParams::new(FRAME_URI))
                .await
                .expect_err("no frame published yet"),
        );
        assert_eq!(
            looked.content[0].as_text().map(|text| text.text.as_str()),
            Some(unavailable.message.as_ref())
        );

        // An rgb8 frame streamed through the ingest serves as a document
        // without the frame and the frame as a JPEG blob, with the same
        // remaining-freshness hint as any other snapshot.
        let frame_ingest = endpoint
            .server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let frames = Streaming::of(frame_ingest, sample_rgb8_frame());
        let read = client
            .read_resource(ReadResourceRequestParams::new(FRAME_URI))
            .await
            .expect("frame snapshot serves");
        assert_eq!(read.ttl_ms, Some(2000));
        assert_eq!(read.cache_scope, Some(CacheScope::Private));
        let [document, blob] = read.contents.as_slice() else {
            panic!(
                "a resource with a representation serves its document, then its blob, got {:?}",
                read.contents
            );
        };
        let snapshot = json_document(document, FRAME_URI);
        assert_eq!(
            snapshot,
            json!({ "encoding": "mjpeg", "width": 8, "height": 8 })
        );
        let ResourceContents::BlobResourceContents {
            blob,
            mime_type,
            uri,
            ..
        } = blob
        else {
            panic!("expected blob contents, got {blob:?}");
        };
        assert_eq!(uri, FRAME_URI);
        assert_eq!(mime_type.as_deref(), Some("image/jpeg"));
        assert_is_jpeg(blob);

        // The picture tool answers with that snapshot: the frame as an image
        // block, the document as text and as the structured content.
        let looked = client
            .call_tool(CallToolRequestParams::new("front_camera.look"))
            .await
            .expect("the picture tool answers");
        assert_eq!(looked.is_error, Some(false));
        assert_eq!(looked.structured_content, Some(snapshot.clone()));
        let [image, text] = looked.content.as_slice() else {
            panic!("expected an image and a text, got {:?}", looked.content);
        };
        let image = image.as_image().expect("the first block is the image");
        assert_eq!(image.mime_type, "image/jpeg");
        assert_eq!(&image.data, blob, "the image is the blob a read serves");
        let text = text.as_text().expect("the second block is the document");
        assert_eq!(
            serde_json::from_str::<Value>(&text.text).expect("the text is JSON"),
            snapshot
        );

        // It takes no argument on a fixed surface.
        let error = protocol_error(
            client
                .call_tool(
                    CallToolRequestParams::new("front_camera.look")
                        .with_arguments(object(json!({ "camera": "front" }))),
                )
                .await
                .expect_err("the picture tool takes no argument"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);

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

        // The record lists the three state-changing calls, newest first,
        // under the identity the client declared; the read-only calls, the
        // picture tool and the reads left no entry.
        let recorded = client
            .call_tool(CallToolRequestParams::new("camera.recent_calls"))
            .await
            .expect("the record answers");
        assert_eq!(recorded.is_error, Some(false));
        let calls = recorded.structured_content.expect("structured")["calls"]
            .as_array()
            .cloned()
            .expect("the calls");
        let summary: Vec<(&str, &str, Value)> = calls
            .iter()
            .map(|call| {
                (
                    call["tool"].as_str().expect("tool"),
                    call["outcome"].as_str().expect("outcome"),
                    call["arguments"]["value"].clone(),
                )
            })
            .collect();
        assert_eq!(
            summary,
            [
                ("front_camera.set_brightness", "failed", json!(13)),
                ("front_camera.set_brightness", "refused", json!(65)),
                ("front_camera.set_brightness", "completed", json!(12)),
            ]
        );
        for call in &calls {
            assert_eq!(
                call["client"],
                json!({ "name": "loopback-test", "version": "1.0" })
            );
        }
        assert_eq!(calls[0]["message"], "call failed: 13 is reserved");

        status.stop().await;
        frames.stop().await;
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

/// The JSON document a read serves first, held to `uri` and to its MIME
/// type.
fn json_document(contents: &ResourceContents, uri: &str) -> Value {
    let ResourceContents::TextResourceContents {
        text,
        mime_type,
        uri: served_uri,
        ..
    } = contents
    else {
        panic!("expected text contents, got {contents:?}");
    };
    assert_eq!(served_uri, uri);
    assert_eq!(mime_type.as_deref(), Some("application/json"));
    serde_json::from_str(text).expect("the document is JSON")
}

fn assert_is_jpeg(base64_text: &str) {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, base64_text)
        .expect("the frame is base64");
    assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG magic bytes");
}

/// A picture tool reads a snapshot and sends no goal, so it answers inside
/// the call for every client, one that declares the tasks extension
/// included.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_picture_tool_is_never_a_task() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        let frame_ingest = endpoint
            .server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let frames = Streaming::of(frame_ingest, sample_rgb8_frame());

        let client = connect_with_tasks(&endpoint.url).await;
        let response = client
            .call_tool_once(CallToolRequestParams::new("front_camera.look"))
            .await
            .expect("the picture tool answers");
        let CallToolResponse::Complete(looked) = response else {
            panic!("expected the picture inside the call, got {response:?}");
        };
        assert_eq!(looked.is_error, Some(false));
        let image = looked.content[0].as_image().expect("the image block");
        assert_is_jpeg(&image.data);
        frames.stop().await;
        client.cancel().await.expect("client disconnects");
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
        // task handle, and the confirmation gate is a task's to carry: the
        // call is refused, naming the tool and the extension in the message
        // and the capability in the data.
        let plain_client = connect(&endpoint.url).await;
        let error = protocol_error(
            plain_client
                .call_tool_once(
                    CallToolRequestParams::new("recorder.record_episode")
                        .with_arguments(object(json!({ "episode_name": "demo" }))),
                )
                .await
                .expect_err("the confirmation needs a task"),
        );
        assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
        assert!(
            error.message.contains("`recorder.record_episode`")
                && error.message.contains("io.modelcontextprotocol/tasks"),
            "got {}",
            error.message
        );
        assert_eq!(
            error.data,
            Some(json!({
                "requiredCapabilities": { "extensions": { "io.modelcontextprotocol/tasks": {} } }
            }))
        );
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

        // Cancellation walk: tasks/cancel is forwarded cooperatively, and
        // the goal that ends cancelled ends the task `cancelled`, its status
        // message carrying the provider's result.
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
        assert_ended_cancelled(&cancelled);

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
        assert_ended_cancelled(&cancelled);
        reconnected.cancel().await.expect("client disconnects");
    }

    set.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_without_the_tasks_capability_runs_an_action_inside_the_call() {
    let set = start_set().await;
    for endpoint in &set.endpoints {
        // Completion walk: the call answers with the goal's structured
        // result, and the feedback reported on the way arrived first as a
        // progress notification under the call's own token.
        let (client, mut progress) = connect_logging_progress(&endpoint.url).await;
        let result = client
            .call_tool(
                CallToolRequestParams::new("recorder.replay_episode")
                    .with_arguments(object(json!({ "episode_name": "demo" }))),
            )
            .await
            .expect("the action answers inside the call");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(json!({ "frames": 120 })));
        let notification = progress
            .try_recv()
            .expect("the feedback was relayed before the result");
        assert_eq!(notification.message.as_deref(), Some("replaying `demo`"));
        assert_eq!(notification.progress, 1.0);
        assert_eq!(notification.total, None);
        assert!(
            progress.try_recv().is_err(),
            "one feedback message, one notification"
        );

        // Refusals keep their shape on this surface: invalid goal fields are
        // a protocol error before anything runs.
        let error = protocol_error(
            client
                .call_tool_once(
                    CallToolRequestParams::new("recorder.replay_episode")
                        .with_arguments(object(json!({ "episode_name": 7 }))),
                )
                .await
                .expect_err("the goal fields fail the derived schema"),
        );
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        client.cancel().await.expect("client disconnects");
    }
    set.stop().await;
}

/// Reports through `signal` when the call's goal sees the cancel request,
/// parking until then.
async fn signal_on_cancel(
    signal: tokio::sync::mpsc::UnboundedSender<()>,
    context: peppy_mcp_runtime::ActionContext,
) -> Result<Value, peppy_mcp_runtime::ActionExit> {
    context.report_feedback("parked");
    context.cancel_requested().await;
    signal.send(()).expect("the test holds the receiver");
    Err(peppy_mcp_runtime::ActionExit::Cancelled(
        peppy_mcp_runtime::CancelledGoal {
            result: support::cancelled_episode(),
            reason: None,
        },
    ))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_call_cancels_the_goal_running_inside_it() {
    let (signal, mut cancelled) = tokio::sync::mpsc::unbounded_channel();
    let mut servers = Vec::new();
    for expected in fixture_exposures() {
        let signal = signal.clone();
        let (builder, nanos) = fixture_server(&expected, fixture_bundle(&expected));
        let server = builder
            .with_task("recorder.record_episode", support::record_episode)
            .with_task(
                "recorder.replay_episode",
                move |_call: peppy_mcp_runtime::ToolCall,
                      context: peppy_mcp_runtime::ActionContext| {
                    signal_on_cancel(signal.clone(), context)
                },
            )
            .build()
            .expect("bundle and handlers agree");
        servers.push((expected, server, nanos));
    }
    let set = serve_set(servers).await;
    let http = reqwest::Client::new();
    for endpoint in &set.endpoints {
        // A raw call, so the test owns the response stream: once the first
        // event (the goal's feedback) proves the goal is parked, dropping
        // the response is the client going away mid-call.
        let response = http
            .post(&endpoint.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", "recorder.replay_episode")
            .json(&json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "recorder.replay_episode",
                    "arguments": { "episode_name": "wait_for_cancel" },
                    "_meta": {
                        "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                        "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
                        "io.modelcontextprotocol/clientCapabilities": {},
                        "progressToken": "call-1"
                    }
                }
            }))
            .send()
            .await
            .expect("the call opens");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let mut response = response;
        let first = tokio::time::timeout(GUARD, response.chunk())
            .await
            .expect("the first event arrives")
            .expect("the stream is readable")
            .expect("the stream carries the feedback event");
        let first = String::from_utf8(first.to_vec()).expect("SSE is text");
        assert!(
            first.contains("notifications/progress") && first.contains("parked"),
            "the goal's feedback is the call's first event: {first}"
        );
        assert!(
            cancelled.try_recv().is_err(),
            "the goal is parked while the call is open"
        );

        drop(response);
        tokio::time::timeout(GUARD, cancelled.recv())
            .await
            .expect("closing the call reaches the goal as a cancel request")
            .expect("the signal sender lives in the server");
    }
    set.stop().await;
}
