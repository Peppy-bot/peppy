//! The MCP server: a catalog-driven
//! [`ServerHandler`](rmcp::ServerHandler) per exposure bundle, and the
//! [`ExposureSet`] serving several of them side by side over Streamable
//! HTTP under MCP `2026-07-28`.

use crate::clock::Clock;
use crate::error::{BuildError, ToolCallError};
use crate::fleet::{
    Fleet, FleetMember, FleetRuntime, FleetSource, MemberAddress, published_name, published_uri,
    quoted,
};
use crate::state::{CatalogEvent, ReadRefusal, ResourceIngest, ResourceState};
use crate::tasks::{ActionContext, ActionExit, ActionSurface, TaskHandler};
use peppy_mcp_catalog::{
    BundleIdentity, BundleServer, DescribeSource, ExposureBundle, ResourceEntry, ServiceOperation,
    TaskEntry, ToolEntry,
};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
    ClientCapabilities, ContentBlock, CreateTaskResult, DiscoverResult, ElicitRequest,
    ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema, ErrorCode,
    GetTaskParams, GetTaskResult, Implementation, InputRequest, JsonObject, ListResourcesResult,
    ListToolsResult, PaginatedRequestParams, ProgressNotificationParam, ProgressToken,
    ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerInfo, SubscriptionFilter, Tool, ToolAnnotations,
    UpdateTaskParams,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSendError};
use rmcp::task_manager::{TaskContext, TaskExit, TaskManager, TaskOptions};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, Peer, RoleServer, ServerHandler};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

/// The id the SEP-2663 tasks extension is declared under in a client's
/// `extensions` capability map.
const TASKS_EXTENSION_ID: &str = "io.modelcontextprotocol/tasks";

/// `ttlMs` for the catalog-shaped results: discovery, `tools/list`, and
/// `resources/list`. The catalog is fixed for the life of the server (a
/// changed exposure restarts the process serving it), so clients may cache
/// it for as long as they keep the connection.
const UNAVAILABLE_SINCE_START: &str =
    "unavailable: nothing has been published since the server started";
const UNAVAILABLE_SINCE_JOIN: &str =
    "unavailable: nothing has been published since the robot joined";
const CATALOG_TTL_MS: u64 = 3_600_000;

/// Grace period the advertised task TTL carries on top of the exposure's
/// whole-goal deadline. The runtime fails an overrunning goal itself, with a
/// message naming the deadline; the manager's TTL sweep fires at
/// `created + ttl` and aborts the operation with a generic expiry instead, so
/// the two must not coincide. The task stays observable for a further TTL
/// window past that, which is what a poller reads the terminal state from.
const TASK_TTL_GRACE_MS: u64 = 1_000;

/// Capacity of the resource-updated event channel; a listener lagging this
/// far behind skips to the newest events, which for latest-snapshot
/// semantics loses nothing that a fresh read would not recover.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One validated call handed to a bridge: the canonical-JSON input the
/// contract member takes and, on a per-robot surface, the member of the
/// target the call goes to. On a fixed surface `member` is `None` and the
/// bridge calls the producer the launcher bound.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub input: Value,
    pub member: Option<MemberAddress>,
}

