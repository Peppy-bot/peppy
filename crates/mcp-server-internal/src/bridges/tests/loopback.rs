//! The bridge behind a real endpoint: an exposure document validated
//! against its contract, laid out by [`prepare`], served by the runtime
//! over Streamable HTTP on `127.0.0.1`, and called with raw HTTP requests,
//! so the test reads the response stream of the call itself. The providers
//! are the mock action server and the mock service of the bridge tests,
//! and the action server holds each goal until the test ends it.

use super::{
    LINK_ID, MEMBER, Mesh, PROVIDER_REASON, SERVICE, accepted_goal, bridge_identity, codec,
    encoded, end_cancelled, kind, mesh, publish_percent, service_provider, unreachable_width,
};
use crate::bridges::{PreparedExposure, call_service, drive_goal, prepare};
use crate::test_support::Events;
use daemon_config::contract::PeppyContractParser;
use daemon_config::mcp_exposure::PeppyMcpExposureParser;
use daemon_config::repository::ManifestFingerprint;
use message_codec::MessageCodec;
use peppy_mcp_catalog::{DeclaredMembers, ResolvedContract, build_exposure_bundle};
use peppy_mcp_runtime::{ActionContext, ExposureServer, ExposureSet, ToolCall};
use peppylib::messaging::Failure;
use peppylib::testing::{MockActionServerCore, MockPendingGoal, READINESS_TIMEOUT};
use peppylib::types::Payload;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// The contract behind the endpoint: the gripper action of the bridge
/// tests, with a result that says where the gripper is, the service that
/// checks a width, and the kind the contract declares of its own.
const CONTRACT: &str = r#"{
    peppy_schema: "contract/v1",
    manifest: { name: "limb_motion", tag: "v1" },
    failure_kinds: {
        unreachable: "No plan to the width",
    },
    interfaces: {
        services: [
            {
                name: "check_width",
                response_message_format: { width: "f64" },
            },
        ],
        actions: [
            {
                name: "move_gripper",
                goal_service: { request_message_format: { width: "f64" } },
                feedback_topic: {
                    qos_profile: "reliable",
                    message_format: { percent: "u8" },
                },
                result_service: {
                    response_message_format: { width: "f64" },
                },
            },
        ],
    },
}"#;

