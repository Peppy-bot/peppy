//! The bridge behind a real endpoint: an exposure document validated
//! against its contract, laid out by [`prepare`], served by the runtime
//! over Streamable HTTP on `127.0.0.1`, and called with raw HTTP requests,
//! so the test reads the response stream of the call itself. The provider
//! is the mock action server of the bridge tests, and it holds each goal
//! until the test ends it.

use super::{
    LINK_ID, MEMBER, Mesh, accepted_goal, bridge_identity, codec, encoded, mesh, publish_percent,
};
use crate::bridges::{PreparedExposure, drive_goal, prepare};
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::ManifestFingerprint;
use message_codec::MessageCodec;
use peppy_mcp_catalog::{DeclaredMembers, ResolvedContract, build_exposure_bundle};
use peppy_mcp_runtime::{ActionContext, ExposureServer, ExposureSet, ToolCall};
use peppylib::testing::READINESS_TIMEOUT;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// The contract behind the endpoint: the gripper action of the bridge
/// tests, with a result that says what happened.
const CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "limb_motion", tag: "v1" },
    interfaces: {
        actions: [
            {
                name: "move_gripper",
                goal_service: { request_message_format: { width: "f64" } },
                feedback_topic: {
                    qos_profile: "reliable",
                    message_format: { percent: "u8" },
                },
                result_service: {
                    response_message_format: { success: "bool", message: "string" },
                },
            },
        ],
    },
}"#;

/// The exposure: the gripper action as a tool bounded by its progress. The
/// window is far longer than any step of the tests, which never wait on it.
const EXPOSURE: &str = r#"{
    peppy_schema: "mcp_exposure/v1",
    manifest: { name: "gripper", tag: "v1" },
    server: { title: "Gripper" },
    targets: {
        limb_motion: {
            contract: { name: "limb_motion", tag: "v1" },
            actions: [
                {
                    member: "move_gripper",
                    tool: "limb_motion.move_gripper",
                    description: "Move the gripper to a width.",
                    operation: "long_running",
                    progress_timeout_ms: 600000,
                },
            ],
        },
    },
}"#;

const TOOL: &str = "limb_motion.move_gripper";

/// The progress token every call of these tests carries.
const PROGRESS_TOKEN: &str = "call-1";

/// The exposure as `serve` prepares it: validated against the contract,
/// then laid out into its bridges.
fn prepared_exposure() -> PreparedExposure {
    let contract = PeppyContractParser::from_content(CONTRACT).expect("the contract parses");
    let fingerprint = ManifestFingerprint::of_bytes(CONTRACT.as_bytes());
    let resolved = ResolvedContract {
        name: "limb_motion",
        tag: "v1",
        sha256: &fingerprint,
        members: DeclaredMembers {
            topics: &contract.interfaces.topics,
            services: &contract.interfaces.services,
            actions: &contract.interfaces.actions,
        },
    };
    let exposure = PeppyMcpExposureParser::from_content(EXPOSURE).expect("the exposure parses");
    let validated =
        build_exposure_bundle(&exposure, &[resolved], &[]).expect("the exposure validates");
    let mut prepared = prepare(vec![validated]).expect("the exposure lays out");
    assert_eq!(prepared.len(), 1, "one exposure, one prepared exposure");
    prepared.remove(0)
}

/// The codec of the result the provider ends a goal with.
fn result_codec() -> MessageCodec {
    codec(
        "move_gripper_result",
        json!({ "success": "bool", "message": "string" }),
    )
}

/// The endpoint serving [`EXPOSURE`] on a loopback port, each goal driven
/// by the bridge to the mesh's provider.
struct Endpoint {
    url: String,
    shutdown: CancellationToken,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Endpoint {
    /// Serves the prepared exposure. Each task handler drives its goal as
    /// `run_task` does, to the one provider of the mesh, which stands for
    /// the producer the launcher bound to the target.
    async fn serve(mesh: &Mesh) -> Self {
        let exposure = prepared_exposure();
        let mut builder = ExposureServer::builder(exposure.bundle);
        for task in exposure.tasks {
            assert_eq!(task.binding.target, LINK_ID);
            assert_eq!(task.binding.member, MEMBER);
            let task = Arc::new(task);
            let messenger = mesh.bridge_messenger.clone();
            let producer = mesh.producer.clone();
            builder = builder.with_task(
                task.name.clone(),
                move |call: ToolCall, context: ActionContext| {
                    let task = Arc::clone(&task);
                    let messenger = messenger.clone();
                    let producer = producer.clone();
                    async move {
                        drive_goal(
                            &task,
                            &messenger,
                            &bridge_identity(),
                            &task.binding.member_binding(),
                            &producer,
                            call.input,
                            &context,
                        )
                        .await
                    }
                },
            );
        }
        let server = builder.build().expect("the catalog and the handlers agree");
        let path = server.endpoint_path();
        let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
            .await
            .expect("an OS-assigned loopback port binds");
        let address = listener
            .local_addr()
            .expect("a bound listener has an address");
        let set = ExposureSet::new(vec![server]).expect("one exposure composes");
        let shutdown = CancellationToken::new();
        let served = tokio::spawn(set.serve(listener, shutdown.clone()));
        Self {
            url: format!("http://{address}{path}"),
            shutdown,
            served,
        }
    }

    /// Calls the tool as a client without the tasks extension, with a
    /// progress token. Returns once the response headers arrive.
    fn call(&self, width: f64) -> impl Future<Output = reqwest::Response> + Send + 'static {
        let request = reqwest::Client::new()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", TOOL)
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": {
                        "name": TOOL,
                        "arguments": { "width": width },
                        "_meta": {
                            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
                            "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
                            "io.modelcontextprotocol/clientCapabilities": {},
                            "progressToken": PROGRESS_TOKEN
                        }
                    }
                })
                .to_string(),
            );
        async move {
            tokio::time::timeout(READINESS_TIMEOUT, request.send())
                .await
                .expect("the response headers arrive")
                .expect("the call opens")
        }
    }

    /// Stops the listener and settles the serve task.
    async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(READINESS_TIMEOUT, self.served)
            .await
            .expect("the serve task ends once the token is cancelled")
            .expect("the serve task does not panic")
            .expect("serving the endpoint succeeds");
    }
}

