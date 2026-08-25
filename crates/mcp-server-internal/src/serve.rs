//! The process: load the spec, plan the deployment, lay the codecs out,
//! then run as a node and serve.

use crate::bridges::{self, PreparedExposure};
use daemon_config::mcp_deployment::{
    McpDeploymentError, McpDeploymentPlan, McpServeSpec, PORT_PARAMETER, SPEC_ENV_VAR,
    plan_deployment,
};
use message_codec::consumer::ConsumerIdentity;
use peppy_mcp_runtime::{Clock, ExposureServer, ExposureSet};
use peppylib::runtime::{NodeBuilder, NodeRunner};
use serde::Deserialize;
use std::path::Path;
use std::sync::Arc;
use tokio::net::TcpListener;

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
        source: message_codec::CodecError,
    },
    #[error("{0}")]
    Build(#[from] peppy_mcp_runtime::BuildError),
    #[error("cannot bind 127.0.0.1:{port}: {source}")]
    Bind { port: u16, source: std::io::Error },
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
/// Everything that can refuse the deployment (a spec that does not verify,
/// an exposure that does not validate against its pinned contracts, a
/// format that cannot be laid out) refuses before the process connects to
/// the daemon, with the full report on stderr.
pub fn serve() -> Result<(), ServeError> {
    let spec_path = std::env::var_os(SPEC_ENV_VAR).ok_or(ServeError::NoSpec)?;
    let spec = McpServeSpec::from_path(Path::new(&spec_path)).map_err(ServeError::Spec)?;
    let (exposures, contracts) = spec.resolve().map_err(ServeError::Spec)?;
    let plan: McpDeploymentPlan = plan_deployment(&exposures, &contracts)?;
    let prepared = bridges::prepare(&plan, &contracts)?;
    let manifest = plan.config;
    NodeBuilder::<ServeArguments>::new()
        .with_manifest(manifest)
        .run(move |arguments, node_runner| async move {
            run(arguments, node_runner, prepared)
                .await
                .map_err(|error| peppylib::PeppyError::Io(std::io::Error::other(error.to_string())))
        })?;
    Ok(())
}

/// The node's setup: bridges bound to the launcher's producers, one server
/// per exposure, the listener, and the pumps that feed the resources.
async fn run(
    arguments: ServeArguments,
    node_runner: Arc<NodeRunner>,
    prepared: Vec<PreparedExposure>,
) -> Result<(), ServeError> {
    let port = arguments.port;

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

    let mut servers = Vec::with_capacity(prepared.len());
    let mut pumps = Vec::new();
    for exposure in prepared {
        let mut builder = ExposureServer::builder(exposure.bundle).with_clock(clock.clone());
        for tool in exposure.tools {
            let tool = Arc::new(tool);
            let name = tool.name.clone();
            let node_runner = Arc::clone(&node_runner);
            let identity = identity.clone();
            builder = builder.with_tool(name, move |input: serde_json::Value| {
                let tool = Arc::clone(&tool);
                let node_runner = Arc::clone(&node_runner);
                let identity = identity.clone();
                async move { bridges::call_tool(&tool, &node_runner, &identity, input).await }
            });
        }
        for task in exposure.tasks {
            let task = Arc::new(task);
            let name = task.name.clone();
            let node_runner = Arc::clone(&node_runner);
            let identity = identity.clone();
            builder = builder.with_task(
                name,
                move |input: serde_json::Value, context: peppy_mcp_runtime::ActionContext| {
                    let task = Arc::clone(&task);
                    let node_runner = Arc::clone(&node_runner);
                    let identity = identity.clone();
                    async move {
                        bridges::run_task(&task, &node_runner, &identity, input, context).await
                    }
                },
            );
        }
        let server = builder.build()?;
        for resource in exposure.resources {
            let ingest = server
                .ingest(&resource.name)
                .expect("the bundle the server was built from exposes this resource");
            pumps.push((resource, ingest));
        }
        servers.push(server);
    }
    let set = ExposureSet::new(servers)?;
    let endpoints = set.endpoint_paths();

    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|source| ServeError::Bind { port, source })?;
    tracing::info!(
        port,
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
            identity.clone(),
            resource,
            ingest,
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
