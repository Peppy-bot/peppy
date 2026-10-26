//! The process: load the spec, plan the deployment, lay the codecs out,
//! then run as a node and serve.

use crate::bridges::{self, PreparedExposure, PreparedTask, PreparedTool, Route};
use crate::daemon_scopes::DaemonScopes;
use crate::fleet;
use crate::moves::MoveRules;
use daemon_config::daemon_interface::DaemonInterface;
use daemon_config::mcp_deployment::{
    McpDeploymentError, McpDeploymentPlan, McpServeSpec, PORT_PARAMETER, SPEC_ENV_VAR,
    endpoint_label, plan_deployment,
};
use peppy_codec::consumer::ConsumerIdentity;
use peppy_mcp_catalog::BundleSurface;
use peppy_mcp_runtime::{Clock, ExposureServer, ExposureServerBuilder, ExposureSet, FleetHandle};
use peppylib::runtime::{EndpointBinding, NodeBuilder, NodeRunner, bind_preferred};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum ServeError {
    #[error(
        "{SPEC_ENV_VAR} is not set: `peppy mcp serve` is started by the daemon for an `exposures` \
         deployment, which hands it the exposures to serve"
    )]
    NoSpec,
    #[error("{0}")]
    Spec(String),
    #[error("the exposures cannot be served:\n{0}")]
    Plan(#[from] McpDeploymentError),
    #[error("cannot lay out the wire format of {member}: {source}")]
    Codec {
        member: String,
        source: peppy_codec::CodecError,
    },
    #[error(
        "tool `{tool}` is bounded by its progress, and its action declares no feedback message to \
         show progress with"
    )]
    ProgressWithoutFeedback { tool: String },
    #[error(
        "resource `{resource}` reads a topic of daemon target `{target}`; a daemon interface \
         serves no topic"
    )]
    DaemonResource { resource: String, target: String },
    #[error("target `{target}` names a daemon interface this server does not serve: {reason}")]
    UnservedDaemonInterface { target: String, reason: String },
    #[error("the server cannot call fleet:v1: {0}")]
    FleetClient(#[from] peppy_codec::consumer::MemberError),
    #[error(
        "the daemon targets cannot be served:{problems}\nA launch and a join check every scope \
         before they start the server; a server started another way, for example with `peppy \
         node run`, gets its scopes from the runtime configuration of its instance only"
    )]
    DaemonScopes { problems: String },
    #[error("{0}")]
    Build(#[from] peppy_mcp_runtime::BuildError),
    #[error("cannot bind 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
    #[error(
        "the stack binds members exposure `{exposure}` cannot serve:\n{problems}\nA per-robot \
         surface is filled by copies alone, each target once per robot unless the target \
         declares an argument: bind each instance from the launcher's `adjustments` under its \
         copy's option with `add_links`, one per target"
    )]
    Fleet { exposure: String, problems: String },
    #[error(transparent)]
    Node(#[from] peppylib::PeppyError),
}

/// The one argument the synthesized manifest declares, under the name
/// [`PORT_PARAMETER`].
#[derive(Debug, Deserialize, schemars::JsonSchema)]
struct ServeArguments {
    port: u16,
}

const _: () = assert!(
    matches!(PORT_PARAMETER.as_bytes(), b"port"),
    "the manifest's parameter name and the argument field agree"
);

/// Runs the server to completion: until the daemon stops it, the process
/// receives a signal, or the endpoint fails.
///
/// Everything the documents can refuse (a spec that does not verify, an
/// exposure that does not validate against its pinned contracts, a format
/// that cannot be laid out) refuses before the process connects to the
/// daemon, with the full report on stderr. The scopes of the daemon targets
/// come with the runtime configuration of the instance, so a missing or bad
/// scope refuses at the start of the node's setup, before anything is
/// served.
pub fn serve() -> Result<(), ServeError> {
    let spec_path = std::env::var_os(SPEC_ENV_VAR).ok_or(ServeError::NoSpec)?;
    let spec = McpServeSpec::from_path(Path::new(&spec_path)).map_err(ServeError::Spec)?;
    let (exposures, contracts) = spec.resolve().map_err(ServeError::Spec)?;
    let McpDeploymentPlan {
        config: manifest,
        exposures: validated,
        targets,
        fleet_slot,
        ..
    } = plan_deployment(&exposures, &contracts)?;
    let prepared = bridges::prepare(validated)?;
    NodeBuilder::<ServeArguments>::new()
        .with_manifest(manifest)
        .run(move |arguments, node_runner| async move {
            run(arguments, node_runner, prepared, targets.daemon, fleet_slot)
                .await
                .map_err(|error| peppylib::PeppyError::Io(std::io::Error::other(error.to_string())))
        })?;
    Ok(())
}

