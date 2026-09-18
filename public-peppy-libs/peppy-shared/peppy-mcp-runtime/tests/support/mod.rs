//! Shared fixture and client helpers for the HTTP integration tests.
//!
//! The fixture is a set of two exposures served on one listener: two tags
//! of one exposure, `camera_and_recording:v1` and `camera_and_recording:v2`,
//! with identical public names but their own identity, prose, handlers, and
//! clock. Every suite drives each endpoint through the real rmcp client
//! over Streamable HTTP; the Peppy side is stubbed, so snapshots are fed
//! through each server's ingest directly.
//!
//! Each integration test binary compiles this module separately and uses a
//! subset of it, so unused-item warnings are suppressed for the module.
#![allow(dead_code)]

use peppy_mcp_catalog::ExposureBundle;
use peppy_mcp_runtime::{ActionContext, ActionExit, Clock, ExposureServer, ExposureSet};
use rmcp::model::{
    ClientCapabilities, ClientInfo, DetailedTask, GetTaskParams, ProtocolVersion, UpdateTaskParams,
};
use rmcp::service::ServiceError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{ClientLifecycleMode, ClientServiceExt};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

pub const STATUS_URI: &str = "peppy://resource/front_camera.status";
pub const FRAME_URI: &str = "peppy://resource/front_camera.latest_frame";

/// Guard for waits that are already response-driven; generous on purpose so
/// it only fires when something is genuinely broken.
pub const GUARD: Duration = Duration::from_secs(30);

pub type Client = rmcp::service::RunningService<rmcp::RoleClient, ClientInfo>;

/// What a test expects one endpoint of the fixture set to advertise and
/// answer, so a suite can run the same assertions against each endpoint.
#[derive(Debug, Clone)]
pub struct Expected {
    pub tag: &'static str,
    pub title: &'static str,
    pub instructions: &'static str,
    /// The structured result of the `front_camera.info` tool.
    pub info: Value,
}

/// The two exposures of the fixture set.
pub fn fixture_exposures() -> [Expected; 2] {
    [
        Expected {
            tag: "v1",
            title: "OpenArm camera",
            instructions: "Observe the front camera on this robot.",
            info: json!({ "width": 640, "height": 480 }),
        },
        Expected {
            tag: "v2",
            title: "OpenArm rear camera",
            instructions: "Observe the rear camera on this robot.",
            info: json!({ "width": 1920, "height": 1080 }),
        },
    ]
}

/// The bundle of one fixture exposure: the same selection under each tag,
/// with the tag's own identity and prose.
pub fn fixture_bundle(expected: &Expected) -> ExposureBundle {
    let json = BUNDLE_TEMPLATE
        .replace("{tag}", expected.tag)
        .replace("{title}", expected.title)
        .replace("{instructions}", expected.instructions);
    ExposureBundle::from_json_str(&json).expect("fixture bundle parses")
}

const BUNDLE_TEMPLATE: &str = r#"{
  "bundle_format": 1,
  "schema_mapping_version": 1,
  "exposure": { "name": "camera_and_recording", "tag": "{tag}" },
  "server": {
    "title": "{title}",
    "instructions": "{instructions}"
  },
  "contracts": [
    { "name": "rgb_camera", "tag": "v1", "sha256": "aa", "link_id": "front_camera" },
    { "name": "episode_recording", "tag": "v1", "sha256": "bb", "link_id": "recorder" }
  ],
  "resources": [
    {
      "name": "front_camera.status",
      "uri": "peppy://resource/front_camera.status",
      "description": "Latest camera status.",
      "target": "front_camera",
      "member": "camera_status",
      "policies": {
        "freshness": { "max_age_ms": 2000 },
        "update": { "max_hz": 2.0 }
      },
      "schema": {
        "type": "object",
        "properties": { "battery": { "type": "integer", "minimum": 0, "maximum": 255 } },
        "required": ["battery"],
        "additionalProperties": false
      }
    },
    {
      "name": "front_camera.latest_frame",
      "uri": "peppy://resource/front_camera.latest_frame",
      "description": "Latest frame from the front-facing camera, JPEG encoded.",
      "target": "front_camera",
      "member": "video_stream",
      "policies": {
        "freshness": { "max_age_ms": 2000 },
        "update": { "max_hz": 2.0 },
        "representation": {
          "image": "jpeg",
          "quality": 80,
          "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
        },
        "max_result_bytes": 524288,
        "on_oversize": "downscale"
      },
      "schema": { "type": "object" }
    }
  ],
  "tools": [
    {
      "name": "front_camera.info",
      "description": "Report the camera's resolution, frame rate, and encoding.",
      "target": "front_camera",
      "member": "video_stream_info",
      "operation": "read_only",
      "deadline_ms": 2000,
      "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
      "output_schema": {
        "type": "object",
        "properties": {
          "width": { "type": "integer", "minimum": 0, "maximum": 65535 },
          "height": { "type": "integer", "minimum": 0, "maximum": 65535 }
        },
        "required": ["width", "height"],
        "additionalProperties": false
      }
    },
    {
      "name": "front_camera.set_brightness",
      "description": "Set the camera brightness in device units.",
      "target": "front_camera",
      "member": "set_brightness",
      "operation": "mutating",
      "deadline_ms": 2000,
      "input_schema": {
        "type": "object",
        "properties": {
          "value": { "type": "integer", "minimum": -64, "maximum": 64 }
        },
        "required": ["value"],
        "additionalProperties": false
      },
      "output_schema": {
        "type": "object",
        "properties": { "applied": { "type": "boolean" } },
        "required": ["applied"],
        "additionalProperties": false
      }
    }
  ],
  "tasks": [
    {
      "name": "recorder.record_episode",
      "description": "Record one teleoperation episode to the local dataset.",
      "target": "recorder",
      "member": "record_episode",
      "operation": "long_running",
      "safety_sensitive": true,
      "confirmation_required": true,
      "deadline_ms": 600000,
      "input_schema": {
        "type": "object",
        "properties": { "episode_name": { "type": "string" } },
        "required": ["episode_name"],
        "additionalProperties": false
      },
      "output_schema": {
        "type": "object",
        "properties": { "frames": { "type": "integer" } },
        "required": ["frames"],
        "additionalProperties": false
      },
      "feedback_schema": {
        "type": "object",
        "properties": { "frame": { "type": "integer" } },
        "required": ["frame"],
        "additionalProperties": false
      }
    }
  ]
}"#;