/// One registered bridge: a validated call in, canonical-JSON output or a
/// [`ToolCallError`] out. Any `Fn(ToolCall) -> impl Future` with those
/// shapes implements it.
pub trait ToolHandler: Send + Sync + 'static {
    fn call(
        &self,
        call: ToolCall,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolCallError>> + Send>>;
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(ToolCall) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ToolCallError>> + Send + 'static,
{
    fn call(
        &self,
        call: ToolCall,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolCallError>> + Send>> {
        Box::pin(self(call))
    }
}

struct ToolState {
    entry: ToolEntry,
    /// Compiled from the bundle's derived input schema; every call is
    /// validated before it can reach the Peppy graph.
    validator: jsonschema::Validator,
    handler: Arc<dyn ToolHandler>,
}

struct TaskState {
    entry: TaskEntry,
    /// Compiled from the bundle's derived goal schema; every call is
    /// validated before a goal can run.
    validator: jsonschema::Validator,
    handler: Arc<dyn TaskHandler>,
}

struct ServerState {
    server: BundleServer,
    exposure: BundleIdentity,
    resources_by_uri: HashMap<String, Arc<ResourceState>>,
    resource_uri_by_name: HashMap<String, String>,
    resource_list: Vec<Resource>,
    tools: HashMap<String, Arc<ToolState>>,
    tasks: HashMap<String, Arc<TaskState>>,
    /// `tools/list` order: the bundle's tools, then its tasks.
    tool_list: Vec<Tool>,
    /// Task handles are in-memory and live as long as the serving process
    /// by design; every HTTP session shares this manager, which is what
    /// lets a reconnecting client keep polling an existing task id.
    manager: TaskManager,
    events: broadcast::Sender<CatalogEvent>,
    clock: Clock,
    /// The per-robot surface, on a bundle that declares one.
    fleet: Option<FleetRuntime>,
}

/// Builds an [`ExposureServer`] from a parsed bundle, one registered
/// handler per exposed tool, one task handler per exposed action, and the
/// source of the fleet on a per-robot bundle.
pub struct ExposureServerBuilder {
    bundle: ExposureBundle,
    clock: Clock,
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
    task_handlers: HashMap<String, Arc<dyn TaskHandler>>,
    fleet_source: Option<Arc<dyn FleetSource>>,
}

impl ExposureServerBuilder {
    /// Registers where the server reads the fleet of a per-robot bundle:
    /// the members of every target as the stack binds them now.
    pub fn with_fleet(mut self, source: impl FleetSource) -> Self {
        self.fleet_source = Some(Arc::new(source));
        self
    }

    /// Injects the time source for freshness and rate gating. Defaults to
    /// the wall clock; the host passes its sim-time-aware clock so sim time
    /// governs policies too.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Registers the bridge behind one tool entry of the bundle.
    pub fn with_tool(mut self, name: impl Into<String>, handler: impl ToolHandler) -> Self {
        self.handlers.insert(name.into(), Arc::new(handler));
        self
    }

    /// Registers the action bridge behind one task entry of the bundle.
    pub fn with_task(mut self, name: impl Into<String>, handler: impl TaskHandler) -> Self {
        self.task_handlers.insert(name.into(), Arc::new(handler));
        self
    }

    /// Checks the bundle and the registered handlers against each other and
    /// prepares the served catalog.
    pub fn build(self) -> Result<ExposureServer, BuildError> {
        let Self {
            bundle,
            clock,
            mut handlers,
            mut task_handlers,
            fleet_source,
        } = self;

        let mut names = HashSet::new();
        let mut resources_by_uri = HashMap::new();
        let mut resource_uri_by_name = HashMap::new();
        let mut resource_list = Vec::new();
        for entry in &bundle.resources {
            if !names.insert(entry.name.clone()) {
                return Err(BuildError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
            // A per-robot surface publishes its resources per robot, from
            // the fleet; the catalog's own URIs serve a fixed surface.
            if fleet_source.is_some() {
                continue;
            }
            resource_list.push(
                Resource::new(entry.uri.clone(), entry.name.clone())
                    .with_description(entry.description.clone())
                    .with_mime_type("application/json"),
            );
            resource_uri_by_name.insert(entry.name.clone(), entry.uri.clone());
            if resources_by_uri
                .insert(
                    entry.uri.clone(),
                    Arc::new(ResourceState::new(entry.clone())),
                )
                .is_some()
            {
                return Err(BuildError::DuplicateName {
                    name: entry.uri.clone(),
                });
            }
        }

        let mut tools = HashMap::new();
        let mut tool_list = Vec::new();
        for entry in &bundle.tools {
            if !names.insert(entry.name.clone()) {
                return Err(BuildError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
            let handler =
                handlers
                    .remove(&entry.name)
                    .ok_or_else(|| BuildError::MissingToolHandler {
                        name: entry.name.clone(),
                    })?;
            let (tool, validator) = catalog_tool(
                &entry.name,
                &entry.description,
                &entry.input_schema,
                &entry.output_schema,
                annotations_for(entry.operation),
            )?;
            tool_list.push(tool);
            tools.insert(
                entry.name.clone(),
                Arc::new(ToolState {
                    entry: entry.clone(),
                    validator,
                    handler,
                }),
            );
        }
        if let Some(name) = handlers.into_keys().next() {
            return Err(BuildError::UnknownToolHandler { name });
        }

        let mut tasks = HashMap::new();
        for entry in &bundle.tasks {
            if !names.insert(entry.name.clone()) {
                return Err(BuildError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
            let handler = task_handlers.remove(&entry.name).ok_or_else(|| {
                BuildError::MissingTaskHandler {
                    name: entry.name.clone(),
                }
            })?;
            let (tool, validator) = catalog_tool(
                &entry.name,
                &entry.description,
                &entry.input_schema,
                &entry.output_schema,
                task_annotations(entry),
            )?;
            tool_list.push(tool);
            tasks.insert(
                entry.name.clone(),
                Arc::new(TaskState {
                    entry: entry.clone(),
                    validator,
                    handler,
                }),
            );
        }
        if let Some(name) = task_handlers.into_keys().next() {
            return Err(BuildError::UnknownTaskHandler { name });
        }

        let fleet = match (&bundle.robots, fleet_source) {
            (None, None) => None,
            (None, Some(_)) => return Err(BuildError::UnexpectedFleetSource),
            (Some(_), None) => return Err(BuildError::MissingFleetSource),
            (Some(catalog), Some(source)) => {
                if !names.insert(catalog.list.name.clone()) {
                    return Err(BuildError::DuplicateName {
                        name: catalog.list.name.clone(),
                    });
                }
                let fleet = FleetRuntime::new(&bundle, catalog.clone(), source);
                tool_list.push(listing_tool(&fleet));
                Some(fleet)
            }
        };

        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(ExposureServer {
            state: Arc::new(ServerState {
                server: bundle.server,
                exposure: bundle.exposure,
                resources_by_uri,
                resource_uri_by_name,
                resource_list,
                tools,
                tasks,
                tool_list,
                manager: TaskManager::new(),
                events,
                clock,
                fleet,
            }),
        })
    }
}

/// Measures the compact serialized size of a value without materializing
/// the string.
fn serialized_len(value: &Value) -> u64 {
    struct ByteCount(u64);
    impl std::io::Write for ByteCount {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0 += buf.len() as u64;
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut sink = ByteCount(0);
    serde_json::to_writer(&mut sink, value).expect("JSON value serializes");
    sink.0
}

/// Validates one catalog entry's input schema and builds the served `Tool`
/// listing plus its compiled validator, shared by the tool and task loops.
fn catalog_tool(
    name: &str,
    description: &str,
    input_schema: &Value,
    output_schema: &Value,
    annotations: ToolAnnotations,
) -> Result<(Tool, jsonschema::Validator), BuildError> {
    let validator = jsonschema::validator_for(input_schema).map_err(|error| {
        BuildError::InvalidInputSchema {
            name: name.to_string(),
            error: error.to_string(),
        }
    })?;
    let Value::Object(input_schema) = input_schema.clone() else {
        return Err(BuildError::InvalidInputSchema {
            name: name.to_string(),
            error: "the input schema root is not an object".to_string(),
        });
    };
    let mut tool = Tool::new(
        name.to_string(),
        description.to_string(),
        Arc::new(input_schema),
    )
    .with_annotations(annotations);
    if let Value::Object(output_schema) = output_schema.clone() {
        tool = tool.with_raw_output_schema(Arc::new(output_schema));
    }
    Ok((tool, validator))
}

fn annotations_for(operation: ServiceOperation) -> ToolAnnotations {
    match operation {
        ServiceOperation::ReadOnly => ToolAnnotations::default()
            .read_only(true)
            .destructive(false),
        ServiceOperation::Mutating => ToolAnnotations::default().read_only(false),
    }
}

/// An action tool is never read-only; the exposure's `safety_sensitive`
/// marker is surfaced as the destructive hint, and an unmarked action stays
/// unhinted rather than claiming to be safe.
fn task_annotations(entry: &TaskEntry) -> ToolAnnotations {
    let annotations = ToolAnnotations::default().read_only(false);
    if entry.safety_sensitive {
        annotations.destructive(true)
    } else {
        annotations
    }
}

/// The MCP server for one exposure bundle. Cheap to clone; all clones share
/// the same snapshots, gates, subscriptions, and task handles. It is served
/// as one endpoint of an [`ExposureSet`].
#[derive(Clone)]
pub struct ExposureServer {
    state: Arc<ServerState>,
}

impl std::fmt::Debug for ExposureServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExposureServer")
            .field("exposure", &self.state.exposure.name)
            .field("tag", &self.state.exposure.tag)
            .finish_non_exhaustive()
    }
}

impl ExposureServer {
    pub fn builder(bundle: ExposureBundle) -> ExposureServerBuilder {
        ExposureServerBuilder {
            bundle,
            clock: Clock::wall(),
            handlers: HashMap::new(),
            task_handlers: HashMap::new(),
            fleet_source: None,
        }
    }

    /// The handle through which the host attaches and detaches the members
    /// of a per-robot bundle; `None` on a fixed surface.
    pub fn fleet(&self) -> Option<FleetHandle> {
        self.state.fleet.as_ref()?;
        Some(FleetHandle {
            state: Arc::clone(&self.state),
        })
    }

    /// The ingest feeding the named resource, or `None` when the bundle
    /// exposes no such resource.
    pub fn ingest(&self, resource_name: &str) -> Option<ResourceIngest> {
        let uri = self.state.resource_uri_by_name.get(resource_name)?;
        Some(ResourceIngest {
            state: Arc::clone(self.state.resources_by_uri.get(uri)?),
            events: self.state.events.clone(),
            clock: self.state.clock.clone(),
        })
    }

    /// The identity of the exposure this server serves.
    pub fn exposure(&self) -> &BundleIdentity {
        &self.state.exposure
    }

    /// The path an [`ExposureSet`] serves this exposure under, from
    /// [`BundleIdentity::endpoint_path`].
    pub fn endpoint_path(&self) -> String {
        self.state.exposure.endpoint_path()
    }

    /// The Streamable HTTP service for this exposure. Sessions are per
    /// service, so each endpoint of a set has its own; `shutdown` ends the
    /// streams it holds open.
    fn http_service(
        self,
        shutdown: &tokio_util::sync::CancellationToken,
    ) -> StreamableHttpService<Self, LocalSessionManager> {
        let config = StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_cancellation_token(shutdown.child_token());
        StreamableHttpService::new(move || Ok(self.clone()), Default::default(), config)
    }

    fn capabilities(&self) -> ServerCapabilities {
        let mut capabilities = ServerCapabilities::builder()
            .enable_resources()
            .enable_resources_subscribe()
            .enable_resources_list_changed()
            .enable_tools()
            .enable_tool_list_changed();
        // The tasks extension is advertised only when the bundle exposes
        // actions; a client probing `tasks/*` on a task-less exposure gets
        // method-not-found instead of a capability it could never use.
        if !self.state.tasks.is_empty() {
            capabilities = capabilities.enable_tasks();
        }
        capabilities.build()
    }

    /// The state behind `uri`: a fixed surface's catalog entry, or a
    /// per-robot surface's attached member resource. On a per-robot surface
    /// the fleet is read once: a resource it does not list is refused naming
    /// the robots present, and one it lists whose member the host has not
    /// attached yet reads as unavailable.
    fn resource_state(&self, uri: &str) -> Result<Arc<ResourceState>, McpError> {
        let Some(fleet) = &self.state.fleet else {
            return self
                .state
                .resources_by_uri
                .get(uri)
                .cloned()
                .ok_or_else(|| {
                    McpError::resource_not_found(
                        format!("`{uri}` is not a resource of this exposure"),
                        Some(json!({ "uri": uri })),
                    )
                });
        };
        let snapshot = fleet.fleet();
        let listed = snapshot
            .resources(&fleet.entries)
            .iter()
            .any(|resource| resource.uri == uri);
        if !listed {
            return Err(McpError::resource_not_found(
                match snapshot.robot_names().as_slice() {
                    [] => format!(
                        "`{uri}` is not a resource of this exposure, whose stack has no robot"
                    ),
                    robots => format!(
                        "`{uri}` is not a resource of this exposure; the robots are {}",
                        quoted(robots)
                    ),
                },
                Some(json!({ "uri": uri })),
            ));
        }
        match fleet.state(uri) {
            Some(state) => Ok(state),
            None => Err(McpError::internal_error(
                format!("resource `{uri}` is {UNAVAILABLE_SINCE_JOIN}"),
                None,
            )),
        }
    }

    fn read_snapshot(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let resource = self.resource_state(uri)?;
        match resource.snapshot_for_read(self.state.clock.now_nanos()) {
            Ok(view) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(view.serialized, uri).with_mime_type("application/json"),
            ])
            .with_ttl_ms(view.remaining_fresh_ms)
            .with_cache_scope(CacheScope::Private)),
            Err(ReadRefusal::Unavailable) => Err(McpError::internal_error(
                format!("resource `{uri}` is {UNAVAILABLE_SINCE_START}"),
                None,
            )),
            Err(ReadRefusal::Stale { age_ms, max_age_ms }) => Err(McpError::internal_error(
                format!(
                    "resource `{uri}` is stale: the snapshot is {age_ms} ms old and \
                     `max_age_ms` is {max_age_ms}"
                ),
                None,
            )),
        }
    }

    async fn execute_tool(
        &self,
        name: &str,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        let Some(tool) = self.state.tools.get(name) else {
            return Err(McpError::invalid_params(
                format!("`{name}` is not a tool of this exposure"),
                None,
            ));
        };
        let input = validated_input(name, &tool.validator, arguments)?;
        let call = self.route(&tool.entry.target, input)?;

        let deadline = Duration::from_millis(tool.entry.deadline_ms.get());
        let result = match tokio::time::timeout(deadline, tool.handler.call(call)).await {
            Err(_elapsed) => {
                return Ok(tool_error(format!(
                    "deadline exceeded: the provider did not answer within {} ms",
                    tool.entry.deadline_ms
                )));
            }
            Ok(Err(error)) => return Ok(tool_error(error.to_string())),
            Ok(Ok(value)) => value,
        };

        match within_result_limit(&tool.entry, result) {
            Ok(result) => Ok(CallToolResult::structured(result)),
            Err(refusal) => Ok(tool_error(refusal)),
        }
    }

    /// Runs the action behind a tool call on the surface the client can
    /// drive: an MCP task for a client that declared the tasks extension,
    /// the call itself for one that did not.
    async fn run_action(
        &self,
        task: &Arc<TaskState>,
        arguments: JsonObject,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let client_declared_tasks = context
            .client_capabilities()
            .is_some_and(|capabilities| capabilities.supports_tasks());
        if client_declared_tasks {
            return self.start_task(task, arguments).map(CallToolResponse::Task);
        }
        let progress = context
            .meta
            .get_progress_token()
            .map(|token| ProgressReporter::new(context.peer, token));
        self.run_action_in_call(task, arguments, context.ct, progress)
            .await
            .map(CallToolResponse::Complete)
    }

    /// Materializes the MCP task behind an action for a client that
    /// declared the tasks extension. The goal fields are validated first,
    /// so invalid fields never materialize a task.
    fn start_task(
        &self,
        task: &Arc<TaskState>,
        arguments: JsonObject,
    ) -> Result<CreateTaskResult, McpError> {
        let input = validated_input(&task.entry.name, &task.validator, arguments)?;
        let call = self.route(&task.entry.target, input)?;

        let task = Arc::clone(task);
        // The advertised TTL is the whole-goal deadline plus a grace window:
        // the manager's own TTL sweep is a hard stop that aborts the
        // operation and reports a generic expiry, so it has to land after
        // the deadline this runtime enforces, never race it.
        let options = TaskOptions::new().with_ttl_ms(
            task.entry
                .deadline_ms
                .get()
                .saturating_add(TASK_TTL_GRACE_MS),
        );
        let seed = self.state.manager.spawn(options, move |context| {
            Box::pin(run_task_operation(task, call, context))
        });
        Ok(CreateTaskResult::new(seed))
    }

    /// Runs the action inside the `tools/call` that started it, for a
    /// client without the tasks extension: the call answers with the
    /// goal's result once it settles, feedback is relayed through
    /// `progress` when the call carries a progress token, `cancel` firing
    /// (the client closing the call) cancels the goal, and the whole-goal
    /// deadline bounds the wait.
    ///
    /// A confirmation-gated action is refused: the confirmation is an
    /// in-task input request, so a task is the only surface that can ask
    /// for it. The refusal is what such a client has to fix, so it is
    /// reported ahead of anything its arguments could be told about.
    async fn run_action_in_call(
        &self,
        task: &Arc<TaskState>,
        arguments: JsonObject,
        cancel: tokio_util::sync::CancellationToken,
        progress: Option<ProgressReporter>,
    ) -> Result<CallToolResult, McpError> {
        if task.entry.confirmation_required {
            return Err(confirmation_needs_the_tasks_extension(&task.entry.name));
        }
        let input = validated_input(&task.entry.name, &task.validator, arguments)?;
        let call = self.route(&task.entry.target, input)?;

        let (feedback, relay) = mpsc::unbounded_channel();
        let action_context = ActionContext {
            surface: ActionSurface::Call { feedback, cancel },
        };
        let deadline = Duration::from_millis(task.entry.deadline_ms.get());
        let operation = task.handler.start(call, action_context);
        let outcome = tokio::time::timeout(
            deadline,
            relay_feedback_until_settled(operation, relay, progress),
        )
        .await;
        match outcome {
            Ok(Ok(value)) => Ok(CallToolResult::structured(value)),
            Ok(Err(exit)) => Ok(tool_error(exit.to_string())),
            Err(_elapsed) => Ok(tool_error(deadline_exceeded(deadline))),
        }
    }
}

/// The refusal for a confirmation-gated action called without the tasks
/// extension. The message names the tool and the extension: a client that
/// shows only the message, not the `requiredCapabilities` data, has to
/// learn what it lacks from there.
fn confirmation_needs_the_tasks_extension(name: &str) -> McpError {
    McpError::new(
        ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY,
        format!(
            "`{name}` asks for confirmation before it runs, which only an MCP task can carry: \
             declare the `{TASKS_EXTENSION_ID}` extension in the request's client capabilities \
             to call it"
        ),
        Some(json!({
            "requiredCapabilities": ClientCapabilities::builder().enable_tasks().build()
        })),
    )
}

/// The failure message of a goal that overran the exposure's whole-goal
/// deadline.
fn deadline_exceeded(deadline: Duration) -> String {
    format!(
        "deadline exceeded: the goal did not reach a terminal state within {} ms",
        deadline.as_millis()
    )
}

/// Relays a goal's feedback to `sink`, in the order it was reported, while
/// the goal runs. Feedback still queued when the goal settles is relayed
/// before the outcome is returned, so the result never overtakes the
/// progress that led to it.
async fn relay_feedback_until_settled(
    mut operation: Pin<Box<dyn Future<Output = Result<Value, ActionExit>> + Send>>,
    mut relay: mpsc::UnboundedReceiver<String>,
    mut sink: impl FeedbackSink,
) -> Result<Value, ActionExit> {
    let outcome = loop {
        tokio::select! {
            outcome = &mut operation => break outcome,
            Some(message) = relay.recv() => sink.report(message).await,
        }
    };
    while let Ok(message) = relay.try_recv() {
        sink.report(message).await;
    }
    outcome
}

/// Where a call relays the goal's feedback to.
trait FeedbackSink {
    async fn report(&mut self, message: String);
}

/// A call that carried no progress token has nowhere to put feedback: it
/// is dropped.
impl<S: FeedbackSink> FeedbackSink for Option<S> {
    async fn report(&mut self, message: String) {
        if let Some(sink) = self {
            sink.report(message).await;
        }
    }
}

/// Sends a goal's feedback as `notifications/progress` on the call, under
/// the call's progress token. `progress` counts the messages sent, which
/// keeps it increasing as the notification contract asks; the goal has no
/// total to report.
struct ProgressReporter {
    peer: Peer<RoleServer>,
    token: ProgressToken,
    reported: u64,
}

impl ProgressReporter {
    fn new(peer: Peer<RoleServer>, token: ProgressToken) -> Self {
        Self {
            peer,
            token,
            reported: 0,
        }
    }
}

impl FeedbackSink for ProgressReporter {
    async fn report(&mut self, message: String) {
        self.reported += 1;
        let notification = ProgressNotificationParam::new(self.token.clone(), self.reported as f64)
            .with_message(message);
        if let Err(error) = self.peer.notify_progress(notification).await {
            // The call's stream is the only route to the client; once it
            // is gone the goal is being cancelled and feedback has no
            // reader.
            tracing::debug!(%error, "dropping feedback the call can no longer deliver");
        }
    }
}

/// The exposures one process serves side by side on one listener, each at
/// its own [`ExposureServer::endpoint_path`]. Endpoints share nothing but
/// the listener:
/// every exposure keeps its own catalog, snapshots, subscriptions, and task
/// handles, so a public name, a subscription, or a task on one endpoint is
/// unknown to the others.
pub struct ExposureSet {
    servers: Vec<ExposureServer>,
}

impl std::fmt::Debug for ExposureSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExposureSet")
            .field("endpoints", &self.endpoint_paths())
            .finish()
    }
}

impl ExposureSet {
    /// Composes the set from one server per exposure. At least one is
    /// required, and two servers for the same exposure identity are refused:
    /// they would claim one path.
    pub fn new(servers: Vec<ExposureServer>) -> Result<Self, BuildError> {
        if servers.is_empty() {
            return Err(BuildError::NoExposures);
        }
        let mut paths = HashSet::new();
        for server in &servers {
            if !paths.insert(server.endpoint_path()) {
                let exposure = server.exposure();
                return Err(BuildError::DuplicateExposure {
                    name: exposure.name.clone(),
                    tag: exposure.tag.clone(),
                });
            }
        }
        Ok(Self { servers })
    }

    /// The endpoint paths the set serves, in server order.
    pub fn endpoint_paths(&self) -> Vec<String> {
        self.servers
            .iter()
            .map(ExposureServer::endpoint_path)
            .collect()
    }

    /// Serves every exposure on the listener until the token cancels. Each
    /// endpoint is exactly its path: any other path, a bare `/mcp` or a
    /// longer path under an endpoint included, answers 404. The listener
    /// decides the address; bind it to `127.0.0.1` (the design's trust
    /// boundary is the machine).
    pub async fn serve(
        self,
        listener: tokio::net::TcpListener,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> std::io::Result<()> {
        let mut router = axum::Router::new();
        let mut managers = Vec::with_capacity(self.servers.len());
        for server in self.servers {
            managers.push(server.state.manager.clone());
            let path = server.endpoint_path();
            router = router.route_service(&path, server.http_service(&shutdown));
        }
        let served = axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await;
        // Task handles live as long as the process serving them: the
        // listener going down aborts every still-running operation on every
        // endpoint instead of leaking it.
        for manager in managers {
            manager.shutdown();
        }
        served
    }
}

/// The whole task operation: the optional confirmation gate, the bridge,
/// and the exposure's whole-goal deadline around both. Enforcing the
/// deadline here (rather than leaving it to the manager's TTL sweep) makes
/// it prompt and gives the failure a descriptive message.
async fn run_task_operation(
    task: Arc<TaskState>,
    call: ToolCall,
    context: TaskContext,
) -> Result<CallToolResult, TaskExit> {
    let deadline = Duration::from_millis(task.entry.deadline_ms.get());
    match tokio::time::timeout(deadline, drive_task(task, call, context)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(TaskExit::Error(McpError::internal_error(
            deadline_exceeded(deadline),
            None,
        ))),
    }
}

/// Identifier of the confirmation entry in the task's `inputRequests`.
const CONFIRMATION_INPUT_KEY: &str = "confirmation";

async fn drive_task(
    task: Arc<TaskState>,
    call: ToolCall,
    context: TaskContext,
) -> Result<CallToolResult, TaskExit> {
    if task.entry.confirmation_required {
        // The task parks in `input_required` with this elicitation until
        // the client answers via `tasks/update`; only an explicit accept
        // lets the goal reach the provider. A decline, a cancel, a
        // malformed response, and `tasks/cancel` all settle the task as
        // `cancelled` with the goal never sent.
        let request = ElicitRequest::new(ElicitRequestParams::FormElicitationParams {
            meta: None,
            message: format!(
                "Confirm running `{}`: {}",
                task.entry.name, task.entry.description
            ),
            requested_schema: ElicitationSchema::new(Default::default()),
        });
        let response = context
            .request_input(CONFIRMATION_INPUT_KEY, InputRequest::Elicitation(request))
            .await?;
        let confirmed = serde_json::from_value::<ElicitResult>(response)
            .is_ok_and(|result| result.action == ElicitationAction::Accept);
        if !confirmed {
            return Err(TaskExit::Cancelled);
        }
    }

    let action_context = ActionContext {
        surface: ActionSurface::Task(context.clone()),
    };
    match task.handler.start(call, action_context).await {
        Ok(value) => Ok(CallToolResult::structured(value)),
        Err(ActionExit::Cancelled) => Err(TaskExit::Cancelled),
        Err(ActionExit::Failed(message)) => {
            Err(TaskExit::Error(McpError::internal_error(message, None)))
        }
    }
}

impl ServerState {
    /// Every resource entry of the bundle, for a per-robot surface to
    /// publish per robot.
    fn fleet_entries(&self) -> &[ResourceEntry] {
        self.fleet
            .as_ref()
            .map(|fleet| fleet.entries.as_slice())
            .unwrap_or_default()
    }
}

/// The listing tool of a per-robot surface: it takes nothing and answers one
/// entry per robot.
fn listing_tool(fleet: &FleetRuntime) -> Tool {
    let mut entry_properties = serde_json::Map::from_iter([
        (
            "robot".to_string(),
            json!({ "type": "string", "description": "The robot's name, which every other tool takes." }),
        ),
        (
            "capabilities".to_string(),
            json!({ "type": "array", "items": { "type": "string" }, "description": "The targets the robot fills, which say which tools and resources answer for it." }),
        ),
        (
            "members".to_string(),
            json!({ "type": "object", "additionalProperties": { "type": "array", "items": { "type": "string" } }, "description": "For each target the robot fills any number of times, the names a call addresses its members by." }),
        ),
        (
            "notes".to_string(),
            json!({ "type": "array", "items": { "type": "string" }, "description": "What could not be read or served for this robot, and why." }),
        ),
    ]);
    entry_properties.extend(fleet.describe_schema());
    let input_schema = json!({ "type": "object", "properties": {}, "additionalProperties": false });
    let output_schema = json!({
        "type": "object",
        "properties": {
            "robots": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": entry_properties,
                    "required": ["robot", "capabilities", "members", "notes"],
                },
            },
        },
        "required": ["robots"],
        "additionalProperties": false,
    });
    let Value::Object(input) = input_schema else {
        unreachable!("the listing input schema is an object");
    };
    let Value::Object(output) = output_schema else {
        unreachable!("the listing output schema is an object");
    };
    Tool::new(
        fleet.catalog.list.name.clone(),
        fleet.catalog.list.description.clone(),
        Arc::new(input),
    )
    .with_annotations(
        ToolAnnotations::default()
            .read_only(true)
            .destructive(false),
    )
    .with_raw_output_schema(Arc::new(output))
}

impl ExposureServer {
    /// Turns a validated input into the call a bridge runs: on a per-robot
    /// surface the routing arguments are taken out of the input and resolved
    /// to the member the robot fills `target` with; on a fixed surface the
    /// input is the call.
    fn route(&self, target: &str, mut input: Value) -> Result<ToolCall, McpError> {
        let Some(fleet) = &self.state.fleet else {
            return Ok(ToolCall {
                input,
                member: None,
            });
        };
        let argument = fleet.argument_of.get(target).cloned().flatten();
        let fields = input.as_object_mut().expect("validated input is an object");
        let robot = take_string(fields, &fleet.catalog.argument);
        let name = argument
            .as_deref()
            .map(|argument| take_string(fields, argument));
        let member = fleet
            .fleet()
            .route(&robot, target, name.as_deref())
            .map_err(|refusal| McpError::invalid_params(refusal.to_string(), None))?;
        Ok(ToolCall {
            input,
            member: Some(member),
        })
    }

    /// Answers the listing tool: one entry per robot with the targets it
    /// fills, its named members, and each `describe` value read through the
    /// robot's own tools and resources, every robot read at once so the
    /// listing takes one robot's describe deadlines at most.
    async fn list_robots(
        &self,
        fleet: &FleetRuntime,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        if !arguments.is_empty() {
            return Err(McpError::invalid_params(
                format!(
                    "`{}` takes no arguments; it lists every robot of the stack",
                    fleet.catalog.list.name
                ),
                None,
            ));
        }
        let snapshot = fleet.fleet();
        let entries = snapshot
            .robot_names()
            .into_iter()
            .map(|robot| self.describe_robot(fleet, &snapshot, robot));
        let robots: Vec<Value> = futures::future::join_all(entries).await;
        Ok(CallToolResult::structured(json!({ "robots": robots })))
    }

    /// One robot's listing entry with its `describe` values filled in. A
    /// value that cannot be read is `null`, with the reason under `notes`.
    async fn describe_robot(&self, fleet: &FleetRuntime, snapshot: &Fleet, robot: String) -> Value {
        let mut entry = snapshot
            .listing_entry(&robot)
            .expect("the robot was read from this snapshot");
        let mut notes: Vec<Value> = Vec::new();
        for describe in &fleet.catalog.describe {
            let value = match &describe.source {
                DescribeSource::Tool { name } => {
                    self.describe_through_tool(snapshot, &robot, &describe.target, name)
                        .await
                }
                DescribeSource::Resource { name, fields } => self.describe_through_resource(
                    fleet,
                    snapshot,
                    &robot,
                    &describe.target,
                    name,
                    fields,
                ),
            };
            match value {
                Ok(value) => {
                    entry[&describe.key] = value;
                }
                Err(reason) => {
                    entry[&describe.key] = Value::Null;
                    notes.push(json!(format!("{}: {reason}", describe.key)));
                }
            }
        }
        if let Some(existing) = entry["notes"].as_array_mut() {
            existing.extend(notes);
        }
        entry
    }

    async fn describe_through_tool(
        &self,
        snapshot: &Fleet,
        robot: &str,
        target: &str,
        tool_name: &str,
    ) -> Result<Value, String> {
        let tool = self
            .state
            .tools
            .get(tool_name)
            .expect("the catalog names a tool of this bundle");
        let member = snapshot
            .route(robot, target, None)
            .map_err(|refusal| refusal.to_string())?;
        let call = ToolCall {
            input: json!({}),
            member: Some(member),
        };
        let deadline = Duration::from_millis(tool.entry.deadline_ms.get());
        match tokio::time::timeout(deadline, tool.handler.call(call)).await {
            Ok(Ok(value)) => within_result_limit(&tool.entry, value),
            Ok(Err(error)) => Err(error.to_string()),
            Err(_elapsed) => Err(format!(
                "deadline exceeded: the provider did not answer within {} ms",
                tool.entry.deadline_ms
            )),
        }
    }

    fn describe_through_resource(
        &self,
        fleet: &FleetRuntime,
        snapshot: &Fleet,
        robot: &str,
        target: &str,
        resource_name: &str,
        fields: &[String],
    ) -> Result<Value, String> {
        let entry = fleet
            .entries
            .iter()
            .find(|entry| entry.name == resource_name)
            .expect("the catalog names a resource of this bundle");
        snapshot
            .route(robot, target, None)
            .map_err(|refusal| refusal.to_string())?;
        let uri = published_uri(&published_name(entry, robot, None));
        let state = fleet
            .state(&uri)
            .ok_or_else(|| UNAVAILABLE_SINCE_JOIN.to_string())?;
        let view = state
            .snapshot_for_read(self.state.clock.now_nanos())
            .map_err(|refusal| read_refusal_text(&refusal, UNAVAILABLE_SINCE_JOIN))?;
        let snapshot: Value = serde_json::from_str(&view.serialized)
            .expect("a stored snapshot is the JSON it was serialized from");
        Ok(Value::Object(
            fields
                .iter()
                .map(|field| (field.clone(), snapshot[field].clone()))
                .collect(),
        ))
    }
}

/// Takes the string field `name` out of a validated object; the schema
/// made it required and a string.
fn take_string(fields: &mut JsonObject, name: &str) -> String {
    match fields.remove(name) {
        Some(Value::String(value)) => value,
        _ => unreachable!("the input schema requires `{name}` as a string"),
    }
}

/// A tool result held to the entry's `max_result_bytes`.
fn within_result_limit(entry: &ToolEntry, result: Value) -> Result<Value, String> {
    if let Some(limit) = entry.max_result_bytes {
        let size = serialized_len(&result);
        if size > limit.get() {
            return Err(format!(
                "result of {size} bytes exceeds the {} byte limit",
                limit.get()
            ));
        }
    }
    Ok(result)
}

/// Why a snapshot cannot be read, `unavailable` saying since when nothing
/// was published.
fn read_refusal_text(refusal: &ReadRefusal, unavailable: &str) -> String {
    match refusal {
        ReadRefusal::Unavailable => unavailable.to_string(),
        ReadRefusal::Stale { age_ms, max_age_ms } => {
            format!("stale: the snapshot is {age_ms} ms old and `max_age_ms` is {max_age_ms}")
        }
    }
}

/// How the host keeps a per-robot server's resources in step with the
/// members that run: it attaches each member it starts feeding, detaches
/// each one that leaves, and says when the list changed.
#[derive(Clone)]
pub struct FleetHandle {
    state: Arc<ServerState>,
}

impl FleetHandle {
    fn runtime(&self) -> &FleetRuntime {
        self.state
            .fleet
            .as_ref()
            .expect("a fleet handle exists on a per-robot server alone")
    }

    /// Registers `member`'s resources and hands back the ingest feeding each
    /// one, with the catalog entry it publishes.
    pub fn attach(&self, member: &FleetMember) -> Vec<(ResourceEntry, ResourceIngest)> {
        self.runtime()
            .attach(member)
            .into_iter()
            .map(|(entry, state)| {
                (
                    entry,
                    ResourceIngest {
                        state,
                        events: self.state.events.clone(),
                        clock: self.state.clock.clone(),
                    },
                )
            })
            .collect()
    }

    /// Drops `member`'s resources.
    pub fn detach(&self, member: &FleetMember) {
        self.runtime().detach(member);
    }

    /// Tells listening clients the resource list changed.
    pub fn changed(&self) {
        // Send fails only when nobody listens, which is fine.
        let _ = self.state.events.send(CatalogEvent::ResourceListChanged);
    }

    /// The members the surface cannot serve as the fleet stands now, each
    /// with the reason.
    pub fn problems(&self) -> Vec<String> {
        self.runtime().fleet().problems().to_vec()
    }
}

/// Validates tool-call arguments against a compiled derived schema; nothing
/// invalid ever reaches a bridge or materializes a task.
fn validated_input(
    name: &str,
    validator: &jsonschema::Validator,
    arguments: JsonObject,
) -> Result<Value, McpError> {
    let input = Value::Object(arguments);
    let problems: Vec<String> = validator
        .iter_errors(&input)
        .map(|error| {
            let path = error.instance_path().to_string();
            if path.is_empty() {
                error.to_string()
            } else {
                format!("{path}: {error}")
            }
        })
        .collect();
    if !problems.is_empty() {
        return Err(McpError::invalid_params(
            format!("invalid arguments for `{name}`: {}", problems.join("; ")),
            None,
        ));
    }
    Ok(input)
}

fn tool_error(message: String) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(message)])
}