/// The node's setup: the scope of each daemon target, the fleet rules, the
/// bridges to the producers of each target, one server per exposure, the
/// listener, and the pumps that feed the resources. `daemon_targets` are
/// the daemon targets of the deployment, with the interface each one names,
/// and `fleet_slot` the slot of `fleet:v1` that the fleet rules ask.
async fn run(
    arguments: ServeArguments,
    node_runner: Arc<NodeRunner>,
    prepared: Vec<PreparedExposure>,
    daemon_targets: BTreeMap<String, DaemonInterface>,
    fleet_slot: String,
) -> Result<(), ServeError> {
    let port = arguments.port;

    // A daemon target is served only under the scope the launch gave it:
    // parsed first, so a server without one stops before it serves anything.
    let scopes = DaemonScopes::parse(
        node_runner.processor().bound_instance_id(),
        &daemon_targets,
        node_runner.daemon_scopes(),
    )?;

    // The daemon's clock, sim time included, governs freshness and rate
    // gating; `0` while a sim clock has not ticked yet reads as "stale".
    let peppy_clock = Arc::new(peppylib::clock::for_node(&node_runner).await?);
    let clock = Clock::from_nanos_fn({
        let peppy_clock = Arc::clone(&peppy_clock);
        move || peppy_clock.now_ns().unwrap_or_default()
    });
    let identity = ConsumerIdentity {
        core_node: node_runner.processor().bound_core_node().to_owned(),
        instance_id: node_runner.processor().bound_instance_id().to_owned(),
    };
    let rules = MoveRules::new(
        node_runner.messenger().clone(),
        identity.clone(),
        node_runner
            .processor()
            .sole_bound_producer(&fleet_slot)
            .clone(),
    )?;
    let route = Route::of_node(&node_runner, identity, rules);

    let mut servers = Vec::with_capacity(prepared.len());
    let mut pumps = Vec::new();
    let mut followers = Vec::new();
    let mut announcements = Vec::with_capacity(prepared.len());
    for mut exposure in prepared {
        scopes.narrow(&mut exposure.bundle);
        exposure.drop_unpublished_entries();
        announcements.push((
            endpoint_label(
                &exposure.bundle.exposure.name,
                &exposure.bundle.exposure.tag,
            ),
            exposure.bundle.exposure.endpoint_path(),
        ));
        // A per-robot exposure reads its fleet from the bound sets of its
        // targets; a fixed one binds each target to the launcher's producer.
        let targets: Vec<String> = exposure
            .bundle
            .surface
            .contracts()
            .iter()
            .map(|pin| pin.link_id.clone())
            .collect();
        let per_robot = matches!(exposure.bundle.surface, BundleSurface::PerRobot { .. });
        let label = format!(
            "{}:{}",
            exposure.bundle.exposure.name, exposure.bundle.exposure.tag
        );
        let mut builder = ExposureServer::builder(exposure.bundle).with_clock(clock.clone());
        builder = register_tools(builder, exposure.tools, &route);
        builder = register_tasks(builder, exposure.tasks, &route);
        if per_robot {
            let node_runner = Arc::clone(&node_runner);
            let targets = targets.clone();
            builder = builder.with_fleet(move || fleet::members(&node_runner, &targets));
        }
        let server = builder.build()?;
        match server.fleet() {
            Some(handle) => {
                ensure_fleet_servable(&handle, label)?;
                followers.push((targets, exposure.resources, handle));
            }
            None => {
                for resource in exposure.resources {
                    let ingest = server
                        .ingest(&resource.name)
                        .expect("the bundle the server was built from exposes this resource");
                    pumps.push((resource, ingest));
                }
            }
        }
        servers.push(server);
    }
    let set = ExposureSet::new(servers)?;
    let endpoints = set.endpoint_paths();

    // The launcher's port is a preference: a server that finds it held takes
    // a port from the operating system, so any number of servers preferring
    // one port start side by side, each announcing the socket it holds.
    let preferred = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
    let (listener, fell_back) = bind_preferred(preferred)
        .await
        .map_err(|source| ServeError::Bind { port, source })?;
    let bound = listener
        .local_addr()
        .map_err(|source| ServeError::Bind { port, source })?;
    if fell_back {
        tracing::warn!(
            "port {port} is held by another process; serving on port {} instead",
            bound.port()
        );
    }
    // The daemon learns the URLs the same way it does for every node: each
    // exposure is announced under the label its synthesized manifest
    // declares, with the loopback socket the listener bound and its path.
    for (label, path) in announcements {
        node_runner.announce_endpoint(
            &label,
            EndpointBinding {
                scheme: "http".to_owned(),
                address: bound,
                path,
            },
        )?;
    }
    tracing::info!(
        port = bound.port(),
        endpoints = endpoints.join(", "),
        "serving {} exposure(s)",
        endpoints.len()
    );

    let shutdown = tokio_util::sync::CancellationToken::new();
    node_runner.on_shutdown({
        let shutdown = shutdown.clone();
        async move {
            shutdown.cancel();
        }
    });
    for (resource, ingest) in pumps {
        tokio::spawn(bridges::pump_resource(
            Arc::clone(&node_runner),
            resource,
            // A fixed surface's target is bound to one producer, and every
            // message it publishes fills the one resource.
            move |_producer| Some(ingest.clone()),
        ));
    }
    for (targets, resources, handle) in followers {
        tokio::spawn(fleet::follow(
            Arc::clone(&node_runner),
            targets,
            resources,
            handle,
        ));
    }
    // The endpoint outlives this setup; a `serve` that stops on its own
    // takes the node down rather than leaving it running with no endpoint.
    tokio::spawn({
        let node_runner = Arc::clone(&node_runner);
        async move {
            match set.serve(listener, shutdown).await {
                Ok(()) => tracing::debug!("the MCP endpoints stopped"),
                Err(error) => {
                    tracing::error!(%error, "the MCP endpoints failed");
                    node_runner.cancellation_token().cancel();
                }
            }
        }
    });
    Ok(())
}