/// One served endpoint of the fixture set, with the in-process handle its
/// snapshots are fed through and the manual clock its policies run on.
pub struct Endpoint {
    pub url: String,
    pub expected: Expected,
    pub server: ExposureServer,
    pub nanos: Arc<AtomicU64>,
}

impl Endpoint {
    /// The path the endpoint is served at on the shared listener.
    pub fn path(&self) -> String {
        self.server.endpoint_path()
    }
}

/// The fixture set as served: every endpoint plus the listener they share.
pub struct ServedSet {
    pub base_url: String,
    pub endpoints: Vec<Endpoint>,
    pub shutdown: tokio_util::sync::CancellationToken,
    served: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl ServedSet {
    /// Cancels the listener and settles the serve task. Ending a test this
    /// way is what turns a serve failure into a named failure here, rather
    /// than into whatever timed out downstream of it.
    pub async fn stop(self) {
        self.shutdown.cancel();
        tokio::time::timeout(GUARD, self.served)
            .await
            .expect("the serve task ends once the token is cancelled")
            .expect("the serve task does not panic")
            .expect("serving the set succeeds");
    }

    /// The URL of a path on the shared listener.
    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base_url)
    }
}

/// A server for one fixture exposure over `bundle` on its own manual
/// clock, with the camera tool handlers registered and the task handler
/// left to the caller.
pub fn fixture_server(
    expected: &Expected,
    bundle: ExposureBundle,
) -> (peppy_mcp_runtime::ExposureServerBuilder, Arc<AtomicU64>) {
    let (clock, nanos) = manual_clock();
    let builder = with_camera_tools(ExposureServer::builder(bundle).with_clock(clock), expected);
    (builder, nanos)
}

/// The `recorder.record_episode` handler: reports the episode as feedback,
/// parks on cancellation for `wait_for_cancel`, completes otherwise.
pub async fn record_episode(input: Value, context: ActionContext) -> Result<Value, ActionExit> {
    let episode = input["episode_name"]
        .as_str()
        .expect("validated string")
        .to_string();
    context.report_feedback(format!("recording `{episode}`"));
    if episode == "wait_for_cancel" {
        context.cancel_requested().await;
        return Err(ActionExit::Cancelled);
    }
    Ok(json!({ "frames": 120 }))
}

/// Serves the full fixture set: both exposures with their tool handlers and
/// the `recorder.record_episode` task handler.
pub async fn start_set() -> ServedSet {
    let mut servers = Vec::new();
    for expected in fixture_exposures() {
        let (builder, nanos) = fixture_server(&expected, fixture_bundle(&expected));
        let server = builder
            .with_task("recorder.record_episode", record_episode)
            .build()
            .expect("bundle and handlers agree");
        servers.push((expected, server, nanos));
    }
    serve_set(servers).await
}

