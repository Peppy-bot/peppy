//! The MCP server: a catalog-driven
//! [`ServerHandler`](rmcp::ServerHandler) per exposure bundle, and the
//! [`ExposureSet`] serving several of them side by side over Streamable
//! HTTP under MCP `2026-07-28`.

use crate::call_record::{CallRecord, ClientIdentity, Completion, Recording};
use crate::clock::Clock;
use crate::error::{BuildError, PublishError, ToolCallError};
use crate::fleet::{
    Fleet, FleetMember, FleetRuntime, FleetSource, MemberAddress, published_name, published_uri,
    quoted,
};
use crate::representation::{
    DOCUMENT_MIME_TYPE, SnapshotContent, apply_response_policies, listed_mime_type,
};
use crate::state::{CatalogEvent, ReadRefusal, ResourceIngest, ResourceState, SnapshotView};
use crate::tasks::{ActionContext, ActionExit, ActionSurface, TaskHandler};
use peppy_mcp_catalog::{
    BundleIdentity, BundleServer, BundleSurface, ExposureBundle, GoalBound, ImageCodec,
    PictureEntry, ROBOT_ARGUMENT, ResourceEntry, ServiceOperation, TaskEntry, ToolEntry,
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

/// What a resource that has stored no snapshot says of itself.
const UNAVAILABLE_SINCE_START: &str =
    "unavailable: nothing has been published since the server started";

/// What a resource of a per-robot surface says of itself until the host
/// attaches the member that feeds it.
const UNAVAILABLE_SINCE_JOIN: &str =
    "unavailable: nothing has been published since the robot joined";

/// `ttlMs` for the catalog-shaped results: discovery, `tools/list`, and
/// `resources/list`. The catalog is fixed for the life of the server (a
/// changed exposure restarts the process serving it), so clients may cache
/// it for as long as they keep the connection.
const CATALOG_TTL_MS: u64 = 3_600_000;

/// Grace period the advertised TTL of a task under a whole-goal deadline
/// carries on top of that deadline. The runtime fails an overrunning goal
/// itself, with a message naming the deadline; the manager's TTL sweep
/// fires at `created + ttl` and aborts the operation with a generic expiry
/// instead, so the two must not coincide. The task stays observable for a
/// further TTL window past that, which is what a poller reads the terminal
/// state from.
const TASK_TTL_GRACE_MS: u64 = 1_000;

/// The TTL of a task whose goal is bounded by its progress: one day, the
/// longest such a goal may run as a task however steadily it progresses.
/// The progress window bounds only the silence between two signs of
/// progress; the MCP specification asks for a maximum all the same, whatever
/// the progress, and the TTL is also what bounds how long the task manager
/// keeps the task once it has ended.
const PROGRESS_TASK_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Capacity of the resource-updated event channel; a listener lagging this
/// far behind skips to the newest events, which for latest-snapshot
/// semantics loses nothing that a fresh read would not recover.
const EVENT_CHANNEL_CAPACITY: usize = 256;

/// One validated call handed to a bridge: the canonical-JSON input the
/// contract member takes, and the provider it goes to.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolCall {
    pub input: Value,
    pub recipient: Recipient,
}

/// The provider a call goes to.
#[derive(Debug, Clone, PartialEq)]
pub enum Recipient {
    /// The producer the launcher bound to the entry's target.
    BoundProducer,
    /// The member of the target the call's robot fills.
    Member(MemberAddress),
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

/// One picture tool. It has no handler: the server answers it from the
/// snapshot of the resource its entry names.
struct PictureState {
    entry: PictureEntry,
    /// Compiled from the entry's input schema; every call is validated
    /// before its routing arguments are read.
    validator: jsonschema::Validator,
}

struct ServerState {
    server: BundleServer,
    exposure: BundleIdentity,
    addressing: Addressing,
    tools: HashMap<String, Arc<ToolState>>,
    tasks: HashMap<String, Arc<TaskState>>,
    pictures: HashMap<String, Arc<PictureState>>,
    /// The record of the state-changing calls, on a bundle that declares
    /// one.
    record: Option<CallRecord>,
    /// `tools/list` order: the bundle's tools, its tasks, its picture tools,
    /// its record tool, then the listing tool of a per-robot surface.
    tool_list: Vec<Tool>,
    /// Task handles are in-memory and live as long as the serving process
    /// by design; every HTTP session shares this manager, which is what
    /// lets a reconnecting client keep polling an existing task id.
    manager: TaskManager,
    events: broadcast::Sender<CatalogEvent>,
    clock: Clock,
}

/// How the server addresses providers and publishes resources, as the
/// bundle's surface decides.
enum Addressing {
    /// Every call goes to the producer the launcher bound to its target,
    /// and the catalog's own resources are the ones served.
    Fixed(CatalogResources),
    /// Every call names its robot, and the resources follow the fleet.
    PerRobot(Arc<FleetRuntime>),
}

/// The resources of a fixed surface, as the catalog fixes them for the life
/// of the server.
struct CatalogResources {
    by_uri: HashMap<String, Arc<ResourceState>>,
    uri_by_name: HashMap<String, String>,
    list: Vec<Resource>,
}

impl CatalogResources {
    /// One resource state per catalog entry, keyed by the URI clients read
    /// it at and by the name the host feeds it under.
    fn new(entries: &[ResourceEntry]) -> Result<Self, BuildError> {
        let mut resources = Self {
            by_uri: HashMap::new(),
            uri_by_name: HashMap::new(),
            list: Vec::new(),
        };
        for entry in entries {
            resources.list.push(
                Resource::new(entry.uri.clone(), entry.name.clone())
                    .with_description(entry.description.clone())
                    .with_mime_type(listed_mime_type(&entry.policies)),
            );
            resources
                .uri_by_name
                .insert(entry.name.clone(), entry.uri.clone());
            if resources
                .by_uri
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
        Ok(resources)
    }
}

/// Builds an [`ExposureServer`] from a parsed bundle, one registered
/// handler per exposed tool, one task handler per exposed action, and the
/// source of the fleet on a per-robot bundle.
pub struct ExposureServerBuilder {
    bundle: ExposureBundle,
    clock: Clock,
    wall_clock: Clock,
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

    /// Injects the wall clock the call record stamps its entries on, as
    /// nanoseconds since the Unix epoch. Defaults to the host wall clock;
    /// tests pass a counter.
    pub fn with_wall_clock(mut self, clock: Clock) -> Self {
        self.wall_clock = clock;
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
            wall_clock,
            mut handlers,
            mut task_handlers,
            fleet_source,
        } = self;