/// Registers a handler per tool on `builder`: each call reaches the tool's
/// service on `route`.
pub(crate) fn register_tools(
    mut builder: ExposureServerBuilder,
    tools: Vec<PreparedTool>,
    route: &Route,
) -> ExposureServerBuilder {
    for tool in tools {
        let tool = Arc::new(tool);
        let name = tool.name.clone();
        let route = route.clone();
        builder = builder.with_tool(name, move |call: peppy_mcp_runtime::ToolCall| {
            let tool = Arc::clone(&tool);
            let route = route.clone();
            async move { bridges::call_tool(&tool, &route, call).await }
        });
    }
    builder
}

/// Registers a handler per task on `builder`: each call drives a goal of
/// the task's action on `route`.
pub(crate) fn register_tasks(
    mut builder: ExposureServerBuilder,
    tasks: Vec<PreparedTask>,
    route: &Route,
) -> ExposureServerBuilder {
    for task in tasks {
        let task = Arc::new(task);
        let name = task.name.clone();
        let route = route.clone();
        builder = builder.with_task(
            name,
            move |call: peppy_mcp_runtime::ToolCall, context: peppy_mcp_runtime::ActionContext| {
                let task = Arc::clone(&task);
                let route = route.clone();
                async move { bridges::run_task(&task, &route, call, context).await }
            },
        );
    }
    builder
}

/// Refuses a per-robot exposure when the stack binds members its surface
/// cannot serve, with every problem on a line of its own.
fn ensure_fleet_servable(handle: &FleetHandle, exposure: String) -> Result<(), ServeError> {
    let problems = handle.problems();
    if problems.is_empty() {
        return Ok(());
    }
    Err(ServeError::Fleet {
        exposure,
        problems: problems
            .iter()
            .map(|problem| format!("  - {problem}"))
            .collect::<Vec<_>>()
            .join("\n"),
    })
}
