//! Shared harness for the roundtrip-latency benchmark and threshold test.
//!
//! It spins up **two real peppy nodes** that talk to each other and measures
//! topic / service roundtrip latency between them:
//!
//! - a **driver** node (always Rust) that times each roundtrip with a single
//!   `Instant` clock and reports the distribution back to this process over a
//!   raw `bench_control` service, and
//! - a **responder** node whose language is the variable — Rust or Python.
//!
//! Both are launched exactly the way peppy launches nodes (`NodeBuilder` reading
//! `PEPPY_RUNTIME_CONFIG`, with a generated `peppygen` library present), reusing
//! the codegen + build + spawn machinery from [`crate::helpers`]. The measured
//! hot path uses the raw peppylib messaging API on `node_runner.messenger()`
//! (persistent subscription / queryable — what peppygen wraps) so the tight
//! request/reply loop is race-free; peppygen is still generated so the processes
//! are genuine peppygen nodes.
//!
//! This module is shared between `tests/latency.rs` (via `mod`) and
//! `benches/latency.rs` (via `#[path]`), so it is the single source of truth for
//! what the bench measures and what the threshold test guards.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;

use config::consts::{PEPPYGEN_OUTPUT_PATH, RUNTIME_CONFIG_VAR_NAME};
use config::launcher::Name;
use config::runtime::{NodeInstanceConfig, RuntimeConfig};
use generator::LanguageGenerator;
use peppylib::MessengerHandle;
use peppylib::messaging::{SenderTarget, ServiceMessenger};
use peppylib::types::Payload;
use tempfile::TempDir;

use crate::helpers::{
    self, DEFAULT_WAIT_TIMEOUT, STUB_NODE_CONFIG, STUB_PYTHON_NODE_CONFIG, WaitContext,
};

// --- Fixed identities shared by the harness and the embedded node sources.
// Each scenario gets its own ephemeral router, so reusing these names across
// scenarios is safe.
const CORE: &str = "bench_core";
const TAG: &str = "v1";
const DRIVER_NODE: &str = "driver";
const DRIVER_INST: &str = "driver_inst";
const RESPONDER_NODE: &str = "responder";
const RESPONDER_INST: &str = "responder_inst";
const HARNESS_INST: &str = "bench_harness";
const BENCH_CONTROL_SERVICE: &str = "bench_control";
const ECHO_SERVICE: &str = "echo";

/// Generous ceiling for a single `bench_control` call: it runs warmup + all
/// measured roundtrips before replying, so it must comfortably exceed
/// `(warmup + iters) * per-roundtrip`.
const CONTROL_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Lang {
    Rust,
    Python,
}

impl Lang {
    pub fn as_str(self) -> &'static str {
        match self {
            Lang::Rust => "rust",
            Lang::Python => "python",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Transport {
    Topic,
    Service,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Topic => "topic",
            Transport::Service => "service",
        }
    }

    fn wire_tag(self) -> u8 {
        match self {
            Transport::Topic => 0,
            Transport::Service => 1,
        }
    }
}

/// Default measurement parameters shared by the bench and the threshold test.
pub const DEFAULT_WARMUP: u64 = 50;
pub const DEFAULT_SAMPLES: u64 = 500;

/// All (lang, transport) scenarios, in display order.
pub const ALL_SCENARIOS: &[(Lang, Transport)] = &[
    (Lang::Rust, Transport::Topic),
    (Lang::Rust, Transport::Service),
    (Lang::Python, Transport::Topic),
    (Lang::Python, Transport::Service),
];

/// Per (lang, transport) p90 ceiling in milliseconds — the threshold the guard
/// test asserts and the bench reports against. Order-of-magnitude guards (fail
/// only on a ~10x regression). Overridable via
/// `PEPPY_LATENCY_MAX_MS_<LANG>_<TRANSPORT>` so CI / a runner can retune without
/// code changes.
pub fn ceiling_ms(lang: Lang, transport: Transport) -> u64 {
    let key = format!(
        "PEPPY_LATENCY_MAX_MS_{}_{}",
        lang.as_str().to_uppercase(),
        transport.as_str().to_uppercase()
    );
    if let Ok(value) = std::env::var(&key)
        && let Ok(parsed) = value.parse()
    {
        return parsed;
    }
    match (lang, transport) {
        (Lang::Rust, Transport::Topic) => 20,
        (Lang::Rust, Transport::Service) => 25,
        (Lang::Python, Transport::Topic) => 40,
        (Lang::Python, Transport::Service) => 50,
    }
}