/// The exposure: the gripper action as a tool bounded by its progress, the
/// width check as a tool that changes something, so the record keeps it,
/// and the record. The window is far longer than any step of the tests,
/// which never wait on it.
const EXPOSURE: &str = r#"{
    peppy_schema: "mcp_exposure/v1",
    manifest: { name: "gripper", tag: "v1" },
    server: { title: "Gripper" },
    call_record: {
        tool: "gripper.recent_calls",
        description: "The last calls of this endpoint that could move the gripper.",
        keep: 10,
    },
    targets: {
        limb_motion: {
            contract: { name: "limb_motion", tag: "v1" },
            services: [
                {
                    member: "check_width",
                    tool: "limb_motion.check_width",
                    description: "Check the width the gripper can reach.",
                    operation: "mutating",
                    deadline_ms: 600000,
                },
            ],
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
const SERVICE_TOOL: &str = "limb_motion.check_width";
const RECORD_TOOL: &str = "gripper.recent_calls";

/// The progress token every call of the action carries.
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
            failure_kinds: &contract.failure_kinds,
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
    codec("move_gripper_result", json!({ "width": "f64" }))
}

/// The endpoint serving [`EXPOSURE`] on a loopback port, each call driven
/// by the bridge to the mesh's providers.
struct Endpoint {
    url: String,
    shutdown: CancellationToken,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Endpoint {
    /// Serves the prepared exposure. Each handler drives its call as
    /// `call_tool` and `run_task` do, to the providers of the mesh, which
    /// stand for the producer the launcher bound to the target.
    async fn serve(mesh: &Mesh) -> Self {
        let exposure = prepared_exposure();
        let mut builder = ExposureServer::builder(exposure.bundle);
        for tool in exposure.tools {
            assert_eq!(tool.binding.target, LINK_ID);
            assert_eq!(tool.binding.member, SERVICE);
            let tool = Arc::new(tool);
            let messenger = mesh.bridge_messenger.clone();
            let producer = mesh.producer.clone();
            builder = builder.with_tool(tool.name.clone(), move |call: ToolCall| {
                let tool = Arc::clone(&tool);
                let messenger = messenger.clone();
                let producer = producer.clone();
                async move {
                    call_service(
                        &tool,
                        &messenger,
                        &bridge_identity(),
                        &tool.binding.member_binding(),
                        &producer,
                        &call.input,
                    )
                    .await
                }
            });
        }
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

    /// Calls `tool` with `arguments` as a client without the tasks
    /// extension, with `progress_token` when one is given. Returns once the
    /// response headers arrive.
    fn call_tool(
        &self,
        tool: &str,
        arguments: Value,
        progress_token: Option<&str>,
    ) -> impl Future<Output = reqwest::Response> + Send + 'static {
        let mut meta = json!({
            "io.modelcontextprotocol/protocolVersion": "2026-07-28",
            "io.modelcontextprotocol/clientInfo": { "name": "raw", "version": "0" },
            "io.modelcontextprotocol/clientCapabilities": {},
        });
        if let Some(token) = progress_token {
            meta["progressToken"] = json!(token);
        }
        let request = reqwest::Client::new()
            .post(&self.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream")
            .header("mcp-protocol-version", "2026-07-28")
            .header("mcp-method", "tools/call")
            .header("mcp-name", tool)
            .body(
                json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": tool, "arguments": arguments, "_meta": meta }
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

    /// Moves the gripper with a progress token. Returns once the response
    /// headers arrive.
    fn move_gripper(&self, width: f64) -> impl Future<Output = reqwest::Response> + Send + 'static {
        self.call_tool(TOOL, json!({ "width": width }), Some(PROGRESS_TOKEN))
    }

    /// The result of a call that carries no progress token, whose response
    /// is the result alone.
    async fn result_of(&self, tool: &str, arguments: Value) -> Value {
        let mut events = Events::of(self.call_tool(tool, arguments, None).await);
        let answer = events.next_message().await;
        assert_eq!(answer["id"], 1, "{answer}");
        answer["result"].clone()
    }

    /// The entries of the record, newest first.
    async fn recorded_calls(&self) -> Vec<Value> {
        let result = self.result_of(RECORD_TOOL, json!({})).await;
        result["structuredContent"]["calls"]
            .as_array()
            .cloned()
            .expect("the record lists its calls")
    }

    /// The entry of the newest call of `tool`, once the call is no longer
    /// running. The wait is bounded by [`READINESS_TIMEOUT`] and driven by
    /// the endpoint's answers.
    async fn ended_entry(&self, tool: &str) -> Value {
        tokio::time::timeout(READINESS_TIMEOUT, async {
            loop {
                let calls = self.recorded_calls().await;
                let newest = calls.into_iter().find(|call| call["tool"] == tool);
                if let Some(entry) = newest.filter(|entry| entry["outcome"] != "running") {
                    return entry;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("the call of `{tool}` never ended"))
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

/// Asserts that `message` is the progress notification of one feedback
/// message, `feedback`, on the call.
fn assert_progress(message: &Value, feedback: &str) {
    assert_eq!(message["method"], "notifications/progress", "{message}");
    assert_eq!(message["params"]["progressToken"], PROGRESS_TOKEN);
    assert_eq!(message["params"]["message"], feedback);
}

/// How a call that a provider refused or failed ended: its outcome, the
/// failure kind and the message.
struct Ending<'a> {
    outcome: &'a str,
    kind: &'a str,
    message: &'a str,
}

impl Ending<'_> {
    /// Asserts that `result` is the tool error of the call: `summary` as its
    /// one text, and the ending as its structured content.
    fn assert_result(&self, result: &Value, summary: &str) {
        assert_eq!(result["isError"], true, "{result}");
        assert_eq!(
            result["content"],
            json!([{ "type": "text", "text": summary }]),
            "{result}"
        );
        assert_eq!(
            result["structuredContent"],
            json!({ "outcome": self.outcome, "failure_kind": self.kind, "message": self.message }),
            "{result}"
        );
    }

    /// Asserts that `entry` of the record keeps the ending.
    fn assert_recorded(&self, entry: &Value) {
        assert_eq!(entry["outcome"], self.outcome, "{entry}");
        assert_eq!(entry["failure_kind"], self.kind, "{entry}");
        assert_eq!(entry["message"], self.message, "{entry}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_progress_bound_call_opens_with_the_first_feedback_of_its_held_goal() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let call = tokio::spawn(endpoint.move_gripper(0.04));
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    // The provider holds the goal: the headers and the first event come
    // with its first feedback message, long before it settles.
    let mut events = Events::of(call.await.expect("the call does not panic"));
    assert_progress(&events.next_message().await, r#"{"percent":10}"#);

    context
        .complete(encoded(&result_codec(), json!({ "width": 0.04 })))
        .await
        .expect("the provider completes the goal");
    let answer = events.next_message().await;
    assert_eq!(answer["id"], 1, "{answer}");
    assert_eq!(answer["result"]["isError"], false, "{answer}");
    assert_eq!(
        answer["result"]["structuredContent"],
        json!({ "width": 0.04 })
    );

    drop(events);
    let entry = endpoint.ended_entry(TOOL).await;
    assert_eq!(entry["outcome"], "completed", "{entry}");
    assert_eq!(entry["failure_kind"], Value::Null, "{entry}");
    assert_eq!(entry["message"], Value::Null, "{entry}");
    endpoint.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_goal_its_provider_cancels_ends_the_call_with_the_reason_and_the_result() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let call = tokio::spawn(endpoint.move_gripper(0.04));
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    let mut events = Events::of(call.await.expect("the call does not panic"));
    assert_progress(&events.next_message().await, r#"{"percent":10}"#);

    // No one asked for a cancel: the provider ends the goal cancelled on
    // its own, and says why.
    end_cancelled(&context).await;
    let answer = events.next_message().await;
    assert_eq!(answer["id"], 1, "{answer}");
    assert_eq!(answer["result"]["isError"], true, "{answer}");
    let result = json!({ "width": 0.02 });
    assert_eq!(answer["result"]["structuredContent"], result);
    assert_eq!(
        answer["result"]["content"],
        json!([
            { "type": "text", "text": format!("the action was cancelled: {PROVIDER_REASON}") },
            { "type": "text", "text": result.to_string() },
        ])
    );

    drop(events);
    let entry = endpoint.ended_entry(TOOL).await;
    assert_eq!(entry["outcome"], "cancelled", "{entry}");
    assert_eq!(entry["message"], PROVIDER_REASON, "{entry}");
    endpoint.stop().await;
}

/// Calls the gripper's action with no progress token and lets `provider`
/// end the goal as `end` does. The call's result, once it settles.
async fn move_gripper_ending<F>(
    endpoint: &Endpoint,
    provider: &mut MockActionServerCore,
    end: impl FnOnce(MockPendingGoal) -> F,
) -> Value
where
    F: Future<Output = ()>,
{
    let result = endpoint.result_of(TOOL, json!({ "width": 0.09 }));
    let ending = async {
        let pending = provider
            .next_goal(READINESS_TIMEOUT)
            .await
            .expect("the goal reaches the provider");
        end(pending).await;
    };
    let (result, ()) = tokio::join!(result, ending);
    result
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_refused_and_a_failed_goal_reach_the_client_and_the_record_with_their_kind() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let refused = move_gripper_ending(&endpoint, &mut provider, |pending| async {
        pending
            .refuse(unreachable_width())
            .await
            .expect("the provider refuses the goal");
    })
    .await;
    let ending = Ending {
        outcome: "refused",
        kind: "unreachable",
        message: "no plan reaches 0.09 m",
    };
    ending.assert_result(
        &refused,
        "the provider refused the goal: unreachable: no plan reaches 0.09 m",
    );
    ending.assert_recorded(&endpoint.ended_entry(TOOL).await);

    let failed = move_gripper_ending(&endpoint, &mut provider, |pending| async {
        let context = pending
            .accept(Payload::new())
            .await
            .expect("the provider accepts the goal");
        context
            .fail(Failure::new(kind("timeout"), "the jaws stalled at 0.05 m"))
            .await
            .expect("the provider fails the goal");
    })
    .await;
    let ending = Ending {
        outcome: "failed",
        kind: "timeout",
        message: "the jaws stalled at 0.05 m",
    };
    ending.assert_result(
        &failed,
        "the provider failed the goal: timeout: the jaws stalled at 0.05 m",
    );
    ending.assert_recorded(&endpoint.ended_entry(TOOL).await);

    endpoint.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_error_reply_reaches_the_client_and_the_record_with_its_kind() {
    let (mesh, _actions) = mesh(true).await;
    let service = service_provider(&mesh).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let replying = async {
        let (_request, responder) = service
            .next_request(READINESS_TIMEOUT)
            .await
            .expect("the call reaches the provider");
        responder
            .respond_failure(unreachable_width())
            .await
            .expect("the provider gives an error reply");
    };
    let (result, ()) = tokio::join!(endpoint.result_of(SERVICE_TOOL, json!({})), replying);

    let ending = Ending {
        outcome: "failed",
        kind: "unreachable",
        message: "no plan reaches 0.09 m",
    };
    ending.assert_result(
        &result,
        "the provider gave an error reply: unreachable: no plan reaches 0.09 m",
    );
    ending.assert_recorded(&endpoint.ended_entry(SERVICE_TOOL).await);
    endpoint.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn closing_the_call_leaves_its_goal_running_to_its_end() {
    let (mesh, mut provider) = mesh(true).await;
    let endpoint = Endpoint::serve(&mesh).await;

    let call = tokio::spawn(endpoint.move_gripper(0.04));
    let context = accepted_goal(&mut provider).await;
    publish_percent(&context, 10).await;
    let mut events = Events::of(call.await.expect("the call does not panic"));
    assert_progress(&events.next_message().await, r#"{"percent":10}"#);

    // The client goes away mid-goal. In Streamable HTTP that is also how a
    // client cancels a call: the endpoint cannot tell the two apart, and
    // the goal runs on. The provider reports and ends the goal afterwards.
    drop(events);
    publish_percent(&context, 60).await;
    context
        .complete(encoded(&result_codec(), json!({ "width": 0.04 })))
        .await
        .expect("the provider completes the goal");

    let entry = endpoint.ended_entry(TOOL).await;
    assert_eq!(entry["outcome"], "completed", "{entry}");
    assert!(
        !context.is_cancelled(),
        "the provider got no cancel for a client that went away"
    );
    endpoint.stop().await;
}