/// Serves the fixture set stripped of its tasks, so no endpoint advertises
/// the tasks extension.
pub async fn start_task_less_set() -> ServedSet {
    let mut servers = Vec::new();
    for expected in fixture_exposures() {
        let mut bundle = fixture_bundle(&expected);
        bundle.tasks.clear();
        let (builder, nanos) = fixture_server(&expected, bundle);
        let server = builder.build().expect("bundle and handlers agree");
        servers.push((expected, server, nanos));
    }
    serve_set(servers).await
}

fn manual_clock() -> (Clock, Arc<AtomicU64>) {
    let nanos = Arc::new(AtomicU64::new(0));
    let source = Arc::clone(&nanos);
    (
        Clock::from_nanos_fn(move || source.load(Ordering::SeqCst)),
        nanos,
    )
}

fn with_camera_tools(
    builder: peppy_mcp_runtime::ExposureServerBuilder,
    expected: &Expected,
) -> peppy_mcp_runtime::ExposureServerBuilder {
    let info = expected.info.clone();
    builder
        .with_tool("front_camera.info", move |_input: Value| {
            let info = info.clone();
            async move { Ok(info) }
        })
        .with_tool("front_camera.set_brightness", |input: Value| async move {
            let value = input["value"].as_i64().expect("validated integer");
            if value == 13 {
                return Err(peppy_mcp_runtime::ToolCallError::Failed(
                    "13 is reserved".to_string(),
                ));
            }
            Ok(json!({ "applied": true }))
        })
}

/// Binds an OS-assigned loopback port and serves the servers as one set.
pub async fn serve_set(servers: Vec<(Expected, ExposureServer, Arc<AtomicU64>)>) -> ServedSet {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0))
        .await
        .expect("an OS-assigned loopback port binds");
    let address = listener
        .local_addr()
        .expect("bound listener has an address");
    let base_url = format!("http://{address}");
    let set = ExposureSet::new(
        servers
            .iter()
            .map(|(_, server, _)| server.clone())
            .collect(),
    )
    .expect("distinct exposures compose");
    let shutdown = tokio_util::sync::CancellationToken::new();
    let served = tokio::spawn(set.serve(listener, shutdown.clone()));

    let endpoints = servers
        .into_iter()
        .map(|(expected, server, nanos)| Endpoint {
            url: format!("{base_url}{}", server.endpoint_path()),
            expected,
            server,
            nanos,
        })
        .collect();
    ServedSet {
        base_url,
        endpoints,
        shutdown,
        served,
    }
}

pub async fn connect(url: &str) -> Client {
    connect_as(url, ClientCapabilities::default()).await
}

/// Connects a client that declares the SEP-2663 tasks extension capability;
/// in discover mode the SDK attaches it to every request's `_meta`.
pub async fn connect_with_tasks(url: &str) -> Client {
    connect_as(url, ClientCapabilities::builder().enable_tasks().build()).await
}

async fn connect_as(url: &str, capabilities: ClientCapabilities) -> Client {
    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(url.to_string()),
    );
    let mut info = ClientInfo::default();
    info.capabilities = capabilities;
    info.serve_with_lifecycle(
        transport,
        ClientLifecycleMode::Discover {
            preferred_versions: vec![ProtocolVersion::V_2026_07_28],
        },
    )
    .await
    .expect("client negotiates 2026-07-28 over loopback")
}

pub fn protocol_error(error: ServiceError) -> rmcp::ErrorData {
    match error {
        ServiceError::McpError(data) => data,
        other => panic!("expected a protocol error, got {other:?}"),
    }
}

/// Polls `tasks/get` until the task satisfies `accept`; the wait is bounded
/// by [`GUARD`] and driven by server responses, not by elapsed host time.
pub async fn poll_task_until(
    client: &Client,
    task_id: &str,
    description: &str,
    accept: impl Fn(&DetailedTask) -> bool,
) -> DetailedTask {
    tokio::time::timeout(GUARD, async {
        loop {
            let result = client
                .get_task(GetTaskParams::new(task_id))
                .await
                .expect("tasks/get answers");
            if accept(&result.task) {
                return result.task;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("task `{task_id}` never reached: {description}"))
}

pub fn confirmation_accept(task_id: &str) -> UpdateTaskParams {
    UpdateTaskParams::new(
        task_id,
        [("confirmation".to_string(), json!({ "action": "accept" }))]
            .into_iter()
            .collect(),
    )
}

/// A tiny valid rgb8 frame in the shape the JPEG representation expects.
pub fn sample_rgb8_frame() -> Value {
    let pixels: Vec<u8> = (0..8u32 * 8 * 3).map(|index| (index % 251) as u8).collect();
    let pixels_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &pixels);
    json!({
        "frame": pixels_b64,
        "encoding": "rgb8",
        "width": 8,
        "height": 8,
    })
}
