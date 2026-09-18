//! The MCP server: a catalog-driven
//! [`ServerHandler`](rmcp::ServerHandler) per exposure bundle, and the
//! [`ExposureSet`] serving several of them side by side over Streamable
//! HTTP under MCP `2026-07-28`.

use crate::clock::Clock;
use crate::error::{BuildError, ToolCallError};
use crate::state::{ReadRefusal, ResourceIngest, ResourceState, ResourceUpdated};
use crate::tasks::{ActionContext, ActionExit, TaskHandler};
use peppy_mcp_catalog::{
    BundleIdentity, BundleServer, ExposureBundle, ServiceOperation, TaskEntry, ToolEntry,
};
use rmcp::model::{
    CacheScope, CallToolRequestParams, CallToolResponse, CallToolResult, CancelTaskParams,
    ClientCapabilities, ContentBlock, CreateTaskResult, DiscoverResult, ElicitRequest,
    ElicitRequestParams, ElicitResult, ElicitationAction, ElicitationSchema, GetTaskParams,
    GetTaskResult, Implementation, InputRequest, JsonObject, ListResourcesResult, ListToolsResult,
    PaginatedRequestParams, ProtocolVersion, ReadResourceRequestParams, ReadResourceResponse,
    ReadResourceResult, Resource, ResourceContents, ServerCapabilities, ServerInfo,
    SubscriptionFilter, Tool, ToolAnnotations, UpdateTaskParams,
};
use rmcp::service::{RequestContext, SubscriptionContext, SubscriptionSendError};
use rmcp::task_manager::{TaskContext, TaskExit, TaskManager, TaskOptions};
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};
use rmcp::{ErrorData as McpError, RoleServer, ServerHandler};
use serde_json::{Value, json};
use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;

/// `ttlMs` for the catalog-shaped results: discovery, `tools/list`, and
/// `resources/list`. The catalog is fixed for the life of the server (a
/// changed exposure restarts the process serving it), so clients may cache
/// it for as long as they keep the connection.
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

/// One registered bridge: validated canonical-JSON input in, canonical-JSON
/// output or a [`ToolCallError`] out. Any `Fn(Value) -> impl Future` with
/// those shapes implements it.
pub trait ToolHandler: Send + Sync + 'static {
    fn call(
        &self,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolCallError>> + Send>>;
}