/// Latency distribution reported by the driver for one `bench_control` run.
/// All durations are in nanoseconds; `total` is the summed roundtrip time of
/// the measured iterations.
#[derive(Clone, Copy, Debug)]
pub struct LatencyStats {
    total_ns: u64,
    p50_ns: u64,
    p90_ns: u64,
    mean_ns: u64,
    count: u64,
}

impl LatencyStats {
    fn decode(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 40, "bench_control response must be 40 bytes");
        let read = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Self {
            total_ns: read(0),
            p50_ns: read(1),
            p90_ns: read(2),
            mean_ns: read(3),
            count: read(4),
        }
    }

    pub fn total(&self) -> u64 {
        self.total_ns
    }
    pub fn p50(&self) -> Duration {
        Duration::from_nanos(self.p50_ns)
    }
    pub fn p90(&self) -> Duration {
        Duration::from_nanos(self.p90_ns)
    }
    pub fn mean(&self) -> Duration {
        Duration::from_nanos(self.mean_ns)
    }
    pub fn count(&self) -> u64 {
        self.count
    }
}

// ---------------------------------------------------------------------------
// Node build (codegen + compile), cached so each node is built at most once
// per process.
// ---------------------------------------------------------------------------

/// Ensures the freshly `cargo init`-ed Rust node depends on `peppylib` (for the
/// raw messaging types peppygen does not re-export).
///
/// Critically, it points at the **same vendored peppylib that peppygen depends
/// on** (`.peppy/libs/peppylib`, deployed next to the generated peppygen lib),
/// not the workspace crate. peppygen's Cargo.toml uses `path = "../peppylib"`,
/// so both resolve to one canonical source; pointing at the workspace copy
/// instead makes Cargo see two different `build-helpers` packages and refuse to
/// write the lockfile.
fn ensure_peppylib_dep(user_node: &Path) {
    let cargo_toml = user_node.join("Cargo.toml");
    let contents = std::fs::read_to_string(&cargo_toml).expect("read node Cargo.toml");
    if contents
        .lines()
        .any(|l| l.trim_start().starts_with("peppylib"))
    {
        return;
    }
    let vendored = Path::new(PEPPYGEN_OUTPUT_PATH)
        .parent()
        .expect("peppygen libs dir")
        .join("peppylib");
    let dep = format!("peppylib = {{ path = \"{}\" }}\n", vendored.display());
    let updated = helpers::insert_dependency_line(&contents, &dep);
    std::fs::write(&cargo_toml, updated).expect("write node Cargo.toml");
}

/// Codegen an (interface-less) peppygen library + fingerprint into a fresh node
/// dir so the process is a real peppygen node, write `main`, and build it.
/// Returns the built node directory. The `TempDir` is intentionally leaked so
/// the built artifacts survive for the process lifetime (cached reuse).
fn build_rust_node(main_src: &str) -> PathBuf {
    let temp_dir = TempDir::new().expect("temp dir");
    let (generator, output_dir, user_node, peppy_config_path) =
        helpers::init_test_env::<generator::RustGenerator>(&temp_dir, STUB_NODE_CONFIG);
    let output_config = helpers::copy_config_to_output(&user_node, &output_dir);
    generator
        .build(&output_dir, &helpers::test_peppy_dirs(), Default::default())
        .expect("codegen peppygen (rust)");
    std::fs::remove_file(output_config).expect("remove staged config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    helpers::init_cargo_user_node(&user_node);
    ensure_peppylib_dep(&user_node);
    std::fs::write(user_node.join("src").join("main.rs"), main_src).expect("write main.rs");
    helpers::compile_project(&user_node);

    std::mem::forget(temp_dir);
    user_node
}

fn build_python_node(main_src: &str) -> PathBuf {
    let temp_dir = TempDir::new().expect("temp dir");
    let (generator, output_dir, user_node, peppy_config_path) =
        helpers::init_test_env::<generator::PythonGenerator>(&temp_dir, STUB_PYTHON_NODE_CONFIG);
    let output_config = helpers::copy_config_to_output(&user_node, &output_dir);
    generator
        .build(&output_dir, &helpers::test_peppy_dirs(), Default::default())
        .expect("codegen peppygen (python)");
    std::fs::remove_file(output_config).expect("remove staged config");
    config::fingerprint::create_codegen_fingerprint(
        &peppy_config_path,
        Path::new(PEPPYGEN_OUTPUT_PATH),
    );

    helpers::init_python_user_node(&user_node);
    std::fs::write(user_node.join("main.py"), main_src).expect("write main.py");
    helpers::init_python_project_venv(&user_node);

    std::mem::forget(temp_dir);
    user_node
}

fn driver_node_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| build_rust_node(DRIVER_MAIN_RS))
}