impl ServerHandler for ExposureServer {
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }

    fn get_info(&self) -> ServerInfo {
        let implementation = Implementation::new(
            self.state.exposure.name.clone(),
            self.state.exposure.tag.clone(),
        )
        .with_title(self.state.server.title.clone());
        let mut info = ServerInfo::new(self.capabilities())
            .with_server_info(implementation)
            .with_protocol_version(ProtocolVersion::V_2026_07_28);
        if let Some(instructions) = &self.state.server.instructions {
            info = info.with_instructions(instructions.clone());
        }
        info
    }

    async fn discover(
        &self,
        _context: RequestContext<RoleServer>,
    ) -> Result<DiscoverResult, McpError> {
        Ok(DiscoverResult::from_server_info(
            self.supported_protocol_versions().into_owned(),
            self.get_info(),
        )
        .with_ttl_ms(CATALOG_TTL_MS)
        .with_cache_scope(CacheScope::Private))
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        // A per-robot surface's resources follow its robots, so the list
        // is read from the fleet on every request and carries no cache hint.
        let Some(fleet) = &self.state.fleet else {
            return Ok(
                ListResourcesResult::with_all_items(self.state.resource_list.clone())
                    .with_ttl_ms(CATALOG_TTL_MS)
                    .with_cache_scope(CacheScope::Private),
            );
        };
        Ok(ListResourcesResult::with_all_items(
            fleet.fleet().resources(self.state.fleet_entries()),
        ))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.read_snapshot(&request.uri).map(Into::into)
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(
            ListToolsResult::with_all_items(self.state.tool_list.clone())
                .with_ttl_ms(CATALOG_TTL_MS)
                .with_cache_scope(CacheScope::Private),
        )
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.state
            .tool_list
            .iter()
            .find(|tool| tool.name.as_ref() == name)
            .cloned()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let name = request.name.as_ref();
        let arguments = request.arguments.unwrap_or_default();
        if let Some(fleet) = &self.state.fleet
            && name == fleet.catalog.list.name
        {
            return self.list_robots(fleet, arguments).await.map(Into::into);
        }
        let Some(task) = self.state.tasks.get(name) else {
            return self.execute_tool(name, arguments).await.map(Into::into);
        };
        self.run_action(task, arguments, context).await
    }

    async fn get_task(
        &self,
        request: GetTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<GetTaskResult, McpError> {
        self.state
            .manager
            .get_task(&request.task_id)
            .map(GetTaskResult::new)
    }

    async fn update_task(
        &self,
        request: UpdateTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.state
            .manager
            .update_task(&request.task_id, request.input_responses)
    }

    async fn cancel_task(
        &self,
        request: CancelTaskParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<(), McpError> {
        self.state.manager.cancel_task(&request.task_id)
    }

    fn accepted_subscription_filter(
        &self,
        requested: &SubscriptionFilter,
    ) -> Option<SubscriptionFilter> {
        Some(requested.supported_by(&self.capabilities()))
    }

    async fn listen(&self, context: SubscriptionContext) -> Result<(), McpError> {
        let sink = context.sink().clone();
        let mut events = self.state.events.subscribe();
        loop {
            tokio::select! {
                _ = context.cancelled() => return Ok(()),
                event = events.recv() => match event {
                    Ok(event) => {
                        let sent = match event {
                            CatalogEvent::ResourceUpdated { uri } => {
                                sink.notify_resource_updated(uri).await
                            }
                            CatalogEvent::ResourceListChanged => {
                                sink.notify_resource_list_changed().await
                            }
                        };
                        match sent {
                            Ok(()) => {}
                            Err(
                                SubscriptionSendError::SubscriptionClosed
                                | SubscriptionSendError::Service(_),
                            ) => return Ok(()),
                            // The subscription's accepted filter does not
                            // cover this notification; other listeners may
                            // still want it.
                            Err(_) => {}
                        }
                    }
                    // Latest-snapshot semantics: a lagged listener re-reads
                    // and loses nothing.
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return Ok(()),
                },
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clock::test_support::manual_clock;
    use rmcp::model::ErrorCode;
    use std::sync::atomic::Ordering;

    fn test_bundle() -> ExposureBundle {
        ExposureBundle::from_json_str(
            r#"{
  "bundle_format": 1,
  "schema_mapping_version": 1,
  "exposure": { "name": "camera_and_recording", "tag": "v1" },
  "server": {
    "title": "OpenArm camera",
    "instructions": "Observe the front camera."
  },
  "contracts": [
    { "name": "rgb_camera", "tag": "v1", "sha256": "aa", "link_id": "front_camera" }
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
      "schema": { "type": "object" }
    }
  ],
  "tools": [
    {
      "name": "front_camera.set_brightness",
      "description": "Set the camera brightness in device units.",
      "target": "front_camera",
      "member": "set_brightness",
      "operation": "mutating",
      "deadline_ms": 2000,
      "max_result_bytes": 64,
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
  "tasks": []
}"#,
        )
        .expect("test bundle parses")
    }

    async fn brightness_handler(call: ToolCall) -> Result<Value, ToolCallError> {
        let value = call.input["value"].as_i64().expect("validated integer");
        Ok(json!({ "applied": value >= 0 }))
    }

    fn built_server() -> ExposureServer {
        ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .build()
            .expect("bundle and handlers agree")
    }

    fn arguments(raw: Value) -> JsonObject {
        match raw {
            Value::Object(map) => map,
            other => panic!("arguments must be an object, got {other}"),
        }
    }

    /// `test_bundle` plus two exposed actions: `record_episode` requires
    /// confirmation and is safety-sensitive, `resume_session` is neither.
    fn task_bundle() -> ExposureBundle {
        let mut bundle = test_bundle();
        bundle.tasks = vec![
            serde_json::from_value(json!({
                "name": "recorder.record_episode",
                "description": "Record one teleoperation episode.",
                "target": "recorder",
                "member": "record_episode",
                "operation": "long_running",
                "safety_sensitive": true,
                "confirmation_required": true,
                "deadline_ms": 900000,
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
                }
            }))
            .expect("valid task entry"),
            serde_json::from_value(json!({
                "name": "recorder.resume_session",
                "description": "Resume the recording session.",
                "target": "recorder",
                "member": "resume_session",
                "operation": "long_running",
                "safety_sensitive": false,
                "confirmation_required": false,
                "deadline_ms": 2000,
                "input_schema": {
                    "type": "object",
                    "additionalProperties": false
                },
                "output_schema": {
                    "type": "object",
                    "properties": { "resumed": { "type": "boolean" } },
                    "required": ["resumed"],
                    "additionalProperties": false
                }
            }))
            .expect("valid task entry"),
        ];
        bundle
    }

    async fn record_handler(
        _call: ToolCall,
        _context: crate::tasks::ActionContext,
    ) -> Result<Value, ActionExit> {
        Ok(json!({ "frames": 120 }))
    }

    async fn resume_handler(
        _call: ToolCall,
        _context: crate::tasks::ActionContext,
    ) -> Result<Value, ActionExit> {
        Ok(json!({ "resumed": true }))
    }

    fn built_task_server() -> ExposureServer {
        ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task("recorder.resume_session", resume_handler)
            .build()
            .expect("bundle and handlers agree")
    }

    /// Yield-driven wait for a task state; every iteration hands the
    /// scheduler to the spawned operation, so this depends on scheduling
    /// alone, never on host time.
    async fn task_matching(
        server: &ExposureServer,
        task_id: &str,
        description: &str,
        accept: impl Fn(&rmcp::model::DetailedTask) -> bool,
    ) -> rmcp::model::DetailedTask {
        for _ in 0..100_000 {
            let task = server.state.manager.get_task(task_id).expect("task exists");
            if accept(&task) {
                return task;
            }
            tokio::task::yield_now().await;
        }
        panic!("task `{task_id}` never reached: {description}");
    }

    async fn settled(server: &ExposureServer, task_id: &str) -> rmcp::model::DetailedTask {
        task_matching(server, task_id, "a terminal status", |task| {
            task.status().is_terminal()
        })
        .await
    }

    fn task_named(server: &ExposureServer, name: &str) -> Arc<TaskState> {
        Arc::clone(
            server
                .state
                .tasks
                .get(name)
                .expect("the bundle exposes the task"),
        )
    }

    fn start_task(
        server: &ExposureServer,
        name: &str,
        arguments: JsonObject,
    ) -> Result<CreateTaskResult, McpError> {
        server.start_task(&task_named(server, name), arguments)
    }

    /// Runs the action in a call that carries no progress token, on a
    /// cancellation token the test holds.
    async fn run_in_call(
        server: &ExposureServer,
        name: &str,
        arguments: JsonObject,
        cancel: tokio_util::sync::CancellationToken,
    ) -> Result<CallToolResult, McpError> {
        server
            .run_action_in_call(&task_named(server, name), arguments, cancel, None)
            .await
    }

    /// Stands in for the call's progress notifications: every relayed
    /// message lands on the channel in the order the relay handed it over.
    impl FeedbackSink for mpsc::UnboundedSender<String> {
        async fn report(&mut self, message: String) {
            self.send(message).expect("the test holds the receiver");
        }
    }

    fn tool_error_text(result: &CallToolResult) -> &str {
        assert_eq!(
            result.is_error,
            Some(true),
            "expected a tool error, got {result:?}"
        );
        match result.content.first() {
            Some(ContentBlock::Text(text)) => &text.text,
            other => panic!("expected a text block, got {other:?}"),
        }
    }

    #[test]
    fn a_task_without_a_handler_is_refused() {
        let error = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .build()
            .expect_err("resume_session has no handler");
        assert_eq!(
            error,
            BuildError::MissingTaskHandler {
                name: "recorder.resume_session".to_string()
            }
        );
    }

    #[test]
    fn a_task_handler_without_a_task_is_refused() {
        let error = ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .build()
            .expect_err("the plain bundle exposes no tasks");
        assert_eq!(
            error,
            BuildError::UnknownTaskHandler {
                name: "recorder.record_episode".to_string()
            }
        );
    }

    #[test]
    fn task_tools_join_the_catalog_with_annotations() {
        let server = built_task_server();
        let record = server
            .get_tool("recorder.record_episode")
            .expect("task tools are listed");
        assert!(record.output_schema.is_some());
        let annotations = record.annotations.as_ref().expect("annotations set");
        assert_eq!(annotations.read_only_hint, Some(false));
        assert_eq!(
            annotations.destructive_hint,
            Some(true),
            "safety_sensitive surfaces as the destructive hint"
        );
        let resume = server
            .get_tool("recorder.resume_session")
            .expect("task tools are listed");
        let annotations = resume.annotations.as_ref().expect("annotations set");
        assert_eq!(
            annotations.destructive_hint, None,
            "an unmarked action stays unhinted"
        );
    }

    #[test]
    fn the_tasks_capability_tracks_the_bundle() {
        assert!(built_task_server().capabilities().supports_tasks());
        assert!(!built_server().capabilities().supports_tasks());
    }

    #[tokio::test]
    async fn without_the_tasks_capability_an_action_runs_inside_the_call() {
        let server = built_task_server();
        let result = run_in_call(
            &server,
            "recorder.resume_session",
            JsonObject::new(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("the call answers with the goal's result");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(json!({ "resumed": true })));
        assert_eq!(
            server.state.manager.running_task_count(),
            0,
            "no task materializes for a client that cannot poll one"
        );
    }

    #[tokio::test]
    async fn a_confirmation_gated_action_without_the_tasks_capability_is_refused_naming_the_extension()
     {
        let server = built_task_server();
        // The capability is the client's real blocker, so it is reported
        // ahead of anything its arguments could be told about.
        let error = run_in_call(
            &server,
            "recorder.record_episode",
            arguments(json!({ "episode_name": 7 })),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("the confirmation needs a task");
        assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
        assert!(
            error.message.contains("`recorder.record_episode`")
                && error.message.contains(TASKS_EXTENSION_ID),
            "the message names the tool and the extension: {}",
            error.message
        );
        assert_eq!(
            error.data,
            Some(json!({
                "requiredCapabilities": { "extensions": { TASKS_EXTENSION_ID: {} } }
            })),
            "the data names the capability the way the protocol asks"
        );
        assert_eq!(server.state.manager.running_task_count(), 0);
    }

    #[tokio::test]
    async fn invalid_goal_arguments_never_run_inside_a_call() {
        let server = built_task_server();
        let error = run_in_call(
            &server,
            "recorder.resume_session",
            arguments(json!({ "extra": true })),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect_err("the goal fields fail the derived schema");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
    }

    #[tokio::test]
    async fn a_bridge_failure_inside_a_call_is_the_calls_tool_error() {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    Err::<Value, _>(ActionExit::Failed(
                        "the provider abandoned the goal".to_string(),
                    ))
                },
            )
            .build()
            .expect("builds");
        let result = run_in_call(
            &server,
            "recorder.resume_session",
            JsonObject::new(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("a failed goal is a tool error, not a protocol error");
        assert_eq!(
            tool_error_text(&result),
            "the action failed: the provider abandoned the goal"
        );
    }

    #[tokio::test]
    async fn closing_the_call_cancels_the_goal_and_the_cancelled_exit_is_a_tool_error() {
        let cancel_seen = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&cancel_seen);
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                move |_call: ToolCall, context: crate::tasks::ActionContext| {
                    let cancel_seen = Arc::clone(&observed);
                    async move {
                        context.cancel_requested().await;
                        assert!(context.is_cancel_requested());
                        cancel_seen.store(true, Ordering::SeqCst);
                        Err(ActionExit::Cancelled)
                    }
                },
            )
            .build()
            .expect("builds");
        let cancel = tokio_util::sync::CancellationToken::new();
        // Cancelled up front: the goal observes it on its first look, so
        // the outcome depends on nothing but the token.
        cancel.cancel();
        let result = run_in_call(
            &server,
            "recorder.resume_session",
            JsonObject::new(),
            cancel,
        )
        .await
        .expect("a cancelled goal is a tool error, not a protocol error");
        assert!(cancel_seen.load(Ordering::SeqCst));
        assert_eq!(tool_error_text(&result), "the action was cancelled");
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_fails_a_goal_inside_a_call_with_a_descriptive_error() {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    std::future::pending::<Result<Value, ActionExit>>().await
                },
            )
            .build()
            .expect("builds");
        // Paused time auto-advances past the 2000 ms deadline once the
        // goal is the only thing pending.
        let result = run_in_call(
            &server,
            "recorder.resume_session",
            JsonObject::new(),
            tokio_util::sync::CancellationToken::new(),
        )
        .await
        .expect("an overrun is a tool error, not a protocol error");
        assert_eq!(
            tool_error_text(&result),
            "deadline exceeded: the goal did not reach a terminal state within 2000 ms"
        );
    }

    #[tokio::test]
    async fn feedback_inside_a_call_is_relayed_in_order_before_the_result() {
        let (relayed, mut received) = mpsc::unbounded_channel();
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, context: crate::tasks::ActionContext| async move {
                    context.report_feedback("frame 1");
                    context.report_feedback("frame 2");
                    Ok(json!({ "resumed": true }))
                },
            )
            .build()
            .expect("builds");
        let task = task_named(&server, "recorder.resume_session");
        let (feedback, relay) = mpsc::unbounded_channel();
        let context = ActionContext {
            surface: ActionSurface::Call {
                feedback,
                cancel: tokio_util::sync::CancellationToken::new(),
            },
        };
        let operation = task.handler.start(
            ToolCall {
                input: JsonObject::new().into(),
                member: None,
            },
            context,
        );
        let outcome = relay_feedback_until_settled(operation, relay, Some(relayed))
            .await
            .expect("the goal completes");
        assert_eq!(outcome, json!({ "resumed": true }));
        let mut messages = Vec::new();
        while let Ok(message) = received.try_recv() {
            messages.push(message);
        }
        assert_eq!(messages, ["frame 1", "frame 2"]);
    }

    #[tokio::test]
    async fn invalid_goal_arguments_never_materialize_a_task() {
        let server = built_task_server();
        let error = start_task(
            &server,
            "recorder.record_episode",
            arguments(json!({ "episode_name": 7 })),
        )
        .expect_err("the goal fields fail the derived schema");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(server.state.manager.running_task_count(), 0);
    }

    #[tokio::test]
    async fn a_task_completes_with_the_bridge_result() {
        let server = built_task_server();
        let created = start_task(&server, "recorder.resume_session", JsonObject::new())
            .expect("the task starts");
        assert_eq!(
            created.task.ttl_ms,
            Some(2000 + TASK_TTL_GRACE_MS),
            "the advertised TTL clears the 2000 ms whole-goal deadline"
        );
        let task = settled(&server, &created.task.task_id).await;
        let rmcp::model::TaskPayload::Completed { result } = task.payload else {
            panic!("expected a completed task, got {:?}", task.payload);
        };
        assert_eq!(result["structuredContent"], json!({ "resumed": true }));
    }

    #[tokio::test]
    async fn feedback_reports_as_the_status_message_and_cancel_settles_cancelled() {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, context: crate::tasks::ActionContext| async move {
                    context.report_feedback("resuming at frame 42");
                    context.cancel_requested().await;
                    Err(ActionExit::Cancelled)
                },
            )
            .build()
            .expect("builds");
        let created = start_task(&server, "recorder.resume_session", JsonObject::new())
            .expect("the task starts");
        let task_id = created.task.task_id;
        let task = task_matching(&server, &task_id, "the feedback status message", |task| {
            task.task.status_message.as_deref() == Some("resuming at frame 42")
        })
        .await;
        assert!(!task.status().is_terminal());

        server.state.manager.cancel_task(&task_id).expect("cancels");
        let task = settled(&server, &task_id).await;
        assert_eq!(task.status(), rmcp::model::TaskStatus::Cancelled);
    }

    #[tokio::test]
    async fn a_failed_bridge_settles_the_task_as_failed() {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    Err::<Value, _>(ActionExit::Failed(
                        "the provider abandoned the goal".to_string(),
                    ))
                },
            )
            .build()
            .expect("builds");
        let created = start_task(&server, "recorder.resume_session", JsonObject::new())
            .expect("the task starts");
        let task = settled(&server, &created.task.task_id).await;
        let rmcp::model::TaskPayload::Failed { error } = task.payload else {
            panic!("expected a failed task, got {:?}", task.payload);
        };
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|message| message.contains("the provider abandoned the goal")),
            "got {error:?}"
        );
    }

    #[tokio::test]
    async fn confirmation_parks_the_task_and_accept_releases_the_goal() {
        let goal_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&goal_ran);
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task(
                "recorder.record_episode",
                move |_call: ToolCall, _context: crate::tasks::ActionContext| {
                    let goal_ran = Arc::clone(&observed);
                    async move {
                        goal_ran.store(true, Ordering::SeqCst);
                        Ok(json!({ "frames": 120 }))
                    }
                },
            )
            .with_task("recorder.resume_session", resume_handler)
            .build()
            .expect("builds");
        let created = start_task(
            &server,
            "recorder.record_episode",
            arguments(json!({ "episode_name": "pick_and_place" })),
        )
        .expect("the task starts");
        let task_id = created.task.task_id;

        let task = task_matching(&server, &task_id, "input_required", |task| {
            task.status() == rmcp::model::TaskStatus::InputRequired
        })
        .await;
        let rmcp::model::TaskPayload::InputRequired { input_requests } = task.payload else {
            panic!("expected input_required, got {:?}", task.payload);
        };
        assert!(
            input_requests.contains_key(CONFIRMATION_INPUT_KEY),
            "the confirmation elicitation is outstanding"
        );
        assert!(
            !goal_ran.load(Ordering::SeqCst),
            "the goal must not run before the confirmation"
        );

        server
            .state
            .manager
            .update_task(
                &task_id,
                [(
                    CONFIRMATION_INPUT_KEY.to_string(),
                    json!({ "action": "accept" }),
                )],
            )
            .expect("the confirmation is delivered");
        let task = settled(&server, &task_id).await;
        assert_eq!(task.status(), rmcp::model::TaskStatus::Completed);
        assert!(goal_ran.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_declined_confirmation_cancels_the_task_without_running_the_goal() {
        let goal_ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let observed = Arc::clone(&goal_ran);
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task(
                "recorder.record_episode",
                move |_call: ToolCall, _context: crate::tasks::ActionContext| {
                    let goal_ran = Arc::clone(&observed);
                    async move {
                        goal_ran.store(true, Ordering::SeqCst);
                        Ok(json!({ "frames": 120 }))
                    }
                },
            )
            .with_task("recorder.resume_session", resume_handler)
            .build()
            .expect("builds");
        let created = start_task(
            &server,
            "recorder.record_episode",
            arguments(json!({ "episode_name": "pick_and_place" })),
        )
        .expect("the task starts");
        let task_id = created.task.task_id;
        task_matching(&server, &task_id, "input_required", |task| {
            task.status() == rmcp::model::TaskStatus::InputRequired
        })
        .await;

        server
            .state
            .manager
            .update_task(
                &task_id,
                [(
                    CONFIRMATION_INPUT_KEY.to_string(),
                    json!({ "action": "decline" }),
                )],
            )
            .expect("the response is delivered");
        let task = settled(&server, &task_id).await;
        assert_eq!(task.status(), rmcp::model::TaskStatus::Cancelled);
        assert!(
            !goal_ran.load(Ordering::SeqCst),
            "a declined goal never reaches the provider"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn the_deadline_fails_the_task_with_a_descriptive_error() {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    std::future::pending::<Result<Value, ActionExit>>().await
                },
            )
            .build()
            .expect("builds");
        let created = start_task(&server, "recorder.resume_session", JsonObject::new())
            .expect("the task starts");
        // Paused time: this yields to the spawned operation (registering
        // its 2000 ms deadline timer), then auto-advances past it.
        tokio::time::sleep(Duration::from_millis(2001)).await;
        let task = server
            .state
            .manager
            .get_task(&created.task.task_id)
            .expect("task exists");
        let rmcp::model::TaskPayload::Failed { error } = task.payload else {
            panic!("expected a failed task, got {:?}", task.payload);
        };
        assert!(
            error["message"].as_str().is_some_and(|message| {
                message.contains("deadline exceeded") && message.contains("2000 ms")
            }),
            "got {error:?}"
        );
    }

    #[test]
    fn the_endpoint_path_carries_the_exposure_identity() {
        let server = built_server();
        assert_eq!(server.endpoint_path(), "/camera_and_recording/v1/mcp");
        assert_eq!(server.exposure().name, "camera_and_recording");
    }

    /// The per-robot surface the fleet tests drive: a status target filled
    /// once per robot (a resource the listing reads a field of, and an
    /// identity tool it calls) and a camera target filled any number of
    /// times.
    mod per_robot {
        use super::*;
        use crate::fleet::{FleetMember, MemberAddress};
        use std::sync::Mutex;

        fn bundle() -> ExposureBundle {
            ExposureBundle::from_json_str(
                r#"{
  "bundle_format": 1,
  "schema_mapping_version": 1,
  "exposure": { "name": "robot_control", "tag": "v1" },
  "server": { "title": "Robots" },
  "robots": {
    "argument": "robot",
    "list": { "name": "robot.list", "description": "The robots of the stack." },
    "describe": [
      { "key": "identity", "target": "status", "source": { "tool": { "name": "robot.get_identity" } } },
      { "key": "state", "target": "status", "source": { "resource": { "name": "robot.status", "fields": ["battery"] } } }
    ]
  },
  "contracts": [
    { "name": "robot_status", "tag": "v1", "sha256": "aa", "link_id": "status" },
    { "name": "rgb_camera", "tag": "v1", "sha256": "bb", "link_id": "camera", "argument": "camera" }
  ],
  "resources": [
    {
      "name": "robot.status",
      "uri": "peppy://resource/robot.status",
      "description": "The robot's latest status.",
      "target": "status",
      "member": "status",
      "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 2.0 } },
      "schema": { "type": "object" }
    },
    {
      "name": "camera.latest_frame",
      "uri": "peppy://resource/camera.latest_frame",
      "description": "The camera's latest frame.",
      "target": "camera",
      "member": "video_stream",
      "policies": { "freshness": { "max_age_ms": 2000 }, "update": { "max_hz": 2.0 } },
      "schema": { "type": "object" }
    }
  ],
  "tools": [
    {
      "name": "robot.get_identity",
      "description": "Who the robot is.",
      "target": "status",
      "member": "get_identity",
      "operation": "read_only",
      "deadline_ms": 2000,
      "input_schema": {
        "type": "object",
        "properties": { "robot": { "type": "string" } },
        "required": ["robot"],
        "additionalProperties": false
      },
      "output_schema": { "type": "object" }
    },
    {
      "name": "camera.set_brightness",
      "description": "Set the camera's brightness.",
      "target": "camera",
      "member": "set_brightness",
      "operation": "mutating",
      "deadline_ms": 2000,
      "input_schema": {
        "type": "object",
        "properties": {
          "value": { "type": "integer" },
          "robot": { "type": "string" },
          "camera": { "type": "string" }
        },
        "required": ["value", "robot", "camera"],
        "additionalProperties": false
      },
      "output_schema": { "type": "object" }
    }
  ],
  "tasks": []
}"#,
            )
            .expect("per-robot bundle parses")
        }

        fn member(target: &str, robot: &str, name: &str) -> FleetMember {
            FleetMember {
                target: target.to_string(),
                address: MemberAddress {
                    core_node: "cn".to_string(),
                    instance_id: format!("{robot}_{name}"),
                },
                robot: Some(robot.to_string()),
                name: name.to_string(),
            }
        }

        /// A server over a fleet the test edits, whose tools answer with the
        /// member they were routed to.
        fn served() -> (ExposureServer, Arc<Mutex<Vec<FleetMember>>>) {
            let fleet = Arc::new(Mutex::new(vec![
                member("status", "alpha", "backbone_inst"),
                member("camera", "alpha", "wrist_left"),
                member("status", "bravo", "backbone_inst"),
            ]));
            let source = Arc::clone(&fleet);
            let server = ExposureServer::builder(bundle())
                .with_fleet(move || source.lock().unwrap().clone())
                .with_tool("robot.get_identity", |call: ToolCall| async move {
                    let member = call.member.expect("routed");
                    Ok(json!({ "robot": member.instance_id, "input": call.input }))
                })
                .with_tool("camera.set_brightness", |call: ToolCall| async move {
                    let member = call.member.expect("routed");
                    Ok(json!({ "camera": member.instance_id, "input": call.input }))
                })
                .build()
                .expect("bundle and handlers agree");
            (server, fleet)
        }

        fn structured(result: CallToolResult) -> Value {
            result.structured_content.expect("a structured result")
        }

        #[test]
        fn a_per_robot_bundle_needs_its_fleet_and_a_fixed_bundle_takes_none() {
            let missing = ExposureServer::builder(bundle())
                .with_tool("robot.get_identity", brightness_handler)
                .with_tool("camera.set_brightness", brightness_handler)
                .build()
                .expect_err("no fleet source");
            assert_eq!(missing, BuildError::MissingFleetSource);
            let unexpected = ExposureServer::builder(test_bundle())
                .with_tool("front_camera.set_brightness", brightness_handler)
                .with_fleet(Vec::new)
                .build()
                .expect_err("a fixed bundle has no fleet");
            assert_eq!(unexpected, BuildError::UnexpectedFleetSource);
        }

        #[tokio::test]
        async fn a_call_is_routed_to_the_robots_member_without_its_routing_arguments() {
            let (server, _) = served();
            let result = server
                .execute_tool(
                    "camera.set_brightness",
                    arguments(json!({ "robot": "alpha", "camera": "wrist_left", "value": 3 })),
                )
                .await
                .expect("routes");
            assert_eq!(
                structured(result),
                json!({ "camera": "alpha_wrist_left", "input": { "value": 3 } })
            );
            let result = server
                .execute_tool("robot.get_identity", arguments(json!({ "robot": "bravo" })))
                .await
                .expect("routes");
            assert_eq!(
                structured(result),
                json!({ "robot": "bravo_backbone_inst", "input": {} })
            );
        }

        #[tokio::test]
        async fn refusals_name_the_robots_and_members_present() {
            let (server, fleet) = served();
            let refused = |arguments: Value| async {
                server
                    .execute_tool("camera.set_brightness", self::arguments(arguments))
                    .await
                    .expect_err("refused")
            };
            let error =
                refused(json!({ "robot": "charlie", "camera": "wrist_left", "value": 1 })).await;
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error.message,
                "`charlie` is not a robot of this stack; the robots are `alpha`, `bravo`"
            );
            let error =
                refused(json!({ "robot": "bravo", "camera": "wrist_left", "value": 1 })).await;
            assert_eq!(
                error.message,
                "robot `bravo` has no `camera`; it fills `status`; the robots with a `camera` are `alpha`"
            );
            let error = refused(json!({ "robot": "alpha", "camera": "chest", "value": 1 })).await;
            assert_eq!(
                error.message,
                "robot `alpha` has no `camera` named `chest` (`camera`); it has `wrist_left`"
            );
            let error = refused(json!({ "robot": "alpha", "value": 1 })).await;
            assert!(
                error.message.contains("invalid arguments"),
                "{}",
                error.message
            );

            fleet.lock().unwrap().clear();
            let error =
                refused(json!({ "robot": "alpha", "camera": "wrist_left", "value": 1 })).await;
            assert_eq!(
                error.message,
                "`alpha` is not a robot of this stack, which has no robot; `peppy stack join OPTION -i \
                 NAME` adds one"
            );
        }

        #[tokio::test]
        async fn the_listing_reports_each_robot_with_what_its_targets_answer() {
            let (server, fleet) = served();
            let handle = server.fleet().expect("a per-robot server");
            let attached = handle.attach(&member("status", "alpha", "backbone_inst"));
            assert_eq!(attached.len(), 1);
            let (entry, ingest) = &attached[0];
            assert_eq!(entry.name, "robot.status");
            let token = ingest.admit().expect("gate open");
            ingest
                .publish(token, json!({ "battery": 87, "mode": "idle" }))
                .expect("publishes");

            let listed = server
                .list_robots(server.state.fleet.as_ref().unwrap(), JsonObject::new())
                .await
                .expect("lists");
            assert_eq!(
                structured(listed),
                json!({
                    "robots": [
                        {
                            "robot": "alpha",
                            "capabilities": ["status", "camera"],
                            "members": { "camera": ["wrist_left"] },
                            "notes": [],
                            "identity": { "robot": "alpha_backbone_inst", "input": {} },
                            "state": { "battery": 87 },
                        },
                        {
                            "robot": "bravo",
                            "capabilities": ["status"],
                            "members": {},
                            "notes": ["state: unavailable: nothing has been published since the robot joined"],
                            "identity": { "robot": "bravo_backbone_inst", "input": {} },
                            "state": null,
                        },
                    ]
                })
            );
            // A robot filling no described target is listed with what it
            // fills and each describe value null, the note naming the robots
            // that fill the target.
            fleet
                .lock()
                .unwrap()
                .push(member("camera", "charlie", "front"));
            let listed = server
                .list_robots(server.state.fleet.as_ref().unwrap(), JsonObject::new())
                .await
                .expect("lists");
            assert_eq!(
                structured(listed)["robots"][2],
                json!({
                    "robot": "charlie",
                    "capabilities": ["camera"],
                    "members": { "camera": ["front"] },
                    "notes": [
                        "identity: robot `charlie` has no `status`; it fills `camera`; the robots with a \
                         `status` are `alpha`, `bravo`",
                        "state: robot `charlie` has no `status`; it fills `camera`; the robots with a \
                         `status` are `alpha`, `bravo`",
                    ],
                    "identity": null,
                    "state": null,
                })
            );
            let error = server
                .list_robots(
                    server.state.fleet.as_ref().unwrap(),
                    arguments(json!({ "robot": "alpha" })),
                )
                .await
                .expect_err("takes no arguments");
            assert!(error.message.contains("`robot.list` takes no arguments"));
        }

        #[tokio::test]
        async fn resources_follow_the_fleet_and_a_change_is_announced() {
            let (server, fleet) = served();
            let handle = server.fleet().expect("a per-robot server");
            let listed: Vec<String> = server
                .state
                .fleet
                .as_ref()
                .unwrap()
                .fleet()
                .resources(server.state.fleet_entries())
                .iter()
                .map(|resource| resource.uri.to_string())
                .collect();
            assert_eq!(
                listed,
                [
                    "peppy://resource/alpha/robot.status",
                    "peppy://resource/alpha/wrist_left/camera.latest_frame",
                    "peppy://resource/bravo/robot.status",
                ]
            );
            let unavailable = server
                .read_snapshot("peppy://resource/alpha/robot.status")
                .expect_err("listed, not attached");
            assert!(
                unavailable.message.contains("unavailable"),
                "{}",
                unavailable.message
            );
            let unknown = server
                .read_snapshot("peppy://resource/charlie/robot.status")
                .expect_err("not listed");
            assert_eq!(unknown.code, ErrorCode::RESOURCE_NOT_FOUND);
            assert!(
                unknown.message.contains("the robots are `alpha`, `bravo`"),
                "{}",
                unknown.message
            );

            let mut events = server.state.events.subscribe();
            let camera = member("camera", "alpha", "wrist_left");
            let attached = handle.attach(&camera);
            let (_, ingest) = &attached[0];
            let token = ingest.admit().expect("gate open");
            ingest
                .publish(token, json!({ "frame": "AAAA" }))
                .expect("publishes");
            let read = server
                .read_snapshot("peppy://resource/alpha/wrist_left/camera.latest_frame")
                .expect("attached and published");
            assert_eq!(read.contents.len(), 1);
            assert!(matches!(
                events.try_recv().expect("the publish is announced"),
                CatalogEvent::ResourceUpdated { .. }
            ));

            fleet
                .lock()
                .unwrap()
                .retain(|member| member.robot.as_deref() != Some("alpha"));
            handle.detach(&camera);
            handle.changed();
            assert!(matches!(
                events.try_recv().expect("the change is announced"),
                CatalogEvent::ResourceListChanged
            ));
            let gone = server
                .read_snapshot("peppy://resource/alpha/wrist_left/camera.latest_frame")
                .expect_err("detached and unlisted");
            assert_eq!(gone.code, ErrorCode::RESOURCE_NOT_FOUND);
        }

        #[test]
        fn the_listing_tool_joins_the_tool_list_with_the_describe_fields_in_its_schema() {
            let (server, _) = served();
            let names: Vec<&str> = server
                .state
                .tool_list
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect();
            assert_eq!(
                names,
                ["robot.get_identity", "camera.set_brightness", "robot.list"]
            );
            let listing = server.get_tool("robot.list").expect("listed");
            let output = listing.output_schema.expect("an output schema");
            let entry = &output["properties"]["robots"]["items"]["properties"];
            for field in [
                "robot",
                "capabilities",
                "members",
                "notes",
                "identity",
                "state",
            ] {
                assert!(
                    entry.get(field).is_some(),
                    "{field} is a field of every entry"
                );
            }
        }
    }

    fn built_server_tagged(tag: &str) -> ExposureServer {
        let mut bundle = test_bundle();
        bundle.exposure.tag = tag.to_string();
        ExposureServer::builder(bundle)
            .with_tool("front_camera.set_brightness", brightness_handler)
            .build()
            .expect("bundle and handlers agree")
    }

    #[test]
    fn a_set_lists_its_endpoints_in_order() {
        let set = ExposureSet::new(vec![built_server_tagged("v2"), built_server_tagged("v1")])
            .expect("distinct identities compose");
        assert_eq!(
            set.endpoint_paths(),
            [
                "/camera_and_recording/v2/mcp",
                "/camera_and_recording/v1/mcp"
            ]
        );
        assert_eq!(set.endpoint_paths().len(), 2);
    }

    #[test]
    fn a_set_refuses_an_exposure_listed_twice_and_an_empty_list() {
        let error = ExposureSet::new(vec![built_server_tagged("v1"), built_server_tagged("v1")])
            .expect_err("one identity cannot claim one path twice");
        assert_eq!(
            error,
            BuildError::DuplicateExposure {
                name: "camera_and_recording".to_string(),
                tag: "v1".to_string(),
            }
        );
        assert_eq!(
            ExposureSet::new(Vec::new()).expect_err("nothing to serve"),
            BuildError::NoExposures
        );
    }

    #[test]
    fn a_tool_without_a_handler_is_refused() {
        let error = ExposureServer::builder(test_bundle())
            .build()
            .expect_err("the brightness tool has no handler");
        assert_eq!(
            error,
            BuildError::MissingToolHandler {
                name: "front_camera.set_brightness".to_string()
            }
        );
    }

    #[test]
    fn a_handler_without_a_tool_is_refused() {
        let error = ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_tool("front_camera.set_gain", brightness_handler)
            .build()
            .expect_err("set_gain is not in the bundle");
        assert_eq!(
            error,
            BuildError::UnknownToolHandler {
                name: "front_camera.set_gain".to_string()
            }
        );
    }

    #[test]
    fn the_catalog_carries_descriptions_schemas_and_annotations() {
        let server = built_server();
        let resource = &server.state.resource_list[0];
        assert_eq!(resource.uri, "peppy://resource/front_camera.status");
        assert_eq!(resource.name, "front_camera.status");
        assert_eq!(resource.mime_type.as_deref(), Some("application/json"));

        let tool = &server.state.tool_list[0];
        assert_eq!(tool.name.as_ref(), "front_camera.set_brightness");
        assert!(tool.output_schema.is_some());
        let annotations = tool.annotations.as_ref().expect("annotations set");
        assert_eq!(annotations.read_only_hint, Some(false));
        let read_only = annotations_for(ServiceOperation::ReadOnly);
        assert_eq!(read_only.read_only_hint, Some(true));
        assert_eq!(read_only.destructive_hint, Some(false));
    }

    #[tokio::test]
    async fn calling_an_unknown_tool_is_a_protocol_error() {
        let error = built_server()
            .execute_tool("front_camera.set_gain", JsonObject::new())
            .await
            .expect_err("set_gain is not exposed");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(error.message.contains("not a tool of this exposure"));
    }

    #[tokio::test]
    async fn arguments_failing_the_derived_schema_never_reach_the_handler() {
        let server = built_server();
        for (raw, expected_fragment) in [
            (json!({ "value": 65 }), "value"),
            (json!({ "value": "bright" }), "value"),
            (json!({}), "value"),
            (json!({ "value": 1, "extra": true }), "extra"),
        ] {
            let error = server
                .execute_tool("front_camera.set_brightness", arguments(raw.clone()))
                .await
                .expect_err("invalid arguments are rejected");
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS, "for {raw}");
            assert!(
                error.message.contains(expected_fragment),
                "error for {raw} should mention `{expected_fragment}`: {}",
                error.message
            );
        }
    }

    #[tokio::test]
    async fn a_valid_call_returns_structured_output() {
        let result = built_server()
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 12 })),
            )
            .await
            .expect("valid call");
        assert_ne!(result.is_error, Some(true));
        assert_eq!(result.structured_content, Some(json!({ "applied": true })));
    }

    #[tokio::test]
    async fn a_bridge_failure_is_a_readable_tool_error() {
        let server = ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", |_call: ToolCall| async {
                Err(ToolCallError::Unavailable("no producer bound".to_string()))
            })
            .build()
            .expect("builds");
        let result = server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 1 })),
            )
            .await
            .expect("tool errors are results, not protocol errors");
        assert_eq!(result.is_error, Some(true));
        let rendered = serde_json::to_string(&result.content).expect("content serializes");
        assert!(
            rendered.contains("provider unavailable: no producer bound"),
            "got {rendered}"
        );
    }

    #[tokio::test]
    async fn an_oversize_result_is_a_tool_error() {
        let server = ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", |_call: ToolCall| async {
                Ok(json!({ "applied": "y".repeat(128) }))
            })
            .build()
            .expect("builds");
        let result = server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 1 })),
            )
            .await
            .expect("oversize is a tool error");
        assert_eq!(result.is_error, Some(true));
        let rendered = serde_json::to_string(&result.content).expect("content serializes");
        assert!(
            rendered.contains("exceeds the 64 byte limit"),
            "got {rendered}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_handler_slower_than_the_deadline_is_a_tool_error() {
        let server = ExposureServer::builder(test_bundle())
            .with_tool("front_camera.set_brightness", |_call: ToolCall| async {
                tokio::time::sleep(Duration::from_secs(3600)).await;
                Ok(json!({ "applied": true }))
            })
            .build()
            .expect("builds");
        let result = server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 1 })),
            )
            .await
            .expect("deadline is a tool error");
        assert_eq!(result.is_error, Some(true));
        let rendered = serde_json::to_string(&result.content).expect("content serializes");
        assert!(
            rendered.contains("did not answer within 2000 ms"),
            "got {rendered}"
        );
    }

    #[test]
    fn reads_walk_unavailable_fresh_and_stale_with_freshness_as_ttl() {
        let (clock, nanos) = manual_clock();
        let server = ExposureServer::builder(test_bundle())
            .with_clock(clock)
            .with_tool("front_camera.set_brightness", brightness_handler)
            .build()
            .expect("builds");
        let uri = "peppy://resource/front_camera.status";

        // Unavailable and stale are deliberately internal errors: both are
        // server-side snapshot conditions, not client mistakes.
        let error = server
            .read_snapshot(uri)
            .expect_err("nothing published yet");
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("unavailable"));

        let ingest = server
            .ingest("front_camera.status")
            .expect("resource exists");
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");

        nanos.store(500 * 1_000_000, Ordering::SeqCst);
        let read = server.read_snapshot(uri).expect("fresh snapshot serves");
        assert_eq!(read.ttl_ms, Some(1500), "ttl is the remaining freshness");
        assert_eq!(read.cache_scope, Some(CacheScope::Private));

        nanos.store(2_500 * 1_000_000, Ordering::SeqCst);
        let error = server.read_snapshot(uri).expect_err("2500 ms old is stale");
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("stale"), "got {}", error.message);
        assert!(
            error.message.contains("2500 ms old"),
            "got {}",
            error.message
        );
    }

    #[test]
    fn reading_an_unknown_uri_is_resource_not_found() {
        let error = built_server()
            .read_snapshot("peppy://resource/absent")
            .expect_err("absent resources are refused");
        assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
    }

    #[test]
    fn the_ingest_lookup_uses_public_resource_names() {
        let server = built_server();
        assert!(server.ingest("front_camera.status").is_some());
        assert!(server.ingest("front_camera.absent").is_none());
    }

    #[test]
    fn server_info_advertises_the_exposure_identity_and_2026_07_28() {
        let info = built_server().get_info();
        assert_eq!(info.protocol_version, ProtocolVersion::V_2026_07_28);
        assert_eq!(info.server_info.name, "camera_and_recording");
        assert_eq!(info.server_info.version, "v1");
        assert_eq!(
            info.instructions.as_deref(),
            Some("Observe the front camera.")
        );
        let resources = info.capabilities.resources.expect("resources capability");
        assert_eq!(resources.subscribe, Some(true));
        assert_eq!(resources.list_changed, Some(true));
        assert!(info.capabilities.tools.is_some());
    }
}