        let mut names = HashSet::new();
        for entry in &bundle.resources {
            if !names.insert(entry.name.clone()) {
                return Err(BuildError::DuplicateName {
                    name: entry.name.clone(),
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

        let mut pictures = HashMap::new();
        for entry in &bundle.pictures {
            if !names.insert(entry.name.clone()) {
                return Err(BuildError::DuplicateName {
                    name: entry.name.clone(),
                });
            }
            if !bundle
                .resources
                .iter()
                .any(|resource| serves_the_picture_of(resource, entry))
            {
                return Err(BuildError::NoPictureResource {
                    name: entry.name.clone(),
                    resource: entry.resource.clone(),
                    target: entry.target.clone(),
                });
            }
            let (tool, validator) = catalog_tool(
                &entry.name,
                &entry.description,
                &entry.input_schema,
                &entry.output_schema,
                read_only_annotations(),
            )?;
            tool_list.push(tool);
            pictures.insert(
                entry.name.clone(),
                Arc::new(PictureState {
                    entry: entry.clone(),
                    validator,
                }),
            );
        }

        let record = match &bundle.call_record {
            Some(entry) => {
                if !names.insert(entry.name.clone()) {
                    return Err(BuildError::DuplicateName {
                        name: entry.name.clone(),
                    });
                }
                let record = CallRecord::new(
                    entry.name.clone(),
                    entry.description.clone(),
                    entry.keep,
                    wall_clock,
                );
                tool_list.push(record.tool());
                Some(record)
            }
            None => None,
        };

        // A per-robot surface publishes its resources per robot, from the
        // fleet; the catalog's own URIs serve a fixed surface.
        let addressing = match (&bundle.surface, fleet_source) {
            (BundleSurface::Fixed { .. }, None) => {
                Addressing::Fixed(CatalogResources::new(&bundle.resources)?)
            }
            (BundleSurface::Fixed { .. }, Some(_)) => {
                return Err(BuildError::UnexpectedFleetSource);
            }
            (BundleSurface::PerRobot { .. }, None) => return Err(BuildError::MissingFleetSource),
            (BundleSurface::PerRobot { robots, contracts }, Some(source)) => {
                if !names.insert(robots.list.name.clone()) {
                    return Err(BuildError::DuplicateName {
                        name: robots.list.name.clone(),
                    });
                }
                let fleet = Arc::new(FleetRuntime::new(
                    &bundle,
                    robots.clone(),
                    contracts,
                    source,
                ));
                tool_list.push(listing_tool(&fleet));
                Addressing::PerRobot(fleet)
            }
        };

        let (events, _) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        Ok(ExposureServer {
            state: Arc::new(ServerState {
                server: bundle.server,
                exposure: bundle.exposure,
                addressing,
                tools,
                tasks,
                pictures,
                record,
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

/// Whether `resource` is the one `picture` answers with: a resource of the
/// picture's target under a `jpeg` representation, so that every snapshot of
/// it holds a picture.
fn serves_the_picture_of(resource: &ResourceEntry, picture: &PictureEntry) -> bool {
    resource.name == picture.resource
        && resource.target == picture.target
        && resource
            .policies
            .representation
            .as_ref()
            .is_some_and(|representation| representation.image == ImageCodec::Jpeg)
}

/// Validates one catalog entry's input schema and builds the served `Tool`
/// listing plus its compiled validator, shared by the tool, task and picture
/// loops.
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
        ServiceOperation::ReadOnly => read_only_annotations(),
        ServiceOperation::Mutating => ToolAnnotations::default().read_only(false),
    }
}

/// The annotations of a tool that observes and changes nothing: a read-only
/// service, a picture tool, the listing tool, the record tool.
pub(crate) fn read_only_annotations() -> ToolAnnotations {
    ToolAnnotations::default()
        .read_only(true)
        .destructive(false)
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
            wall_clock: Clock::wall(),
            handlers: HashMap::new(),
            task_handlers: HashMap::new(),
            fleet_source: None,
        }
    }

    /// The handle through which the host attaches and detaches the members
    /// of a per-robot bundle; `None` on a fixed surface.
    pub fn fleet(&self) -> Option<FleetHandle> {
        match &self.state.addressing {
            Addressing::Fixed(_) => None,
            Addressing::PerRobot(fleet) => Some(FleetHandle {
                fleet: Arc::clone(fleet),
                events: self.state.events.clone(),
                clock: self.state.clock.clone(),
            }),
        }
    }

    /// The ingest feeding the named resource of a fixed surface, or `None`
    /// when the bundle exposes no such resource.
    pub fn ingest(&self, resource_name: &str) -> Option<ResourceIngest> {
        let Addressing::Fixed(resources) = &self.state.addressing else {
            return None;
        };
        let uri = resources.uri_by_name.get(resource_name)?;
        Some(ResourceIngest {
            state: Arc::clone(resources.by_uri.get(uri)?),
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
    /// attached yet has no state.
    fn resource_state(&self, uri: &str) -> Result<Option<Arc<ResourceState>>, McpError> {
        let fleet = match &self.state.addressing {
            Addressing::Fixed(resources) => {
                return resources.by_uri.get(uri).cloned().map(Some).ok_or_else(|| {
                    McpError::resource_not_found(
                        format!("`{uri}` is not a resource of this exposure"),
                        Some(json!({ "uri": uri })),
                    )
                });
            }
            Addressing::PerRobot(fleet) => fleet,
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
        Ok(fleet.state(uri))
    }

    /// The snapshot the resource at `uri` serves to a read that arrives
    /// now, and to a picture tool alike: the first message received after
    /// this call, or the stored snapshot once the freshness bound has
    /// passed in silence (see
    /// [`ResourceState::next_snapshot`]); or the words that say why it
    /// serves none. `resource` is `None` for a resource of a per-robot
    /// surface whose member the host has not attached yet.
    async fn snapshot_now(
        &self,
        uri: &str,
        resource: Option<&ResourceState>,
    ) -> Result<SnapshotView, String> {
        let Some(resource) = resource else {
            return Err(format!("resource `{uri}` is {UNAVAILABLE_SINCE_JOIN}"));
        };
        resource
            .next_snapshot(&self.state.clock)
            .await
            .map_err(|refusal| match refusal {
                ReadRefusal::Unavailable => {
                    format!("resource `{uri}` is {UNAVAILABLE_SINCE_START}")
                }
                ReadRefusal::Stale { age_ms, max_age_ms } => format!(
                    "resource `{uri}` is stale: the snapshot is {age_ms} ms old and \
                     `max_age_ms` is {max_age_ms}"
                ),
            })
    }

    /// Answers a read with the snapshot's typed contents, both under the
    /// resource's URI: the document, then the blob of a resource with a
    /// representation.
    async fn read_snapshot(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        let resource = self.resource_state(uri)?;
        let view = self
            .snapshot_now(uri, resource.as_deref())
            .await
            .map_err(|unavailable| McpError::internal_error(unavailable, None))?;
        let mut contents = vec![
            ResourceContents::text(view.content.document, uri).with_mime_type(DOCUMENT_MIME_TYPE),
        ];
        if let Some(blob) = view.content.blob {
            contents.push(ResourceContents::blob(blob.base64, uri).with_mime_type(blob.mime_type));
        }
        Ok(ReadResourceResult::new(contents)
            .with_ttl_ms(view.remaining_fresh_ms)
            .with_cache_scope(CacheScope::Private))
    }

    /// Answers a picture tool with the snapshot a read of its resource
    /// serves now: the blob as an image, the document as text and as the
    /// structured content. Nothing reaches the Peppy graph. A resource that
    /// serves no snapshot now is a tool error in the words of the read.
    async fn look(
        &self,
        picture: &PictureState,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        let input = validated_input(&picture.entry.name, &picture.validator, arguments)?;
        let (uri, resource) = self.pictured_resource(&picture.entry, input)?;
        match self.snapshot_now(&uri, resource.as_deref()).await {
            Ok(view) => Ok(picture_result(view.content)),
            Err(unavailable) => Ok(tool_error(unavailable)),
        }
    }

    /// The resource a picture tool answers with, as its URI and its state:
    /// the catalog's on a fixed surface, and on a per-robot surface the one
    /// published for the member the call's routing arguments name. A call
    /// naming no member of the fleet is refused like a call of any other
    /// tool, and a member the host has not attached yet has no state.
    fn pictured_resource(
        &self,
        picture: &PictureEntry,
        mut input: Value,
    ) -> Result<(String, Option<Arc<ResourceState>>), McpError> {
        let fleet = match &self.state.addressing {
            Addressing::Fixed(resources) => {
                let uri = resources
                    .uri_by_name
                    .get(&picture.resource)
                    .expect("the build held every picture to a resource of the bundle");
                return Ok((uri.clone(), resources.by_uri.get(uri).cloned()));
            }
            Addressing::PerRobot(fleet) => fleet,
        };
        let (robot, name) = take_routing(fleet, &picture.target, &mut input);
        fleet
            .fleet()
            .route(&robot, &picture.target, name.as_deref())
            .map_err(|refusal| McpError::invalid_params(refusal.to_string(), None))?;
        let uri = published_uri(&published_name(&picture.resource, &robot, name.as_deref()));
        let resource = fleet.state(&uri);
        Ok((uri, resource))
    }

    /// The recording of a call of `tool` by `client`: an entry of the call
    /// record when the bundle keeps one, nothing otherwise.
    fn record_call(
        &self,
        client: Option<ClientIdentity>,
        tool: &str,
        arguments: &JsonObject,
    ) -> Recording {
        match &self.state.record {
            Some(record) => record.start(client, tool, arguments),
            None => Recording::unkept(),
        }
    }

    /// The call a bridge runs for `arguments`: validated against the
    /// entry's schema and routed to its provider. A refusal ends the
    /// recording as refused.
    fn prepared_call(
        &self,
        name: &str,
        validator: &jsonschema::Validator,
        target: &str,
        arguments: JsonObject,
        recording: &mut Recording,
    ) -> Result<ToolCall, McpError> {
        let call =
            validated_input(name, validator, arguments).and_then(|input| self.route(target, input));
        if let Err(refusal) = &call {
            recording.refused(refusal.message.to_string());
        }
        call
    }

    async fn execute_tool(
        &self,
        name: &str,
        arguments: JsonObject,
        client: Option<ClientIdentity>,
    ) -> Result<CallToolResult, McpError> {
        let Some(tool) = self.state.tools.get(name) else {
            return Err(McpError::invalid_params(
                format!("`{name}` is not a tool of this exposure"),
                None,
            ));
        };
        // The record keeps the calls that change something.
        let mut recording = match tool.entry.operation {
            ServiceOperation::Mutating => self.record_call(client, name, &arguments),
            ServiceOperation::ReadOnly => Recording::unkept(),
        };
        let call = self.prepared_call(
            name,
            &tool.validator,
            &tool.entry.target,
            arguments,
            &mut recording,
        )?;

        let deadline = Duration::from_millis(tool.entry.deadline_ms.get());
        let value = match tokio::time::timeout(deadline, tool.handler.call(call)).await {
            Err(_elapsed) => {
                let overrun = format!(
                    "deadline exceeded: the provider did not answer within {} ms",
                    tool.entry.deadline_ms
                );
                recording.failed(overrun.clone());
                return Ok(tool_error(overrun));
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                match error {
                    ToolCallError::Unavailable(_) => recording.refused(message.clone()),
                    ToolCallError::Deadline(_) | ToolCallError::Failed(_) => {
                        recording.failed(message.clone());
                    }
                }
                return Ok(tool_error(message));
            }
            Ok(Ok(value)) => value,
        };

        let completion = Completion::of(&value);
        match tool_answer(&tool.entry, value) {
            Ok(result) => {
                recording.completed(completion);
                Ok(result)
            }
            Err(refusal) => {
                recording.failed(refusal.clone());
                Ok(tool_error(refusal))
            }
        }
    }

    /// Runs the action behind a tool call on the surface the client can
    /// drive: an MCP task for a client that declared the tasks extension,
    /// the call itself for one that did not.
    async fn run_action(
        &self,
        task: &Arc<TaskState>,
        arguments: JsonObject,
        client: Option<ClientIdentity>,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, McpError> {
        let client_declared_tasks = context
            .client_capabilities()
            .is_some_and(|capabilities| capabilities.supports_tasks());
        if client_declared_tasks {
            return self
                .start_task(task, arguments, client)
                .map(CallToolResponse::Task);
        }
        let progress = context
            .meta
            .get_progress_token()
            .map(|token| ProgressReporter::new(context.peer, token));
        self.run_action_in_call(task, arguments, client, context.ct, progress)
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
        client: Option<ClientIdentity>,
    ) -> Result<CreateTaskResult, McpError> {
        let mut recording = self.record_call(client, &task.entry.name, &arguments);
        let call = self.prepared_call(
            &task.entry.name,
            &task.validator,
            &task.entry.target,
            arguments,
            &mut recording,
        )?;

        let task = Arc::clone(task);
        let options = TaskOptions::new().with_ttl_ms(task_ttl_ms(task.entry.bound));
        let seed = self.state.manager.spawn(options, move |context| {
            Box::pin(run_task_operation(task, call, context, recording))
        });
        Ok(CreateTaskResult::new(seed))
    }

    /// Runs the action inside the `tools/call` that started it, for a
    /// client without the tasks extension: the call answers with the
    /// goal's result once it settles (a goal that ends cancelled answers
    /// with the tool error of
    /// [`CancelledGoal::into_tool_result`](crate::tasks::CancelledGoal::into_tool_result)),
    /// feedback is relayed through `progress` when the call
    /// carries a progress token, `cancel` firing (the client closing the
    /// call) cancels the goal, and the goal's bound limits the wait (see
    /// [`within_goal_bound`]).
    ///
    /// Feedback is also what opens the call's response: the transport sends
    /// the HTTP status and headers with the handler's first message, so the
    /// first progress notification reaches the client as soon as the goal
    /// reports, and a call without a progress token receives nothing, not
    /// even its headers, before the result.
    ///
    /// A confirmation-gated action is refused: the confirmation is an
    /// in-task input request, so a task is the only surface that can ask
    /// for it. The refusal is what such a client has to fix, so it is
    /// reported ahead of anything its arguments could be told about.
    async fn run_action_in_call(
        &self,
        task: &Arc<TaskState>,
        arguments: JsonObject,
        client: Option<ClientIdentity>,
        cancel: tokio_util::sync::CancellationToken,
        progress: Option<ProgressReporter>,
    ) -> Result<CallToolResult, McpError> {
        let mut recording = self.record_call(client, &task.entry.name, &arguments);
        if task.entry.confirmation_required {
            let refusal = confirmation_needs_the_tasks_extension(&task.entry.name);
            recording.refused(refusal.message.to_string());
            return Err(refusal);
        }
        let call = self.prepared_call(
            &task.entry.name,
            &task.validator,
            &task.entry.target,
            arguments,
            &mut recording,
        )?;

        let (feedback, relay) = mpsc::unbounded_channel();
        let action_context = ActionContext {
            surface: ActionSurface::Call { feedback, cancel },
        };
        let operation = task.handler.start(call, action_context);
        let outcome = within_goal_bound(
            task.entry.bound,
            relay_feedback_until_settled(operation, relay, progress),
        )
        .await;
        let outcome = match outcome {
            Ok(outcome) => outcome,
            Err(overrun) => {
                recording.failed(overrun.clone());
                return Ok(tool_error(overrun));
            }
        };
        recording.ended_by(&outcome);
        match outcome {
            Ok(value) => Ok(CallToolResult::structured(value)),
            Err(ActionExit::Cancelled(goal)) => Ok(goal.into_tool_result()),
            Err(exit @ ActionExit::Failed(_)) => Ok(tool_error(exit.to_string())),
        }
    }

    /// Answers the record tool: every kept call, newest first.
    fn recent_calls(
        &self,
        record: &CallRecord,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        refuse_arguments(
            record.name(),
            "lists the last calls of this endpoint",
            &arguments,
        )?;
        Ok(CallToolResult::structured(record.answer()))
    }
}

/// The result of a tool that answers with a picture: the blob as an image
/// block, the document as the text and as the structured content. Content
/// without a blob is the document alone.
fn picture_result(content: SnapshotContent) -> CallToolResult {
    let document = serde_json::from_str(&content.document)
        .expect("a snapshot's document is the JSON this runtime serialized");
    let mut result = CallToolResult::structured(document);
    if let Some(blob) = content.blob {
        result
            .content
            .insert(0, ContentBlock::image(blob.base64, blob.mime_type));
    }
    result
}

/// The result a service tool answers with `value`, the provider's
/// response: under a representation, the response's frame as an image and
/// the rest as the document, through the entry's content policies; else the
/// response as the structured content, held to `max_result_bytes`.
fn tool_answer(entry: &ToolEntry, value: Value) -> Result<CallToolResult, String> {
    if entry.representation.is_none() {
        return within_result_limit(entry, value).map(CallToolResult::structured);
    }
    apply_response_policies(entry.content(), value)
        .map(picture_result)
        .map_err(|error| match error {
            PublishError::Oversize { size, limit } => oversize_result(size, limit),
            other => other.to_string(),
        })
}

/// The TTL a task advertises and the manager enforces, as a hard abort at
/// `created + ttl`, fixed when the task is spawned. The manager keeps the
/// task, terminal state included, for one more TTL after it ends.
///
/// Under a whole-goal deadline it is that deadline plus
/// [`TASK_TTL_GRACE_MS`], so the sweep lands after the deadline this
/// runtime enforces and never races it.
///
/// Under a progress bound it is [`PROGRESS_TASK_TTL_MS`]. The goal's bridge
/// fails it once it goes a whole window without a sign of progress, long
/// before that; the TTL ends, with the manager's generic expiry, only a goal
/// that keeps making progress for a whole day. The one wait before the
/// goal, the confirmation, is refused together with a progress bound.
fn task_ttl_ms(bound: GoalBound) -> u64 {
    match bound {
        GoalBound::WholeGoal { deadline_ms } => deadline_ms.get().saturating_add(TASK_TTL_GRACE_MS),
        GoalBound::Progress { .. } => PROGRESS_TASK_TTL_MS,
    }
}

/// Runs a goal's operation under the goal's bound. A whole-goal deadline is
/// enforced here, and the error is the failure message of an overrun. A
/// progress-bound goal runs to its end: its bridge, the one layer that sees
/// every sign of progress, bounds the time between two of them, and a
/// timeout here would cut a goal that still makes progress (and, dropping
/// the bridge, leave the provider without a cancel). Its total time is
/// bounded outside: in a task by the TTL (see [`task_ttl_ms`]), in a call by
/// the client, which decides how long it waits for the call.
async fn within_goal_bound<T>(
    bound: GoalBound,
    operation: impl Future<Output = T>,
) -> Result<T, String> {
    match bound {
        GoalBound::WholeGoal { deadline_ms } => {
            let deadline = Duration::from_millis(deadline_ms.get());
            tokio::time::timeout(deadline, operation)
                .await
                .map_err(|_elapsed| deadline_exceeded(deadline))
        }
        GoalBound::Progress { .. } => Ok(operation.await),
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

/// The whole task operation: the optional confirmation gate and the
/// bridge, under the goal's bound (see [`within_goal_bound`]). Enforcing a
/// whole-goal deadline here (rather than leaving it to the manager's TTL
/// sweep) makes it prompt and gives the failure a descriptive message.
async fn run_task_operation(
    task: Arc<TaskState>,
    call: ToolCall,
    context: TaskContext,
    mut recording: Recording,
) -> Result<CallToolResult, TaskExit> {
    let bound = task.entry.bound;
    match within_goal_bound(bound, drive_task(task, call, context, &mut recording)).await {
        Ok(result) => result,
        Err(overrun) => {
            recording.failed(overrun.clone());
            Err(TaskExit::Error(McpError::internal_error(overrun, None)))
        }
    }
}

/// Identifier of the confirmation entry in the task's `inputRequests`.
const CONFIRMATION_INPUT_KEY: &str = "confirmation";

/// What the record says of a task whose confirmation was not accepted.
const CONFIRMATION_DECLINED: &str = "the confirmation was declined: the goal never ran";

async fn drive_task(
    task: Arc<TaskState>,
    call: ToolCall,
    context: TaskContext,
    recording: &mut Recording,
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
            recording.cancelled(CONFIRMATION_DECLINED.to_string());
            return Err(TaskExit::Cancelled);
        }
    }

    let action_context = ActionContext {
        surface: ActionSurface::Task(context.clone()),
    };
    let outcome = task.handler.start(call, action_context).await;
    recording.ended_by(&outcome);
    match outcome {
        Ok(value) => Ok(CallToolResult::structured(value)),
        // A `cancelled` task carries no result: its status message says
        // why, with the result the provider ended the goal with.
        Err(ActionExit::Cancelled(goal)) => {
            context.set_status_message(goal.status_message());
            Err(TaskExit::Cancelled)
        }
        Err(ActionExit::Failed(message)) => {
            Err(TaskExit::Error(McpError::internal_error(message, None)))
        }
    }
}

/// The listing tool of a per-robot surface: it takes nothing and answers one
/// entry per robot.
fn listing_tool(fleet: &FleetRuntime) -> Tool {
    let mut entry_properties = serde_json::Map::from_iter([
        (
            ROBOT_ARGUMENT.to_string(),
            json!({ "type": "string", "description": "The robot's name, which every other tool takes." }),
        ),
        (
            "tools".to_string(),
            json!({ "type": "array", "items": { "type": "string" }, "description": "The tools that answer for this robot, each called with its name." }),
        ),
        (
            "resources".to_string(),
            json!({ "type": "array", "items": { "type": "string" }, "description": "The resources this robot publishes, each read at `peppy://resource/<name>`." }),
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
                    "required": [ROBOT_ARGUMENT, "tools", "resources", "members", "notes"],
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
    .with_annotations(read_only_annotations())
    .with_raw_output_schema(Arc::new(output))
}

impl ExposureServer {
    /// Turns a validated input into the call a bridge runs: on a per-robot
    /// surface the routing arguments are taken out of the input and resolved
    /// to the member the robot fills `target` with; on a fixed surface the
    /// input is the call.
    fn route(&self, target: &str, mut input: Value) -> Result<ToolCall, McpError> {
        let Addressing::PerRobot(fleet) = &self.state.addressing else {
            return Ok(ToolCall {
                input,
                recipient: Recipient::BoundProducer,
            });
        };
        let (robot, name) = take_routing(fleet, target, &mut input);
        let member = fleet
            .fleet()
            .route(&robot, target, name.as_deref())
            .map_err(|refusal| McpError::invalid_params(refusal.to_string(), None))?;
        Ok(ToolCall {
            input,
            recipient: Recipient::Member(member),
        })
    }

    /// Answers the listing tool: one entry per robot with the tools and
    /// resources it answers, its named members, and each `describe` value
    /// read through the robot's own tools and resources, every robot read at
    /// once so the listing takes one robot's describe deadlines at most.
    async fn list_robots(
        &self,
        fleet: &FleetRuntime,
        arguments: JsonObject,
    ) -> Result<CallToolResult, McpError> {
        refuse_arguments(
            &fleet.catalog.list.name,
            "lists every robot of the stack",
            &arguments,
        )?;
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
            .listing_entry(&robot, &fleet.by_target)
            .expect("the robot was read from this snapshot");
        let mut notes: Vec<Value> = Vec::new();
        for describe in &fleet.catalog.describe {
            let value = self
                .describe_through_tool(snapshot, &robot, &describe.target, &describe.tool)
                .await;
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
            recipient: Recipient::Member(member),
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
}

/// Takes the routing arguments of a call on `target` out of its validated
/// input: the robot's name, and the member's name on a target a robot fills
/// any number of times.
fn take_routing(fleet: &FleetRuntime, target: &str, input: &mut Value) -> (String, Option<String>) {
    let argument = fleet.argument_of.get(target).cloned().flatten();
    let fields = input.as_object_mut().expect("validated input is an object");
    let robot = take_string(fields, ROBOT_ARGUMENT);
    let name = argument
        .as_deref()
        .map(|argument| take_string(fields, argument));
    (robot, name)
}

/// Takes the string field `name` out of a validated object; the schema
/// made it required and a string.
fn take_string(fields: &mut JsonObject, name: &str) -> String {
    match fields.remove(name) {
        Some(Value::String(value)) => value,
        _ => unreachable!("the input schema requires `{name}` as a string"),
    }
}

/// The refusal of arguments to `tool`, which takes none, `purpose` saying
/// what the tool does instead.
fn refuse_arguments(tool: &str, purpose: &str, arguments: &JsonObject) -> Result<(), McpError> {
    if arguments.is_empty() {
        return Ok(());
    }
    Err(McpError::invalid_params(
        format!("`{tool}` takes no arguments; it {purpose}"),
        None,
    ))
}

/// A tool result held to the entry's `max_result_bytes`.
fn within_result_limit(entry: &ToolEntry, result: Value) -> Result<Value, String> {
    if let Some(limit) = entry.max_result_bytes {
        let size = serialized_len(&result);
        if size > limit.get() {
            return Err(oversize_result(size, limit.get()));
        }
    }
    Ok(result)
}

/// The refusal of a tool result larger than its entry allows.
fn oversize_result(size: u64, limit: u64) -> String {
    format!("result of {size} bytes exceeds the {limit} byte limit")
}

/// How the host keeps a per-robot server's resources in step with the
/// members that run: it attaches each member it starts feeding, detaches
/// each one that leaves, and says when the list changed.
#[derive(Clone)]
pub struct FleetHandle {
    fleet: Arc<FleetRuntime>,
    events: broadcast::Sender<CatalogEvent>,
    clock: Clock,
}

impl FleetHandle {
    /// Registers `member`'s resources and hands back the ingest feeding each
    /// one, with the catalog entry it publishes.
    pub fn attach(&self, member: &FleetMember) -> Vec<(ResourceEntry, ResourceIngest)> {
        self.fleet
            .attach(member)
            .into_iter()
            .map(|(entry, state)| {
                (
                    entry,
                    ResourceIngest {
                        state,
                        events: self.events.clone(),
                        clock: self.clock.clone(),
                    },
                )
            })
            .collect()
    }

    /// Drops `member`'s resources.
    pub fn detach(&self, member: &FleetMember) {
        self.fleet.detach(member);
    }

    /// Tells listening clients the resource list changed.
    pub fn changed(&self) {
        // Send fails only when nobody listens, which is fine.
        let _ = self.events.send(CatalogEvent::ResourceListChanged);
    }

    /// The members the surface cannot serve as the fleet stands now, each
    /// with the reason.
    pub fn problems(&self) -> Vec<String> {
        self.fleet.fleet().problems().to_vec()
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
        match &self.state.addressing {
            Addressing::Fixed(resources) => {
                Ok(ListResourcesResult::with_all_items(resources.list.clone())
                    .with_ttl_ms(CATALOG_TTL_MS)
                    .with_cache_scope(CacheScope::Private))
            }
            Addressing::PerRobot(fleet) => Ok(ListResourcesResult::with_all_items(
                fleet.fleet().resources(&fleet.entries),
            )),
        }
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        self.read_snapshot(&request.uri).await.map(Into::into)
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
        if let Addressing::PerRobot(fleet) = &self.state.addressing
            && name == fleet.catalog.list.name
        {
            return self.list_robots(fleet, arguments).await.map(Into::into);
        }
        if let Some(record) = &self.state.record
            && name == record.name()
        {
            return self.recent_calls(record, arguments).map(Into::into);
        }
        if let Some(picture) = self.state.pictures.get(name) {
            return self.look(picture, arguments).await.map(Into::into);
        }
        let client = ClientIdentity::of(context.client_info());
        let Some(task) = self.state.tasks.get(name) else {
            return self
                .execute_tool(name, arguments, client)
                .await
                .map(Into::into);
        };
        self.run_action(task, arguments, client, context).await
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
    use crate::clock::test_support::{MS, manual_clock};
    use crate::tasks::CancelledGoal;
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
        server.start_task(&task_named(server, name), arguments, None)
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
            .run_action_in_call(&task_named(server, name), arguments, None, cancel, None)
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

    /// The result a provider ends a cancelled `recorder.resume_session`
    /// goal with.
    fn cancelled_resume() -> Value {
        json!({ "resumed": false })
    }

    #[tokio::test]
    async fn closing_the_call_cancels_the_goal_and_the_cancelled_exit_is_a_tool_error_with_the_result()
     {
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
                        Err(ActionExit::Cancelled(CancelledGoal {
                            result: cancelled_resume(),
                            reason: None,
                        }))
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
        assert_eq!(result.structured_content, Some(cancelled_resume()));
        assert_eq!(
            result.content.get(1),
            Some(&ContentBlock::text(cancelled_resume().to_string())),
            "the provider's result is also the text after the summary"
        );
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
                recipient: Recipient::BoundProducer,
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
    async fn feedback_reports_as_the_status_message_and_a_cancelled_goal_cancels_the_task_saying_why()
     {
        let server = ExposureServer::builder(task_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, context: crate::tasks::ActionContext| async move {
                    context.report_feedback("resuming at frame 42");
                    context.cancel_requested().await;
                    Err(ActionExit::Cancelled(CancelledGoal {
                        result: cancelled_resume(),
                        reason: Some("no progress within 2000 ms".to_owned()),
                    }))
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
        assert_eq!(
            task.task.status_message,
            Some(format!(
                "the action was cancelled: no progress within 2000 ms: {}",
                cancelled_resume()
            ))
        );
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

    /// The progress window of the goals below, the same value as the
    /// whole-goal deadline of `task_bundle`'s `recorder.resume_session`.
    const PROGRESS_WINDOW_MS: u64 = 2000;

    /// `task_bundle` with `recorder.resume_session` bounded by its progress
    /// rather than by a whole-goal deadline.
    fn progress_bundle() -> ExposureBundle {
        let mut bundle = task_bundle();
        let resume = bundle
            .tasks
            .iter_mut()
            .find(|task| task.name == "recorder.resume_session")
            .expect("the bundle exposes resume_session");
        resume.bound = GoalBound::Progress {
            window_ms: std::num::NonZeroU64::new(PROGRESS_WINDOW_MS).expect("nonzero"),
        };
        bundle
    }

    /// A server over `bundle` whose `recorder.resume_session` goal reports
    /// that it runs, notifies `started`, then holds until `gate` is
    /// notified, however long the clock has run by then. The runtime sees
    /// no sign of progress in that time: bounding it is the bridge's part,
    /// which this handler stands in for.
    fn server_with_gated_resume(
        bundle: ExposureBundle,
        started: Arc<tokio::sync::Notify>,
        gate: Arc<tokio::sync::Notify>,
    ) -> ExposureServer {
        ExposureServer::builder(bundle)
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_task("recorder.record_episode", record_handler)
            .with_task(
                "recorder.resume_session",
                move |_call: ToolCall, context: crate::tasks::ActionContext| {
                    let started = Arc::clone(&started);
                    let gate = Arc::clone(&gate);
                    async move {
                        context.report_feedback("resuming");
                        started.notify_one();
                        gate.notified().await;
                        Ok(json!({ "resumed": true }))
                    }
                },
            )
            .build()
            .expect("builds")
    }

    /// Ten windows of the progress-bound goals: far past any whole-goal
    /// deadline of the same value.
    const TEN_WINDOWS: Duration = Duration::from_millis(10 * PROGRESS_WINDOW_MS);

    #[tokio::test(start_paused = true)]
    async fn a_progress_bound_goal_inside_a_call_is_not_cut_by_a_whole_goal_timeout() {
        let started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let server =
            server_with_gated_resume(progress_bundle(), Arc::clone(&started), Arc::clone(&gate));
        let mut call = std::pin::pin!(run_in_call(
            &server,
            "recorder.resume_session",
            JsonObject::new(),
            tokio_util::sync::CancellationToken::new(),
        ));
        tokio::select! {
            biased;
            () = started.notified() => {}
            result = &mut call => panic!("the goal settled before it ran: {result:?}"),
        }

        tokio::time::advance(TEN_WINDOWS).await;
        if let std::task::Poll::Ready(result) = futures::poll!(&mut call) {
            panic!("the runtime cut the goal after ten windows: {result:?}");
        }
        gate.notify_one();
        let result = call.await.expect("the call answers with the goal's result");
        assert_eq!(result.is_error, Some(false), "{result:?}");
        assert_eq!(result.structured_content, Some(json!({ "resumed": true })));
    }

    #[tokio::test(start_paused = true)]
    async fn a_progress_bound_task_lives_one_day_and_is_not_cut_by_a_whole_goal_timeout() {
        let started = Arc::new(tokio::sync::Notify::new());
        let gate = Arc::new(tokio::sync::Notify::new());
        let server =
            server_with_gated_resume(progress_bundle(), Arc::clone(&started), Arc::clone(&gate));
        let created = start_task(&server, "recorder.resume_session", JsonObject::new())
            .expect("the task starts");
        let task_id = created.task.task_id;
        started.notified().await;

        tokio::time::advance(TEN_WINDOWS).await;
        // Lets a deadline that fired act before the task is read.
        tokio::task::yield_now().await;
        let running = server
            .state
            .manager
            .get_task(&task_id)
            .expect("task exists");
        assert!(
            !running.status().is_terminal(),
            "the runtime cut the task after ten windows: {:?}",
            running.payload
        );
        gate.notify_one();
        let task = settled(&server, &task_id).await;
        let rmcp::model::TaskPayload::Completed { result } = task.payload else {
            panic!("expected a completed task, got {:?}", task.payload);
        };
        assert_eq!(result["structuredContent"], json!({ "resumed": true }));
        assert_eq!(
            created.task.ttl_ms,
            Some(24 * 60 * 60 * 1000),
            "a progress-bound task runs for at most one day"
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
        use peppy_mcp_catalog::{BundleContractPin, RobotContractPin};
        use std::sync::Mutex;

        fn bundle() -> ExposureBundle {
            ExposureBundle::from_json_str(
                r#"{
  "bundle_format": 1,
  "schema_mapping_version": 1,
  "exposure": { "name": "robot_control", "tag": "v1" },
  "server": { "title": "Robots" },
  "robots": {
    "list": { "name": "robot.list", "description": "The robots of the stack." },
    "describe": [
      { "key": "identity", "target": "status", "tool": "robot.get_identity" }
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

        /// A server of `bundle()` over a fleet the test edits, whose tools
        /// answer with the member they were routed to.
        fn served() -> (ExposureServer, Arc<Mutex<Vec<FleetMember>>>) {
            served_bundle(bundle())
        }

        /// `bundle()` with the camera's frame under a `jpeg` representation
        /// and the picture tool that answers with it.
        fn picture_bundle() -> ExposureBundle {
            let mut bundle = bundle();
            bundle.resources[1].policies.representation = Some(
                serde_json::from_value(json!({
                    "image": "jpeg",
                    "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
                }))
                .expect("a representation"),
            );
            bundle.pictures.push(
                serde_json::from_value(json!({
                    "name": "camera.look",
                    "description": "Look through the camera.",
                    "target": "camera",
                    "member": "video_stream",
                    "resource": "camera.latest_frame",
                    "input_schema": {
                        "type": "object",
                        "properties": { "robot": { "type": "string" }, "camera": { "type": "string" } },
                        "required": ["robot", "camera"],
                        "additionalProperties": false
                    },
                    "output_schema": { "type": "object" }
                }))
                .expect("valid picture entry"),
            );
            bundle
        }

        /// `bundle` with a `depth_camera` target, the depth side of an RGBD
        /// camera: a robot fills it any number of times, and a call names
        /// its member with `camera`, as on the `camera` target.
        fn with_depth_camera(mut bundle: ExposureBundle) -> ExposureBundle {
            let BundleSurface::PerRobot { contracts, .. } = &mut bundle.surface else {
                panic!("a per-robot bundle");
            };
            contracts.push(RobotContractPin {
                pin: BundleContractPin {
                    name: "depth_camera".to_string(),
                    tag: "v1".to_string(),
                    sha256: "cc".to_string(),
                    link_id: "depth_camera".to_string(),
                },
                argument: Some("camera".to_string()),
            });
            bundle
        }

        /// Attaches `camera` and publishes one frame of it, numbered
        /// `frame_id`.
        fn publish_frame_of(server: &ExposureServer, camera: &FleetMember, frame_id: u32) {
            let handle = server.fleet().expect("a per-robot server");
            let attached = handle.attach(camera);
            let (_, ingest) = &attached[0];
            let mut frame = rgb8_frame();
            frame["header"]["frame_id"] = json!(frame_id);
            let token = ingest.admit().expect("gate open");
            ingest.publish(token, frame).expect("frame publishes");
        }

        async fn look(
            server: &ExposureServer,
            arguments: Value,
        ) -> Result<CallToolResult, McpError> {
            server
                .look(
                    &server.state.pictures["camera.look"],
                    self::arguments(arguments),
                )
                .await
        }

        fn served_bundle(bundle: ExposureBundle) -> (ExposureServer, Arc<Mutex<Vec<FleetMember>>>) {
            let fleet = Arc::new(Mutex::new(vec![
                member("status", "alpha", "backbone_inst"),
                member("camera", "alpha", "wrist_left"),
                member("status", "bravo", "backbone_inst"),
            ]));
            let source = Arc::clone(&fleet);
            let server = ExposureServer::builder(bundle)
                .with_fleet(move || source.lock().unwrap().clone())
                .with_tool("robot.get_identity", |call: ToolCall| async move {
                    let Recipient::Member(member) = call.recipient else {
                        panic!("a per-robot call names its member");
                    };
                    Ok(json!({ "robot": member.instance_id, "input": call.input }))
                })
                .with_tool("camera.set_brightness", |call: ToolCall| async move {
                    let Recipient::Member(member) = call.recipient else {
                        panic!("a per-robot call names its member");
                    };
                    Ok(json!({ "camera": member.instance_id, "input": call.input }))
                })
                .build()
                .expect("bundle and handlers agree");
            (server, fleet)
        }

        fn structured(result: CallToolResult) -> Value {
            result.structured_content.expect("a structured result")
        }

        /// The fleet runtime of a per-robot server, which the listing tests
        /// drive directly.
        fn runtime(server: &ExposureServer) -> &FleetRuntime {
            match &server.state.addressing {
                Addressing::PerRobot(fleet) => fleet,
                Addressing::Fixed(_) => panic!("expected a per-robot server"),
            }
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
                    None,
                )
                .await
                .expect("routes");
            assert_eq!(
                structured(result),
                json!({ "camera": "alpha_wrist_left", "input": { "value": 3 } })
            );
            let result = server
                .execute_tool(
                    "robot.get_identity",
                    arguments(json!({ "robot": "bravo" })),
                    None,
                )
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
                    .execute_tool("camera.set_brightness", self::arguments(arguments), None)
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
                "`alpha` is not a robot of this stack, which has no robot; `peppy stack join \
                 OPTION:NAME` adds one"
            );
        }

        #[tokio::test]
        async fn the_listing_reports_each_robot_with_what_its_targets_answer() {
            let (server, fleet) = served();
            let listed = server
                .list_robots(runtime(&server), JsonObject::new())
                .await
                .expect("lists");
            assert_eq!(
                structured(listed),
                json!({
                    "robots": [
                        {
                            "robot": "alpha",
                            "tools": ["camera.set_brightness", "robot.get_identity"],
                            "resources": [
                                "alpha/robot.status",
                                "alpha/wrist_left/camera.latest_frame",
                            ],
                            "members": { "camera": ["wrist_left"] },
                            "notes": [],
                            "identity": { "robot": "alpha_backbone_inst", "input": {} },
                        },
                        {
                            "robot": "bravo",
                            "tools": ["robot.get_identity"],
                            "resources": ["bravo/robot.status"],
                            "members": {},
                            "notes": [],
                            "identity": { "robot": "bravo_backbone_inst", "input": {} },
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
                .list_robots(runtime(&server), JsonObject::new())
                .await
                .expect("lists");
            assert_eq!(
                structured(listed)["robots"][2],
                json!({
                    "robot": "charlie",
                    "tools": ["camera.set_brightness"],
                    "resources": ["charlie/front/camera.latest_frame"],
                    "members": { "camera": ["front"] },
                    "notes": [
                        "identity: robot `charlie` has no `status`; it fills `camera`; the robots with a \
                         `status` are `alpha`, `bravo`",
                    ],
                    "identity": null,
                })
            );
            let error = server
                .list_robots(runtime(&server), arguments(json!({ "robot": "alpha" })))
                .await
                .expect_err("takes no arguments");
            assert!(error.message.contains("`robot.list` takes no arguments"));
        }

        #[tokio::test(start_paused = true)]
        async fn resources_follow_the_fleet_and_a_change_is_announced() {
            let (server, members) = served();
            let handle = server.fleet().expect("a per-robot server");
            let fleet = runtime(&server);
            let listed: Vec<String> = fleet
                .fleet()
                .resources(&fleet.entries)
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
                .await
                .expect_err("listed, not attached");
            assert!(
                unavailable.message.contains("unavailable"),
                "{}",
                unavailable.message
            );
            let unknown = server
                .read_snapshot("peppy://resource/charlie/robot.status")
                .await
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
                .await
                .expect("attached and published");
            assert_eq!(read.contents.len(), 1);
            assert!(matches!(
                events.try_recv().expect("the publish is announced"),
                CatalogEvent::ResourceUpdated { .. }
            ));

            members
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
                .await
                .expect_err("detached and unlisted");
            assert_eq!(gone.code, ErrorCode::RESOURCE_NOT_FOUND);
        }

        #[tokio::test(start_paused = true)]
        async fn a_picture_tool_answers_for_the_camera_the_call_names() {
            let (server, fleet) = served_bundle(picture_bundle());
            let wrist_left = member("camera", "alpha", "wrist_left");
            let wrist_right = member("camera", "alpha", "wrist_right");
            fleet.lock().unwrap().push(wrist_right.clone());
            publish_frame_of(&server, &wrist_left, 1);
            publish_frame_of(&server, &wrist_right, 2);

            for (camera, frame_id) in [("wrist_left", 1), ("wrist_right", 2)] {
                let result = look(&server, json!({ "robot": "alpha", "camera": camera }))
                    .await
                    .expect("routes to the camera");
                assert_eq!(result.is_error, Some(false));
                assert_eq!(
                    structured(result.clone()),
                    json!({
                        "header": { "frame_id": frame_id },
                        "encoding": "mjpeg",
                        "width": 8,
                        "height": 8,
                    }),
                    "{camera}"
                );
                let image = result.content[0].as_image().expect("the image comes first");
                assert_eq!(image.mime_type, "image/jpeg");
                assert_is_jpeg(&image.data);
            }
        }

        #[tokio::test]
        async fn a_picture_tool_refuses_a_robot_or_a_camera_the_fleet_does_not_have() {
            let (server, _) = served_bundle(picture_bundle());
            let refused = async |arguments: Value| {
                look(&server, arguments)
                    .await
                    .expect_err("refused before any snapshot is read")
            };
            let error = refused(json!({ "robot": "charlie", "camera": "wrist_left" })).await;
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error.message,
                "`charlie` is not a robot of this stack; the robots are `alpha`, `bravo`"
            );
            let error = refused(json!({ "robot": "alpha", "camera": "chest" })).await;
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error.message,
                "robot `alpha` has no `camera` named `chest` (`camera`); it has `wrist_left`"
            );
            let error = refused(json!({ "robot": "bravo", "camera": "wrist_left" })).await;
            assert_eq!(
                error.message,
                "robot `bravo` has no `camera`; it fills `status`; the robots with a `camera` are `alpha`"
            );
            let error = refused(json!({ "robot": "alpha" })).await;
            assert!(
                error
                    .message
                    .contains("invalid arguments for `camera.look`"),
                "{}",
                error.message
            );
        }

        #[tokio::test]
        async fn a_picture_tool_names_the_other_targets_the_robot_fills_with_a_refused_camera() {
            let (server, fleet) = served_bundle(with_depth_camera(picture_bundle()));
            fleet
                .lock()
                .unwrap()
                .push(member("depth_camera", "alpha", "chest"));
            let error = look(&server, json!({ "robot": "alpha", "camera": "chest" }))
                .await
                .expect_err("`chest` fills `depth_camera` and not `camera`");
            assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
            assert_eq!(
                error.message,
                "robot `alpha` has no `camera` named `chest` (`camera`); it has `wrist_left`; its \
                 `chest` fills `depth_camera`"
            );
        }

        #[tokio::test]
        async fn a_picture_tool_of_a_camera_with_no_snapshot_is_a_tool_error_in_the_words_of_the_read()
         {
            let (server, _) = served_bundle(picture_bundle());
            let uri = "peppy://resource/alpha/wrist_left/camera.latest_frame";
            let look = async || {
                look(&server, json!({ "robot": "alpha", "camera": "wrist_left" }))
                    .await
                    .expect("a snapshot that does not serve is a tool error")
            };
            let read_refusal = async || {
                server
                    .read_snapshot(uri)
                    .await
                    .expect_err("the read is refused")
                    .message
                    .into_owned()
            };

            // Listed, and the host has not attached the member yet.
            let unattached = look().await;
            assert_eq!(tool_error_text(&unattached), read_refusal().await);
            assert_eq!(
                tool_error_text(&unattached),
                "resource `peppy://resource/alpha/wrist_left/camera.latest_frame` is \
                 unavailable: nothing has been published since the robot joined"
            );

            // Attached, and no frame has arrived yet.
            let handle = server.fleet().expect("a per-robot server");
            handle.attach(&member("camera", "alpha", "wrist_left"));
            let unpublished = look().await;
            assert_eq!(tool_error_text(&unpublished), read_refusal().await);
            assert!(
                tool_error_text(&unpublished).contains("since the server started"),
                "{}",
                tool_error_text(&unpublished)
            );
        }

        #[tokio::test]
        async fn the_listing_reports_a_picture_tool_for_the_robots_that_fill_its_target() {
            let (server, _) = served_bundle(picture_bundle());
            let names: Vec<&str> = server
                .state
                .tool_list
                .iter()
                .map(|tool| tool.name.as_ref())
                .collect();
            assert_eq!(
                names,
                [
                    "robot.get_identity",
                    "camera.set_brightness",
                    "camera.look",
                    "robot.list"
                ]
            );
            let listed = structured(
                server
                    .list_robots(runtime(&server), JsonObject::new())
                    .await
                    .expect("lists"),
            );
            assert_eq!(
                listed["robots"][0]["tools"],
                json!(["camera.look", "camera.set_brightness", "robot.get_identity"]),
                "alpha fills the camera target"
            );
            assert_eq!(
                listed["robots"][1]["tools"],
                json!(["robot.get_identity"]),
                "bravo has no camera"
            );
            let resources = runtime(&server)
                .fleet()
                .resources(&runtime(&server).entries);
            let listed_mime_types: Vec<(&str, Option<&str>)> = resources
                .iter()
                .map(|resource| (resource.name.as_str(), resource.mime_type.as_deref()))
                .collect();
            assert_eq!(
                listed_mime_types,
                [
                    ("alpha/robot.status", Some("application/json")),
                    ("alpha/wrist_left/camera.latest_frame", Some("image/jpeg")),
                    ("bravo/robot.status", Some("application/json")),
                ]
            );
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
                "tools",
                "resources",
                "members",
                "notes",
                "identity",
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
        let Addressing::Fixed(resources) = &server.state.addressing else {
            panic!("expected a fixed server");
        };
        let resource = &resources.list[0];
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
            .execute_tool("front_camera.set_gain", JsonObject::new(), None)
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
                .execute_tool("front_camera.set_brightness", arguments(raw.clone()), None)
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
                None,
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
                None,
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
                None,
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
                None,
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

    #[tokio::test(start_paused = true)]
    async fn reads_walk_unavailable_fresh_and_stale_with_freshness_as_ttl() {
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
            .await
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

        // The topic stays silent: the read answers the stored snapshot once
        // the freshness bound has passed (in paused time, at once).
        nanos.store(500 * 1_000_000, Ordering::SeqCst);
        let read = server
            .read_snapshot(uri)
            .await
            .expect("fresh snapshot serves");
        assert_eq!(read.ttl_ms, Some(1500), "ttl is the remaining freshness");
        assert_eq!(read.cache_scope, Some(CacheScope::Private));

        nanos.store(2_500 * 1_000_000, Ordering::SeqCst);
        let error = server
            .read_snapshot(uri)
            .await
            .expect_err("2500 ms old is stale");
        assert_eq!(error.code, ErrorCode::INTERNAL_ERROR);
        assert!(error.message.contains("stale"), "got {}", error.message);
        assert!(
            error.message.contains("2500 ms old"),
            "got {}",
            error.message
        );
    }

    #[tokio::test]
    async fn reading_an_unknown_uri_is_resource_not_found() {
        let error = built_server()
            .read_snapshot("peppy://resource/absent")
            .await
            .expect_err("absent resources are refused");
        assert_eq!(error.code, ErrorCode::RESOURCE_NOT_FOUND);
    }

    const FRAME_URI: &str = "peppy://resource/front_camera.latest_frame";

    /// `test_bundle` plus a frame resource under a `jpeg` representation and
    /// the picture tool that answers with it.
    fn picture_bundle() -> ExposureBundle {
        let mut bundle = test_bundle();
        bundle.resources.push(
            serde_json::from_value(json!({
                "name": "front_camera.latest_frame",
                "uri": FRAME_URI,
                "description": "Latest frame from the front-facing camera.",
                "target": "front_camera",
                "member": "video_stream",
                "policies": {
                    "freshness": { "max_age_ms": 2000 },
                    "update": { "max_hz": 2.0 },
                    "representation": {
                        "image": "jpeg",
                        "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
                    },
                    "max_result_bytes": 524288,
                    "on_oversize": "downscale"
                },
                "schema": { "type": "object" }
            }))
            .expect("valid resource entry"),
        );
        bundle.pictures.push(
            serde_json::from_value(json!({
                "name": "front_camera.look",
                "description": "Look through the front-facing camera.",
                "target": "front_camera",
                "member": "video_stream",
                "resource": "front_camera.latest_frame",
                "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
                "output_schema": { "type": "object" }
            }))
            .expect("valid picture entry"),
        );
        bundle
    }

    /// A picture server on a clock the test drives.
    fn built_picture_server() -> (ExposureServer, Arc<std::sync::atomic::AtomicU64>) {
        let (clock, nanos) = manual_clock();
        let server = ExposureServer::builder(picture_bundle())
            .with_clock(clock)
            .with_tool("front_camera.set_brightness", brightness_handler)
            .build()
            .expect("bundle and handlers agree");
        (server, nanos)
    }

    /// An 8x8 `rgb8` frame, as the bridge hands a camera message over.
    fn rgb8_frame() -> Value {
        use base64::Engine as _;
        let pixels: Vec<u8> = (0..8u32 * 8 * 3).map(|index| (index % 251) as u8).collect();
        json!({
            "header": { "frame_id": 7 },
            "frame": base64::engine::general_purpose::STANDARD.encode(&pixels),
            "encoding": "rgb8",
            "width": 8,
            "height": 8,
        })
    }

    /// The document every snapshot of [`rgb8_frame`] serves.
    fn frame_document() -> Value {
        json!({ "header": { "frame_id": 7 }, "encoding": "mjpeg", "width": 8, "height": 8 })
    }

    fn publish_frame(server: &ExposureServer) {
        let ingest = server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let token = ingest.admit().expect("gate open");
        ingest
            .publish(token, rgb8_frame())
            .expect("frame publishes");
    }

    fn assert_is_jpeg(base64_text: &str) {
        use base64::Engine as _;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(base64_text)
            .expect("the frame is base64");
        assert_eq!(&bytes[..2], &[0xFF, 0xD8], "JPEG magic bytes");
    }

    #[test]
    fn a_resource_with_a_representation_is_listed_under_the_mime_type_of_its_blob() {
        let (server, _) = built_picture_server();
        let Addressing::Fixed(resources) = &server.state.addressing else {
            panic!("expected a fixed server");
        };
        let listed: Vec<(&str, Option<&str>)> = resources
            .list
            .iter()
            .map(|resource| (resource.name.as_str(), resource.mime_type.as_deref()))
            .collect();
        assert_eq!(
            listed,
            [
                ("front_camera.status", Some("application/json")),
                ("front_camera.latest_frame", Some("image/jpeg")),
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_serves_the_document_then_the_blob_under_the_resources_uri() {
        let (server, _) = built_picture_server();
        publish_frame(&server);
        let read = server
            .read_snapshot(FRAME_URI)
            .await
            .expect("the frame serves");
        let [document, blob] = read.contents.as_slice() else {
            panic!("expected two contents, got {:?}", read.contents);
        };
        let ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } = document
        else {
            panic!("the document comes first, got {document:?}");
        };
        assert_eq!(uri, FRAME_URI);
        assert_eq!(mime_type.as_deref(), Some("application/json"));
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("the document is JSON"),
            frame_document()
        );
        let ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } = blob
        else {
            panic!("the blob comes second, got {blob:?}");
        };
        assert_eq!(uri, FRAME_URI);
        assert_eq!(mime_type.as_deref(), Some("image/jpeg"));
        assert_is_jpeg(blob);

        // A resource with no representation serves its document alone.
        let status = server
            .ingest("front_camera.status")
            .expect("resource exists");
        let token = status.admit().expect("gate open");
        status
            .publish(token, json!({ "battery": 87 }))
            .expect("publishes");
        let read = server
            .read_snapshot("peppy://resource/front_camera.status")
            .await
            .expect("the status serves");
        assert_eq!(
            read.contents,
            [
                ResourceContents::text("{\"battery\":87}", "peppy://resource/front_camera.status")
                    .with_mime_type("application/json")
            ]
        );
    }

    #[test]
    fn picture_tools_join_the_catalog_read_only_after_the_tools_and_the_tasks() {
        let (server, _) = built_picture_server();
        let names: Vec<&str> = server
            .state
            .tool_list
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(names, ["front_camera.set_brightness", "front_camera.look"]);
        let look = server.get_tool("front_camera.look").expect("listed");
        assert_eq!(
            look.description.as_deref(),
            Some("Look through the front-facing camera.")
        );
        assert_eq!(
            Value::Object((*look.input_schema).clone()),
            json!({ "type": "object", "properties": {}, "additionalProperties": false })
        );
        assert!(look.output_schema.is_some());
        let annotations = look.annotations.expect("annotations set");
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
    }

    #[tokio::test(start_paused = true)]
    async fn a_picture_tool_answers_with_the_image_the_text_and_the_structured_document() {
        let (server, _) = built_picture_server();
        publish_frame(&server);
        let result = server
            .look(
                &server.state.pictures["front_camera.look"],
                JsonObject::new(),
            )
            .await
            .expect("the picture tool answers");
        assert_eq!(result.is_error, Some(false));
        assert_eq!(result.structured_content, Some(frame_document()));
        let [image, text] = result.content.as_slice() else {
            panic!("expected an image and a text, got {:?}", result.content);
        };
        let image = image.as_image().expect("the image comes first");
        assert_eq!(image.mime_type, "image/jpeg");
        assert_is_jpeg(&image.data);
        let text = &text.as_text().expect("the document comes second").text;
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("the text is JSON"),
            frame_document()
        );

        // The image is the blob a read of the resource serves.
        let read = server
            .read_snapshot(FRAME_URI)
            .await
            .expect("the frame serves");
        let ResourceContents::BlobResourceContents { blob, .. } = &read.contents[1] else {
            panic!("the blob comes second");
        };
        assert_eq!(&image.data, blob);
    }

    #[tokio::test]
    async fn a_picture_tool_without_a_fresh_snapshot_is_a_tool_error_in_the_words_of_the_read() {
        let (server, nanos) = built_picture_server();
        let look = async || {
            server
                .look(
                    &server.state.pictures["front_camera.look"],
                    JsonObject::new(),
                )
                .await
                .expect("a snapshot that does not serve is a tool error, not a protocol error")
        };
        let read_refusal = async || {
            server
                .read_snapshot(FRAME_URI)
                .await
                .expect_err("the read is refused")
                .message
                .into_owned()
        };

        let unavailable = look().await;
        assert_eq!(unavailable.is_error, Some(true));
        assert_eq!(unavailable.structured_content, None);
        assert_eq!(tool_error_text(&unavailable), read_refusal().await);
        assert_eq!(
            tool_error_text(&unavailable),
            "resource `peppy://resource/front_camera.latest_frame` is unavailable: nothing has \
             been published since the server started"
        );

        publish_frame(&server);
        nanos.store(2_500 * 1_000_000, Ordering::SeqCst);
        let stale = look().await;
        assert_eq!(stale.is_error, Some(true));
        assert_eq!(tool_error_text(&stale), read_refusal().await);
        assert_eq!(
            tool_error_text(&stale),
            "resource `peppy://resource/front_camera.latest_frame` is stale: the snapshot is \
             2500 ms old and `max_age_ms` is 2000"
        );
    }

    #[tokio::test]
    async fn a_picture_tool_of_a_fixed_surface_takes_no_argument() {
        let (server, _) = built_picture_server();
        publish_frame(&server);
        let error = server
            .look(
                &server.state.pictures["front_camera.look"],
                arguments(json!({ "camera": "front" })),
            )
            .await
            .expect_err("an argument is refused");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(
            error
                .message
                .contains("invalid arguments for `front_camera.look`"),
            "{}",
            error.message
        );
    }

    #[test]
    fn a_picture_needs_a_jpeg_resource_of_its_target_and_a_name_of_its_own() {
        let no_picture_resource = |edit: fn(&mut ExposureBundle)| {
            let mut bundle = picture_bundle();
            edit(&mut bundle);
            ExposureServer::builder(bundle)
                .with_tool("front_camera.set_brightness", brightness_handler)
                .build()
                .expect_err("the picture has no picture to answer with")
        };

        let unknown = no_picture_resource(|bundle| {
            bundle.pictures[0].resource = "front_camera.absent".to_string();
        });
        assert_eq!(
            unknown,
            BuildError::NoPictureResource {
                name: "front_camera.look".to_string(),
                resource: "front_camera.absent".to_string(),
                target: "front_camera".to_string(),
            }
        );
        assert_eq!(
            unknown.to_string(),
            "picture tool `front_camera.look` answers with `front_camera.absent`, which is not a \
             resource of target `front_camera` with a `jpeg` representation"
        );

        // A resource with no representation holds no picture.
        let no_representation = no_picture_resource(|bundle| {
            bundle.pictures[0].resource = "front_camera.status".to_string();
        });
        assert!(matches!(
            no_representation,
            BuildError::NoPictureResource { .. }
        ));

        // Nor does one whose frames are data: 16-bit samples.
        let png16 = no_picture_resource(|bundle| {
            bundle.resources[1].policies.representation = Some(
                serde_json::from_value(json!({
                    "image": "png16",
                    "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
                }))
                .expect("a representation"),
            );
        });
        assert!(matches!(png16, BuildError::NoPictureResource { .. }));

        // The resource of another target is not the picture's.
        let other_target = no_picture_resource(|bundle| {
            bundle.pictures[0].target = "recorder".to_string();
        });
        assert!(matches!(other_target, BuildError::NoPictureResource { .. }));

        let duplicate = no_picture_resource(|bundle| {
            bundle.pictures[0].name = "front_camera.set_brightness".to_string();
        });
        assert_eq!(
            duplicate,
            BuildError::DuplicateName {
                name: "front_camera.set_brightness".to_string()
            }
        );
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

    const STATUS_URI: &str = "peppy://resource/front_camera.status";

    fn publish_status(server: &ExposureServer, battery: u32) {
        let ingest = server
            .ingest("front_camera.status")
            .expect("resource exists");
        let token = ingest.admit().expect("the gate admits the message");
        ingest
            .publish(token, json!({ "battery": battery }))
            .expect("publishes");
    }

    /// Runs `read` on a task of its own and yields to it once, so it stands
    /// registered as a waiting reader when the caller publishes next.
    async fn read_in_a_task<T: Send + 'static>(
        read: impl Future<Output = T> + Send + 'static,
    ) -> tokio::task::JoinHandle<T> {
        let task = tokio::spawn(read);
        tokio::task::yield_now().await;
        task
    }

    #[tokio::test(start_paused = true)]
    async fn a_read_answers_with_the_message_that_arrives_after_it() {
        let (server, nanos) = built_picture_server();
        let mut events = server.state.events.subscribe();
        publish_status(&server, 87);
        assert!(matches!(
            events.try_recv(),
            Ok(CatalogEvent::ResourceUpdated { .. })
        ));

        // 100 ms into the 500 ms gate interval, a read arrives, then a
        // message: the message is admitted for the reader, answers it, and
        // is announced to no subscriber.
        nanos.store(100 * MS, Ordering::SeqCst);
        let reader = server.clone();
        let read = read_in_a_task(async move { reader.read_snapshot(STATUS_URI).await }).await;
        publish_status(&server, 88);
        let read = read
            .await
            .expect("the read task ends")
            .expect("the read answers");
        assert_eq!(
            read.contents,
            [ResourceContents::text("{\"battery\":88}", STATUS_URI)
                .with_mime_type("application/json")]
        );
        assert_eq!(read.ttl_ms, Some(2000));
        assert!(
            events.try_recv().is_err(),
            "a message admitted for a reader is announced to no subscriber"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_picture_tool_answers_with_the_frame_that_arrives_after_it() {
        let (server, nanos) = built_picture_server();
        publish_frame(&server);
        nanos.store(100 * MS, Ordering::SeqCst);
        let looker = server.clone();
        let looked = read_in_a_task(async move {
            looker
                .look(
                    &looker.state.pictures["front_camera.look"],
                    JsonObject::new(),
                )
                .await
        })
        .await;
        let ingest = server
            .ingest("front_camera.latest_frame")
            .expect("resource exists");
        let mut frame = rgb8_frame();
        frame["header"]["frame_id"] = json!(8);
        let token = ingest.admit().expect("a waiting reader opens the gate");
        ingest.publish(token, frame).expect("frame publishes");
        let looked = looked
            .await
            .expect("the look task ends")
            .expect("the picture tool answers");
        assert_eq!(looked.is_error, Some(false));
        assert_eq!(
            looked.structured_content,
            Some(
                json!({ "header": { "frame_id": 8 }, "encoding": "mjpeg", "width": 8, "height": 8 })
            )
        );
    }

    /// `test_bundle` plus a service that answers with a picture, under the
    /// same representation the picture resources take.
    fn snap_bundle() -> ExposureBundle {
        let mut bundle = test_bundle();
        bundle.tools.push(
            serde_json::from_value(json!({
                "name": "front_camera.snap",
                "description": "Take one picture.",
                "target": "front_camera",
                "member": "snap",
                "operation": "read_only",
                "deadline_ms": 2000,
                "representation": {
                    "image": "jpeg",
                    "quality": 80,
                    "fields": { "data": "frame", "encoding": "encoding", "width": "width", "height": "height" }
                },
                "max_result_bytes": 4096,
                "on_oversize": "downscale",
                "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
                "output_schema": { "type": "object" }
            }))
            .expect("valid tool entry"),
        );
        bundle
    }

    /// A server whose `front_camera.snap` answers `response`.
    fn snap_server(response: Value) -> ExposureServer {
        ExposureServer::builder(snap_bundle())
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_tool("front_camera.snap", move |_call: ToolCall| {
                let response = response.clone();
                async move { Ok(response) }
            })
            .build()
            .expect("bundle and handlers agree")
    }

    async fn snap(server: &ExposureServer) -> CallToolResult {
        server
            .execute_tool("front_camera.snap", JsonObject::new(), None)
            .await
            .expect("the service answers")
    }

    #[tokio::test]
    async fn a_service_with_a_representation_answers_with_the_image_and_the_document() {
        let mut response = rgb8_frame();
        response["success"] = json!(true);
        response["message"] = json!("");
        let result = snap(&snap_server(response)).await;
        assert_eq!(result.is_error, Some(false));
        let mut document = frame_document();
        document["success"] = json!(true);
        document["message"] = json!("");
        assert_eq!(result.structured_content, Some(document.clone()));
        let [image, text] = result.content.as_slice() else {
            panic!("expected an image and a text, got {:?}", result.content);
        };
        let image = image.as_image().expect("the image comes first");
        assert_eq!(image.mime_type, "image/jpeg");
        assert_is_jpeg(&image.data);
        let text = &text.as_text().expect("the document comes second").text;
        assert_eq!(
            serde_json::from_str::<Value>(text).expect("the text is JSON"),
            document
        );
    }

    #[tokio::test]
    async fn a_service_response_without_a_frame_is_the_document_alone() {
        let refusal = json!({
            "success": false,
            "message": "the simulation runs without rendering",
            "encoding": "",
            "width": 0,
            "height": 0,
            "frame": "",
        });
        let result = snap(&snap_server(refusal)).await;
        assert_eq!(result.is_error, Some(false));
        let document = json!({
            "success": false,
            "message": "the simulation runs without rendering",
            "encoding": "",
            "width": 0,
            "height": 0,
        });
        assert_eq!(result.structured_content, Some(document.clone()));
        let [text] = result.content.as_slice() else {
            panic!("expected the document alone, got {:?}", result.content);
        };
        assert_eq!(
            serde_json::from_str::<Value>(&text.as_text().expect("a text").text).expect("JSON"),
            document
        );
    }

    #[tokio::test]
    async fn an_oversize_service_picture_is_downscaled_and_a_bad_frame_is_a_tool_error() {
        use base64::Engine as _;
        let bytes: Vec<u8> = (0..64usize * 64 * 3)
            .map(|index| ((index * 97 + index / 3 * 31) % 256) as u8)
            .collect();
        let noisy = json!({
            "frame": base64::engine::general_purpose::STANDARD.encode(&bytes),
            "encoding": "rgb8",
            "width": 64,
            "height": 64,
        });
        let result = snap(&snap_server(noisy)).await;
        assert_eq!(result.is_error, Some(false), "{result:?}");
        let document = result.structured_content.expect("the document");
        let width = document["width"].as_u64().expect("width is rewritten");
        assert!(
            width < 64,
            "the picture shrinks to fit 4096 bytes, got {width}"
        );
        let image = result.content[0].as_image().expect("the image comes first");
        assert!(image.data.len() as u64 <= 4096);

        let bad = json!({ "frame": "not base64!", "encoding": "rgb8", "width": 8, "height": 8 });
        let result = snap(&snap_server(bad)).await;
        assert_eq!(result.is_error, Some(true));
        assert!(
            tool_error_text(&result).contains("representation field `frame`"),
            "{}",
            tool_error_text(&result)
        );
    }

    /// `task_bundle` with a read-only tool beside the mutating one, and a
    /// record of two calls.
    fn record_bundle() -> ExposureBundle {
        let mut bundle = task_bundle();
        bundle.tools.push(
            serde_json::from_value(json!({
                "name": "front_camera.get_brightness",
                "description": "Report the camera brightness.",
                "target": "front_camera",
                "member": "get_brightness",
                "operation": "read_only",
                "deadline_ms": 2000,
                "input_schema": { "type": "object", "properties": {}, "additionalProperties": false },
                "output_schema": { "type": "object" }
            }))
            .expect("valid tool entry"),
        );
        bundle.call_record = Some(peppy_mcp_catalog::CallRecordEntry {
            name: "camera.recent_calls".to_string(),
            description: "The last state-changing calls of this endpoint.".to_string(),
            keep: 2,
        });
        bundle
    }

    /// The builder of a record server, on a wall clock the test drives,
    /// with every handler registered; a test registers its own handler of
    /// a name over the one here.
    /// The builder of `bundle` with the handlers of every member of
    /// `record_bundle`.
    fn record_builder_of(bundle: ExposureBundle) -> ExposureServerBuilder {
        ExposureServer::builder(bundle)
            .with_tool("front_camera.set_brightness", brightness_handler)
            .with_tool("front_camera.get_brightness", |_call: ToolCall| async {
                Ok(json!({ "value": 3 }))
            })
            .with_task("recorder.record_episode", record_handler)
            .with_task("recorder.resume_session", resume_handler)
    }

    fn record_builder() -> (ExposureServerBuilder, Arc<std::sync::atomic::AtomicU64>) {
        let (clock, nanos) = manual_clock();
        let builder = record_builder_of(record_bundle()).with_wall_clock(clock);
        (builder, nanos)
    }

    fn built_record_server() -> (ExposureServer, Arc<std::sync::atomic::AtomicU64>) {
        let (builder, nanos) = record_builder();
        (builder.build().expect("builds"), nanos)
    }

    fn recorded_calls(server: &ExposureServer) -> Vec<Value> {
        let record = server
            .state
            .record
            .as_ref()
            .expect("the bundle keeps a record");
        let answer = server
            .recent_calls(record, JsonObject::new())
            .expect("the record answers");
        answer.structured_content.expect("structured")["calls"]
            .as_array()
            .cloned()
            .expect("the calls")
    }

    fn a_client() -> Option<ClientIdentity> {
        Some(ClientIdentity {
            name: "claude-code".to_string(),
            version: "2.1".to_string(),
        })
    }

    #[test]
    fn the_record_tool_is_listed_last_read_only_and_a_taken_name_is_refused() {
        let (server, _) = built_record_server();
        let names: Vec<&str> = server
            .state
            .tool_list
            .iter()
            .map(|tool| tool.name.as_ref())
            .collect();
        assert_eq!(
            names,
            [
                "front_camera.set_brightness",
                "front_camera.get_brightness",
                "recorder.record_episode",
                "recorder.resume_session",
                "camera.recent_calls",
            ]
        );
        let tool = server.get_tool("camera.recent_calls").expect("listed");
        assert_eq!(
            tool.description.as_deref(),
            Some("The last state-changing calls of this endpoint.")
        );
        let annotations = tool.annotations.expect("annotations set");
        assert_eq!(annotations.read_only_hint, Some(true));
        assert_eq!(annotations.destructive_hint, Some(false));
        assert!(tool.output_schema.is_some());

        let record = server.state.record.as_ref().expect("a record");
        let error = server
            .recent_calls(record, arguments(json!({ "count": 1 })))
            .expect_err("takes no argument");
        assert_eq!(error.code, ErrorCode::INVALID_PARAMS);
        assert!(recorded_calls(&server).is_empty());

        let mut bundle = record_bundle();
        bundle.call_record.as_mut().expect("a record").name =
            "front_camera.set_brightness".to_string();
        let error = record_builder_of(bundle)
            .build()
            .expect_err("the record tool claims a taken name");
        assert_eq!(
            error,
            BuildError::DuplicateName {
                name: "front_camera.set_brightness".to_string()
            }
        );
    }

    #[tokio::test]
    async fn a_service_call_leaves_one_entry_and_a_read_only_call_none() {
        let (server, nanos) = built_record_server();
        nanos.store(1_000 * MS, Ordering::SeqCst);
        server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 12 })),
                a_client(),
            )
            .await
            .expect("answers");
        server
            .execute_tool("front_camera.get_brightness", JsonObject::new(), a_client())
            .await
            .expect("answers");
        let calls = recorded_calls(&server);
        assert_eq!(
            calls.len(),
            1,
            "the read-only call left no entry: {calls:?}"
        );
        assert_eq!(
            calls[0],
            json!({
                "started_at": "1970-01-01T00:00:01.000000000Z",
                "client": { "name": "claude-code", "version": "2.1" },
                "tool": "front_camera.set_brightness",
                "arguments": { "value": 12 },
                "arguments_bytes": 12,
                "outcome": "completed",
                "success": null,
                "message": "",
                "duration_ms": 0,
            })
        );
    }

    #[tokio::test]
    async fn a_refused_call_is_recorded_as_refused_with_a_null_client() {
        let (server, _) = built_record_server();
        server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 65 })),
                None,
            )
            .await
            .expect_err("65 is out of bounds");
        // A name that is no tool is no call of a tool.
        server
            .execute_tool("front_camera.set_gain", JsonObject::new(), None)
            .await
            .expect_err("not a tool");
        let calls = recorded_calls(&server);
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0]["outcome"], "refused");
        assert_eq!(calls[0]["client"], Value::Null);
        assert_eq!(calls[0]["arguments"], json!({ "value": 65 }));
        assert!(
            calls[0]["message"]
                .as_str()
                .expect("a message")
                .contains("invalid arguments for `front_camera.set_brightness`"),
            "{}",
            calls[0]["message"]
        );

        // A provider that cannot be reached is a refusal too.
        let (builder, _) = record_builder();
        let server = builder
            .with_tool("front_camera.set_brightness", |_call: ToolCall| async {
                Err(ToolCallError::Unavailable("no producer bound".to_string()))
            })
            .build()
            .expect("builds");
        server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 1 })),
                None,
            )
            .await
            .expect("a tool error");
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "refused");
        assert_eq!(
            calls[0]["message"],
            "provider unavailable: no producer bound"
        );
    }

    #[tokio::test]
    async fn an_action_inside_a_call_and_a_task_each_leave_one_entry() {
        let (builder, nanos) = record_builder();
        let server = builder
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    Ok(json!({ "success": false, "message": "no session to resume" }))
                },
            )
            .build()
            .expect("builds");
        nanos.store(5_000 * MS, Ordering::SeqCst);
        server
            .run_action_in_call(
                &task_named(&server, "recorder.resume_session"),
                JsonObject::new(),
                a_client(),
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .expect("the call answers");
        let calls = recorded_calls(&server);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["tool"], "recorder.resume_session");
        assert_eq!(calls[0]["outcome"], "completed");
        assert_eq!(calls[0]["success"], false);
        assert_eq!(calls[0]["message"], "no session to resume");

        nanos.store(6_000 * MS, Ordering::SeqCst);
        let created = server
            .start_task(
                &task_named(&server, "recorder.resume_session"),
                JsonObject::new(),
                a_client(),
            )
            .expect("the task starts");
        let calls = recorded_calls(&server);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["outcome"], "running");
        assert_eq!(calls[0]["duration_ms"], Value::Null);
        nanos.store(6_250 * MS, Ordering::SeqCst);
        settled(&server, &created.task.task_id).await;
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "completed");
        assert_eq!(calls[0]["success"], false);
        assert_eq!(calls[0]["message"], "no session to resume");
        assert_eq!(calls[0]["duration_ms"], 250);
        assert_eq!(calls[0]["started_at"], "1970-01-01T00:00:06.000000000Z");
        assert_eq!(calls[1]["started_at"], "1970-01-01T00:00:05.000000000Z");
    }

    #[tokio::test(start_paused = true)]
    async fn a_cancelled_a_failed_and_an_overrun_goal_are_recorded_as_such() {
        let (builder, _) = record_builder();
        let server = builder
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, context: crate::tasks::ActionContext| async move {
                    context.cancel_requested().await;
                    Err(ActionExit::Cancelled(CancelledGoal {
                        result: cancelled_resume(),
                        reason: None,
                    }))
                },
            )
            .build()
            .expect("builds");
        let cancel = tokio_util::sync::CancellationToken::new();
        cancel.cancel();
        server
            .run_action_in_call(
                &task_named(&server, "recorder.resume_session"),
                JsonObject::new(),
                None,
                cancel,
                None,
            )
            .await
            .expect("a cancelled goal is a tool error");
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "cancelled");
        assert_eq!(
            calls[0]["message"],
            format!("the action was cancelled: {}", cancelled_resume())
        );

        let (builder, _) = record_builder();
        let server = builder
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
        let created = server
            .start_task(
                &task_named(&server, "recorder.resume_session"),
                JsonObject::new(),
                None,
            )
            .expect("the task starts");
        settled(&server, &created.task.task_id).await;
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "failed");
        assert_eq!(
            calls[0]["message"],
            "the action failed: the provider abandoned the goal"
        );

        let (builder, _) = record_builder();
        let server = builder
            .with_task(
                "recorder.resume_session",
                |_call: ToolCall, _context: crate::tasks::ActionContext| async move {
                    std::future::pending::<Result<Value, ActionExit>>().await
                },
            )
            .build()
            .expect("builds");
        server
            .run_action_in_call(
                &task_named(&server, "recorder.resume_session"),
                JsonObject::new(),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .expect("an overrun is a tool error");
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "failed");
        assert_eq!(
            calls[0]["message"],
            "deadline exceeded: the goal did not reach a terminal state within 2000 ms"
        );
    }

    #[tokio::test]
    async fn a_declined_confirmation_is_recorded_as_cancelled_and_the_record_keeps_the_last_two() {
        let (server, _) = built_record_server();
        // Without the tasks extension, the confirmation-gated action is
        // refused, and the refusal is recorded.
        server
            .run_action_in_call(
                &task_named(&server, "recorder.record_episode"),
                arguments(json!({ "episode_name": "demo" })),
                None,
                tokio_util::sync::CancellationToken::new(),
                None,
            )
            .await
            .expect_err("the confirmation needs a task");
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "refused");
        assert!(
            calls[0]["message"]
                .as_str()
                .expect("a message")
                .contains(TASKS_EXTENSION_ID)
        );

        let created = server
            .start_task(
                &task_named(&server, "recorder.record_episode"),
                arguments(json!({ "episode_name": "demo" })),
                None,
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
        settled(&server, &task_id).await;
        let calls = recorded_calls(&server);
        assert_eq!(calls[0]["outcome"], "cancelled");
        assert_eq!(calls[0]["message"], CONFIRMATION_DECLINED);

        // A third call: the record keeps two, so the refusal is gone.
        server
            .execute_tool(
                "front_camera.set_brightness",
                arguments(json!({ "value": 1 })),
                None,
            )
            .await
            .expect("answers");
        let calls = recorded_calls(&server);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["tool"], "front_camera.set_brightness");
        assert_eq!(calls[1]["tool"], "recorder.record_episode");
        assert_eq!(calls[1]["outcome"], "cancelled");
    }
}