fn responder_rust_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| build_rust_node(RESPONDER_MAIN_RS))
}

fn responder_python_dir() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| build_python_node(RESPONDER_MAIN_PY))
}

/// Eagerly build all node binaries (driver + both responders). Useful for the
/// bench so the build cost is not attributed to the first measured scenario.
pub fn build_all_nodes(include_python: bool) {
    driver_node_dir();
    responder_rust_dir();
    if include_python {
        responder_python_dir();
    }
}

// ---------------------------------------------------------------------------
// Scenario lifecycle: router + two spawned nodes, driven via bench_control.
// ---------------------------------------------------------------------------

fn write_runtime_config(
    cfg_dir: &Path,
    host: &str,
    port: u16,
    node_name: &str,
    instance_id: &str,
) -> PathBuf {
    let runtime_config = RuntimeConfig::new(
        host,
        port,
        NodeInstanceConfig::new(Name::new(instance_id).expect("instance name")),
        node_name,
        TAG,
        CORE,
    )
    .expect("build runtime config");
    let path = cfg_dir.join(format!("{node_name}_runtime.json5"));
    runtime_config
        .save_json5_launch_config(&path)
        .expect("save runtime config");
    path
}

/// A running scenario: an ephemeral router plus a spawned driver + responder,
/// kept alive so `run` can drive many `bench_control` invocations. One scenario
/// per responder language; both transports run against it.
pub struct Scenario {
    _router: pmi::ZenohdInstance,
    control: MessengerHandle,
    driver_child: std::process::Child,
    responder_child: std::process::Child,
    driver_dir: PathBuf,
    responder_dir: PathBuf,
    _cfg_dir: TempDir,
}