/// The response stream of a call, read one server-sent event at a time.
struct Events {
    response: reqwest::Response,
    unread: String,
}

impl Events {
    /// The stream of a call whose headers say it streams events.
    fn of(response: reqwest::Response) -> Self {
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default();
        assert!(
            content_type.starts_with("text/event-stream"),
            "the call answers with an event stream, not with {content_type:?}"
        );
        Self {
            response,
            unread: String::new(),
        }
    }

    /// The JSON-RPC message the next event carries. An event without data
    /// carries no message and is passed over.
    async fn next_message(&mut self) -> Value {
        loop {
            if let Some(end) = self.unread.find("\n\n") {
                let event: String = self.unread.drain(..end + 2).collect();
                let data: Vec<&str> = event
                    .lines()
                    .filter_map(|line| line.strip_prefix("data:"))
                    .map(str::trim_start)
                    .collect();
                if data.is_empty() {
                    continue;
                }
                return serde_json::from_str(&data.join("\n"))
                    .expect("an event carries one JSON-RPC message");
            }
            let chunk = tokio::time::timeout(READINESS_TIMEOUT, self.response.chunk())
                .await
                .expect("the next event arrives")
                .expect("the stream is readable")
                .expect("the stream carries another event");
            let text = std::str::from_utf8(&chunk).expect("server-sent events are text");
            self.unread.push_str(&text.replace("\r\n", "\n"));
        }
    }
}

/// Asserts that `message` is the progress notification of one feedback
/// message, `feedback`, on the call.
fn assert_progress(message: &Value, feedback: &str) {
    assert_eq!(message["method"], "notifications/progress", "{message}");
    assert_eq!(message["params"]["progressToken"], PROGRESS_TOKEN);
    assert_eq!(message["params"]["message"], feedback);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_progress_bound_call_opens_with_the_first_feedback_of_its_held_goal() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let call = tokio::spawn(endpoint.call(0.04));
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    // The provider holds the goal: the headers and the first event come
    // with its first feedback message, long before it settles.
    let mut events = Events::of(call.await.expect("the call does not panic"));
    assert_progress(&events.next_message().await, r#"{"percent":10}"#);

    context
        .complete(encoded(
            &result_codec(),
            json!({ "success": true, "message": "the gripper is at 0.04 m" }),
        ))
        .await
        .expect("the provider completes the goal");
    let answer = events.next_message().await;
    assert_eq!(answer["id"], 1, "{answer}");
    assert_eq!(answer["result"]["isError"], false, "{answer}");
    assert_eq!(
        answer["result"]["structuredContent"],
        json!({ "success": true, "message": "the gripper is at 0.04 m" })
    );

    drop(events);
    endpoint.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_its_provider_cancels_ends_the_call_with_the_provider_s_result() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let call = tokio::spawn(endpoint.call(0.04));
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    let mut events = Events::of(call.await.expect("the call does not panic"));
    assert_progress(&events.next_message().await, r#"{"percent":10}"#);

    // No one asked for a cancel: the provider ends the goal cancelled on
    // its own, and its result says why.
    let result = json!({
        "success": false,
        "message": "the move to 0.04 m was replaced by the move to 0.08 m",
    });
    context
        .complete_cancelled(encoded(&result_codec(), result.clone()))
        .await
        .expect("the provider settles the goal cancelled");
    let answer = events.next_message().await;
    assert_eq!(answer["id"], 1, "{answer}");
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    assert_eq!(answer["result"]["structuredContent"], result);
    assert_eq!(
        answer["result"]["content"],
        json!([
            { "type": "text", "text": "the action was cancelled" },
            { "type": "text", "text": result.to_string() },
        ])
    );

    drop(events);
    endpoint.stop().await;
}