impl<F, Fut> ToolHandler for F
where
    F: Fn(Value) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<Value, ToolCallError>> + Send + 'static,
{
    fn call(
        &self,
        input: Value,
    ) -> Pin<Box<dyn Future<Output = Result<Value, ToolCallError>> + Send>> {
        Box::pin(self(input))
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
    /// validated before a task can be materialized.
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
    events: broadcast::Sender<ResourceUpdated>,
    clock: Clock,
}

/// Builds an [`ExposureServer`] from a parsed bundle, one registered
/// handler per exposed tool, and one task handler per exposed action.
pub struct ExposureServerBuilder {
    bundle: ExposureBundle,
    clock: Clock,
    handlers: HashMap<String, Arc<dyn ToolHandler>>,
    task_handlers: HashMap<String, Arc<dyn TaskHandler>>,
}

impl ExposureServerBuilder {
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
        }
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

    fn read_snapshot(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let Some(resource) = self.state.resources_by_uri.get(uri) else {
            return Err(McpError::resource_not_found(
                format!("`{uri}` is not a resource of this exposure"),
                Some(json!({ "uri": uri })),
            ));
        };
        match resource.snapshot_for_read(self.state.clock.now_nanos()) {
            Ok(view) => Ok(ReadResourceResult::new(vec![
                ResourceContents::text(view.serialized, uri).with_mime_type("application/json"),
            ])
            .with_ttl_ms(view.remaining_fresh_ms)
            .with_cache_scope(CacheScope::Private)),
            Err(ReadRefusal::Unavailable) => Err(McpError::internal_error(
                format!(
                    "resource `{uri}` is unavailable: nothing has been published since the \
                     server started"
                ),
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

        let deadline = Duration::from_millis(tool.entry.deadline_ms.get());
        let result = match tokio::time::timeout(deadline, tool.handler.call(input)).await {
            Err(_elapsed) => {
                return Ok(tool_error(format!(
                    "deadline exceeded: the provider did not answer within {} ms",
                    tool.entry.deadline_ms
                )));
            }
            Ok(Err(error)) => return Ok(tool_error(error.to_string())),
            Ok(Ok(value)) => value,
        };

        if let Some(limit) = tool.entry.max_result_bytes {
            let size = serialized_len(&result);
            if size > limit.get() {
                return Ok(tool_error(format!(
                    "result of {size} bytes exceeds the {} byte limit",
                    limit.get()
                )));
            }
        }
        Ok(CallToolResult::structured(result))
    }

    /// Materializes the MCP task behind a task-backed tool call.
    ///
    /// A client that did not declare the tasks extension capability is
    /// refused first: per the design, such a client never receives a task
    /// handle, and the capability, not its arguments, is what it has to fix.
    /// The goal fields are validated next, so neither invalid fields nor a
    /// goal orphaned by a client that could not poll it ever materializes a
    /// task.
    fn start_task(
        &self,
        name: &str,
        arguments: JsonObject,
        client_declared_tasks: bool,
    ) -> Result<CreateTaskResult, McpError> {
        let Some(task) = self.state.tasks.get(name) else {
            return Err(McpError::invalid_params(
                format!("`{name}` is not a task of this exposure"),
                None,
            ));
        };
        if !client_declared_tasks {
            return Err(McpError::missing_required_client_capability(
                ClientCapabilities::builder().enable_tasks().build(),
            ));
        }
        let input = validated_input(name, &task.validator, arguments)?;

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
            Box::pin(run_task_operation(task, input, context))
        });
        Ok(CreateTaskResult::new(seed))
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
    input: Value,
    context: TaskContext,
) -> Result<CallToolResult, TaskExit> {
    let deadline = Duration::from_millis(task.entry.deadline_ms.get());
    match tokio::time::timeout(deadline, drive_task(task, input, context)).await {
        Ok(result) => result,
        Err(_elapsed) => Err(TaskExit::Error(McpError::internal_error(
            format!(
                "deadline exceeded: the goal did not reach a terminal state within {} ms",
                deadline.as_millis()
            ),
            None,
        ))),
    }
}

/// Identifier of the confirmation entry in the task's `inputRequests`.
const CONFIRMATION_INPUT_KEY: &str = "confirmation";

async fn drive_task(
    task: Arc<TaskState>,
    input: Value,
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
        inner: context.clone(),
    };
    match task.handler.start(input, action_context).await {
        Ok(value) => Ok(CallToolResult::structured(value)),
        Err(ActionExit::Cancelled) => Err(TaskExit::Cancelled),
        Err(ActionExit::Failed(message)) => {
            Err(TaskExit::Error(McpError::internal_error(message, None)))
        }
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
        Ok(
            ListResourcesResult::with_all_items(self.state.resource_list.clone())
                .with_ttl_ms(CATALOG_TTL_MS)
                .with_cache_scope(CacheScope::Private),
        )
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
        if self.state.tasks.contains_key(name) {
            let client_declared_tasks = context
                .client_capabilities()
                .is_some_and(|capabilities| capabilities.supports_tasks());
            return self
                .start_task(name, arguments, client_declared_tasks)
                .map(CallToolResponse::Task);
        }
        self.execute_tool(name, arguments).await.map(Into::into)
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
                    Ok(ResourceUpdated { uri }) => {
                        match sink.notify_resource_updated(uri).await {
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

    async fn brightness_handler(input: Value) -> Result<Value, ToolCallError> {
        let value = input["value"].as_i64().expect("validated integer");
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
        _input: Value,
        _context: crate::tasks::ActionContext,
    ) -> Result<Value, ActionExit> {
        Ok(json!({ "frames": 120 }))
    }

    async fn resume_handler(
        _input: Value,
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
    async fn a_client_without_the_tasks_capability_never_materializes_a_task() {
        let server = built_task_server();
        let error = server
            .start_task("recorder.resume_session", JsonObject::new(), false)
            .expect_err("the capability is required");
        assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
        assert_eq!(
            server.state.manager.running_task_count(),
            0,
            "no orphaned goal may run for a client that cannot poll it"
        );

        // The capability is the client's real blocker, so it is reported
        // ahead of anything its arguments could be told about.
        let error = server
            .start_task(
                "recorder.record_episode",
                arguments(json!({ "episode_name": 7 })),
                false,
            )
            .expect_err("the capability is required");
        assert_eq!(error.code, ErrorCode::MISSING_REQUIRED_CLIENT_CAPABILITY);
    }

    #[tokio::test]
    async fn invalid_goal_arguments_never_materialize_a_task() {
        let server = built_task_server();
        let error = server
            .start_task(
                "recorder.record_episode",
                arguments(json!({ "episode_name": 7 })),
                true,
            )
            .expect_err("the goal fields fail the derived schema");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert_eq!(server.state.manager.running_task_count(), 0);
    }

    #[tokio::test]
    async fn a_task_completes_with_the_bridge_result() {
        let server = built_task_server();
        let created = server
            .start_task("recorder.resume_session", JsonObject::new(), true)
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
                |_input: Value, context: crate::tasks::ActionContext| async move {
                    context.report_feedback("resuming at frame 42");
                    context.cancel_requested().await;
                    Err(ActionExit::Cancelled)
                },
            )
            .build()
            .expect("builds");
        let created = server
            .start_task("recorder.resume_session", JsonObject::new(), true)
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
                |_input: Value, _context: crate::tasks::ActionContext| async move {
                    Err::<Value, _>(ActionExit::Failed(
                        "the provider abandoned the goal".to_string(),
                    ))
                },
            )
            .build()
            .expect("builds");
        let created = server
            .start_task("recorder.resume_session", JsonObject::new(), true)
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
                move |_input: Value, _context: crate::tasks::ActionContext| {
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
        let created = server
            .start_task(
                "recorder.record_episode",
                arguments(json!({ "episode_name": "pick_and_place" })),
                true,
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
                move |_input: Value, _context: crate::tasks::ActionContext| {
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
        let created = server
            .start_task(
                "recorder.record_episode",
                arguments(json!({ "episode_name": "pick_and_place" })),
                true,
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
                |_input: Value, _context: crate::tasks::ActionContext| async move {
                    std::future::pending::<Result<Value, ActionExit>>().await
                },
            )
            .build()
            .expect("builds");
        let created = server
            .start_task("recorder.resume_session", JsonObject::new(), true)
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
            .with_tool("front_camera.set_brightness", |_input: Value| async {
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
            .with_tool("front_camera.set_brightness", |_input: Value| async {
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
            .with_tool("front_camera.set_brightness", |_input: Value| async {
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