pub async fn start_scenario(lang: Lang) -> Scenario {
    let router = pmi::ZenohAdapter::start_router_ephemeral("127.0.0.1", None)
        .await
        .expect("start ephemeral router");
    let host = router.host.clone();
    let port = router.port;

    let driver_dir = driver_node_dir().to_path_buf();
    let responder_dir = match lang {
        Lang::Rust => responder_rust_dir().to_path_buf(),
        Lang::Python => responder_python_dir().to_path_buf(),
    };

    let cfg_dir = TempDir::new().expect("cfg temp dir");
    let driver_cfg = write_runtime_config(cfg_dir.path(), &host, port, DRIVER_NODE, DRIVER_INST);
    let responder_cfg =
        write_runtime_config(cfg_dir.path(), &host, port, RESPONDER_NODE, RESPONDER_INST);

    // Spawn the responder first so its echo service / ping subscription are up
    // before the driver starts probing.
    let mut responder_child = match lang {
        Lang::Rust => helpers::spawn_cargo_run(
            &responder_dir,
            &[(RUNTIME_CONFIG_VAR_NAME, responder_cfg.to_str().unwrap())],
        ),
        Lang::Python => helpers::spawn_python_run(
            &responder_dir,
            &[(RUNTIME_CONFIG_VAR_NAME, responder_cfg.to_str().unwrap())],
        ),
    };
    let mut driver_child = helpers::spawn_cargo_run(
        &driver_dir,
        &[(RUNTIME_CONFIG_VAR_NAME, driver_cfg.to_str().unwrap())],
    );

    let control = MessengerHandle::from_host_port(&host, port)
        .await
        .expect("control messenger");

    {
        let ctx = WaitContext {
            messenger: &control,
            bound_core_node: CORE,
            caller_instance_id: HARNESS_INST,
            target_core_node: Some(CORE),
        };

        helpers::wait_for_health_service_reachable_or_exit(
            &ctx,
            RESPONDER_NODE,
            RESPONDER_INST,
            &mut responder_child,
            &responder_dir,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await;
        helpers::wait_for_service_reachable_or_exit(
            &ctx,
            RESPONDER_NODE,
            ECHO_SERVICE,
            Some(RESPONDER_INST),
            &mut responder_child,
            &responder_dir,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await;
        helpers::wait_for_health_service_reachable_or_exit(
            &ctx,
            DRIVER_NODE,
            DRIVER_INST,
            &mut driver_child,
            &driver_dir,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await;
        helpers::wait_for_service_reachable_or_exit(
            &ctx,
            DRIVER_NODE,
            BENCH_CONTROL_SERVICE,
            Some(DRIVER_INST),
            &mut driver_child,
            &driver_dir,
            DEFAULT_WAIT_TIMEOUT,
        )
        .await;
    }

    // Let the topic ping/pong subscriptions finish propagating through the
    // router before the first measured roundtrip; warmup absorbs the rest.
    tokio::time::sleep(Duration::from_millis(500)).await;

    Scenario {
        _router: router,
        control,
        driver_child,
        responder_child,
        driver_dir,
        responder_dir,
        _cfg_dir: cfg_dir,
    }
}

impl Scenario {
    /// Ask the driver to run `warmup` (discarded) + `iters` (measured)
    /// roundtrips of `transport` and return the reported distribution.
    pub async fn run(&self, transport: Transport, warmup: u64, iters: u64) -> LatencyStats {
        let mut request = Vec::with_capacity(17);
        request.push(transport.wire_tag());
        request.extend_from_slice(&warmup.to_le_bytes());
        request.extend_from_slice(&iters.to_le_bytes());

        let response = ServiceMessenger::poll(
            &self.control,
            CORE,
            HARNESS_INST,
            SenderTarget::node(DRIVER_NODE, TAG).expect("driver target"),
            BENCH_CONTROL_SERVICE,
            Some(CORE),
            Some(DRIVER_INST),
            Payload::from(request),
            CONTROL_TIMEOUT,
        )
        .await
        .expect("bench_control poll");

        LatencyStats::decode(response.payload().as_ref())
    }

    pub async fn shutdown(mut self) {
        helpers::try_send_shutdown(
            &self.control,
            CORE,
            HARNESS_INST,
            DRIVER_NODE,
            Some(CORE),
            DRIVER_INST,
            Duration::from_secs(5),
        )
        .await;
        helpers::try_send_shutdown(
            &self.control,
            CORE,
            HARNESS_INST,
            RESPONDER_NODE,
            Some(CORE),
            RESPONDER_INST,
            Duration::from_secs(5),
        )
        .await;
        helpers::wait_for_child(
            &mut self.driver_child,
            Some(Duration::from_secs(15)),
            &self.driver_dir,
        );
        helpers::wait_for_child(
            &mut self.responder_child,
            Some(Duration::from_secs(15)),
            &self.responder_dir,
        );
    }
}

/// Convenience for the threshold test: full spawn -> measure -> shutdown.
pub async fn run_once(lang: Lang, transport: Transport, warmup: u64, iters: u64) -> LatencyStats {
    let scenario = start_scenario(lang).await;
    let stats = scenario.run(transport, warmup, iters).await;
    scenario.shutdown().await;
    stats
}

// ---------------------------------------------------------------------------
// Embedded node sources.
//
// All three are real peppygen nodes (NodeBuilder + PEPPY_RUNTIME_CONFIG). The
// measured loops use the raw peppylib messaging API on `node_runner.messenger()`
// with a persistent subscription / queryable and a trivial 8-byte little-endian
// `seq` payload, so the tight request/reply loop is race-free.
// ---------------------------------------------------------------------------

const DRIVER_MAIN_RS: &str = r####"
use peppygen::{NodeBuilder, Result};
use peppylib::config::QoSProfile;
use peppylib::messaging::{ConsumerFilter, SenderTarget, ServiceMessenger, Subscription, TopicMessenger};
use peppylib::types::Payload;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const TAG: &str = "v1";
const CORE: &str = "bench_core";
const DRIVER_NODE: &str = "driver";
const RESPONDER_NODE: &str = "responder";
const RESPONDER_INST: &str = "responder_inst";
const RPC_TIMEOUT: Duration = Duration::from_secs(30);
const PONG_TIMEOUT: Duration = Duration::from_secs(5);

fn percentile(sorted: &[u64], p: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = (((sorted.len() - 1) as f64) * p).round() as usize;
    sorted[idx]
}

fn encode_stats(mut samples: Vec<u64>) -> Payload {
    samples.sort_unstable();
    let count = samples.len() as u64;
    let total: u64 = samples.iter().sum();
    let p50 = percentile(&samples, 0.50);
    let p90 = percentile(&samples, 0.90);
    let mean = if count == 0 { 0 } else { total / count };
    let mut out = Vec::with_capacity(40);
    for value in [total, p50, p90, mean, count] {
        out.extend_from_slice(&value.to_le_bytes());
    }
    Payload::from(out)
}

async fn run_service(
    node_runner: &peppygen::NodeRunner,
    core: &str,
    inst: &str,
    warmup: u64,
    iters: u64,
) -> Result<Vec<u64>> {
    let messenger = node_runner.messenger();
    let mut samples = Vec::with_capacity(iters as usize);
    for i in 0..(warmup + iters) {
        let payload = Payload::from(i.to_le_bytes().to_vec());
        let start = Instant::now();
        let _ = ServiceMessenger::poll(
            messenger,
            core,
            inst,
            SenderTarget::node(RESPONDER_NODE, TAG)?,
            "echo",
            Some(CORE),
            Some(RESPONDER_INST),
            payload,
            RPC_TIMEOUT,
        )
        .await?;
        if i >= warmup {
            samples.push(start.elapsed().as_nanos() as u64);
        }
    }
    Ok(samples)
}

async fn run_topic(
    node_runner: &peppygen::NodeRunner,
    core: &str,
    inst: &str,
    sub: &mut Subscription,
    warmup: u64,
    iters: u64,
) -> Result<Vec<u64>> {
    let messenger = node_runner.messenger();
    let mut samples = Vec::with_capacity(iters as usize);
    for i in 0..(warmup + iters) {
        let payload = Payload::from(i.to_le_bytes().to_vec());
        let mut start = Instant::now();
        TopicMessenger::emit(
            messenger,
            core,
            inst,
            SenderTarget::node(DRIVER_NODE, TAG)?,
            "ping",
            QoSProfile::Reliable,
            payload.clone(),
        )
        .await?;
        loop {
            match tokio::time::timeout(PONG_TIMEOUT, sub.on_next_message()).await {
                Ok(Some(msg)) => {
                    if msg.payload().as_ref() == i.to_le_bytes().as_slice() {
                        break;
                    }
                    // Stale pong from an earlier seq; keep draining.
                }
                Ok(None) => return Ok(samples),
                Err(_) => {
                    // Lost ping (warmup-phase propagation): re-emit, reset clock.
                    start = Instant::now();
                    TopicMessenger::emit(
                        messenger,
                        core,
                        inst,
                        SenderTarget::node(DRIVER_NODE, TAG)?,
                        "ping",
                        QoSProfile::Reliable,
                        payload.clone(),
                    )
                    .await?;
                }
            }
        }
        if i >= warmup {
            samples.push(start.elapsed().as_nanos() as u64);
        }
    }
    Ok(samples)
}

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let core = node_runner.processor().bound_core_node().to_string();
        let inst = node_runner.processor().bound_instance_id().to_string();

        // Persistent subscription to the responder's `pong` topic, declared up
        // front so pongs are buffered from the first ping. Parked in a slot the
        // control handler takes out for the duration of a topic run.
        let pong_sub = TopicMessenger::subscribe(
            node_runner.messenger(),
            &core,
            &inst,
            Some(SenderTarget::node(RESPONDER_NODE, TAG)?),
            false,
            "pong",
            Some(CORE),
            &ConsumerFilter::Any,
            QoSProfile::Reliable,
        )
        .await?;
        let pong_slot: Arc<Mutex<Option<Subscription>>> = Arc::new(Mutex::new(Some(pong_sub)));

        let serve_runner = node_runner.clone();
        let serve_core = core.clone();
        let serve_inst = inst.clone();
        tokio::spawn(async move {
            let mut endpoint = ServiceMessenger::listen(
                serve_runner.messenger(),
                &serve_core,
                &serve_inst,
                SenderTarget::node(DRIVER_NODE, TAG).expect("driver target"),
                "bench_control",
            )
            .await
            .expect("listen bench_control");

            let _ = endpoint
                .handle_requests(move |req| {
                    let runner = serve_runner.clone();
                    let core = serve_core.clone();
                    let inst = serve_inst.clone();
                    let pong_slot = pong_slot.clone();
                    async move {
                        let bytes = req.message().payload().as_ref().to_vec();
                        let transport = bytes[0];
                        let warmup = u64::from_le_bytes(bytes[1..9].try_into().unwrap());
                        let iters = u64::from_le_bytes(bytes[9..17].try_into().unwrap());
                        let samples = if transport == 0 {
                            let mut sub = pong_slot
                                .lock()
                                .unwrap()
                                .take()
                                .expect("pong subscription present");
                            let result =
                                run_topic(&runner, &core, &inst, &mut sub, warmup, iters).await;
                            *pong_slot.lock().unwrap() = Some(sub);
                            result?
                        } else {
                            run_service(&runner, &core, &inst, warmup, iters).await?
                        };
                        Ok(encode_stats(samples))
                    }
                })
                .await;
        });

        Ok(())
    })
}
"####;

const RESPONDER_MAIN_RS: &str = r####"
use peppygen::{NodeBuilder, Result};
use peppylib::config::QoSProfile;
use peppylib::messaging::{ConsumerFilter, SenderTarget, ServiceMessenger, TopicMessenger};

const TAG: &str = "v1";
const CORE: &str = "bench_core";
const DRIVER_NODE: &str = "driver";
const RESPONDER_NODE: &str = "responder";

fn main() -> Result<()> {
    NodeBuilder::new().run(|_parameters: peppygen::Parameters, node_runner| async move {
        let core = node_runner.processor().bound_core_node().to_string();
        let inst = node_runner.processor().bound_instance_id().to_string();

        // Service echo: persistent queryable, answer requests forever.
        {
            let runner = node_runner.clone();
            let core = core.clone();
            let inst = inst.clone();
            tokio::spawn(async move {
                let mut endpoint = ServiceMessenger::listen(
                    runner.messenger(),
                    &core,
                    &inst,
                    SenderTarget::node(RESPONDER_NODE, TAG).expect("responder target"),
                    "echo",
                )
                .await
                .expect("listen echo");
                let _ = endpoint
                    .handle_requests(|req| async move { Ok(req.message().payload().clone()) })
                    .await;
            });
        }

        // Topic ping -> pong: persistent subscription, echo each ping back.
        {
            let runner = node_runner.clone();
            let core = core.clone();
            let inst = inst.clone();
            tokio::spawn(async move {
                let mut sub = TopicMessenger::subscribe(
                    runner.messenger(),
                    &core,
                    &inst,
                    Some(SenderTarget::node(DRIVER_NODE, TAG).expect("driver target")),
                    false,
                    "ping",
                    Some(CORE),
                    &ConsumerFilter::Any,
                    QoSProfile::Reliable,
                )
                .await
                .expect("subscribe ping");
                while let Some(msg) = sub.on_next_message().await {
                    let _ = TopicMessenger::emit(
                        runner.messenger(),
                        &core,
                        &inst,
                        SenderTarget::node(RESPONDER_NODE, TAG).expect("responder target"),
                        "pong",
                        QoSProfile::Reliable,
                        msg.payload().clone(),
                    )
                    .await;
                }
            });
        }

        Ok(())
    })
}
"####;

const RESPONDER_MAIN_PY: &str = r####"
import asyncio

from peppygen import NodeBuilder
from peppylib import QoSProfile, SenderTarget, ServiceMessenger, TopicMessenger

TAG = "v1"
CORE = "bench_core"
DRIVER_NODE = "driver"
RESPONDER_NODE = "responder"


async def serve_echo(node_runner):
    handle = node_runner.messenger()
    core = node_runner.bound_core_node()
    inst = node_runner.bound_instance_id()
    service = await ServiceMessenger.listen(
        handle, core, inst, SenderTarget.node(RESPONDER_NODE, TAG), "echo"
    )
    while True:
        handled = await service.handle_next_request(lambda request: request.payload)
        if handled is False:
            break


async def echo_topic(node_runner):
    handle = node_runner.messenger()
    core = node_runner.bound_core_node()
    inst = node_runner.bound_instance_id()
    subscription = await TopicMessenger.subscribe(
        handle,
        core,
        inst,
        SenderTarget.node(DRIVER_NODE, TAG),
        "ping",
        CORE,
        None,
        QoSProfile.Reliable,
    )
    while True:
        message = await subscription.on_next_message()
        if message is None:
            break
        await TopicMessenger.emit(
            handle,
            core,
            inst,
            SenderTarget.node(RESPONDER_NODE, TAG),
            "pong",
            QoSProfile.Reliable,
            message.payload,
        )


async def setup(parameters, node_runner):
    return [
        asyncio.create_task(serve_echo(node_runner)),
        asyncio.create_task(echo_topic(node_runner)),
    ]


def main():
    NodeBuilder().run(setup)


if __name__ == "__main__":
    main()
"####;
