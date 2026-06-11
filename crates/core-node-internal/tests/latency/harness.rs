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
use std::process::{Command, Stdio};
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
    /// Topic roundtrip where the driver publishes through a pre-bound
    /// publisher's loaned buffer (`loan` + `publish_loaned`) — the zero-copy
    /// path when shared memory is on; identical code over the heap when off.
    TopicLoaned,
}

impl Transport {
    pub fn as_str(self) -> &'static str {
        match self {
            Transport::Topic => "topic",
            Transport::Service => "service",
            Transport::TopicLoaned => "topic-loaned",
        }
    }

    fn wire_tag(self) -> u8 {
        match self {
            Transport::Topic => 0,
            Transport::Service => 1,
            Transport::TopicLoaned => 2,
        }
    }
}

/// One bench scenario: a responder language, a transport shape, and the
/// payload size the driver sends each roundtrip. `label` doubles as the
/// baseline key and the `cargo bench -- <filter>` id, so the historical
/// 8-byte scenarios keep their stored baselines.
#[derive(Clone, Copy, Debug)]
pub struct BenchScenario {
    pub lang: Lang,
    pub transport: Transport,
    pub payload_bytes: u64,
    pub label: &'static str,
}

/// The 8-byte legs measure pure transport overhead; the 1 MiB legs measure
/// the large-payload (camera-frame-sized) path where shared memory matters —
/// plain publish exercises the transparent copy-into-SHM tier, the loaned
/// variant the zero-copy tier.
pub const LARGE_PAYLOAD_BYTES: u64 = 1024 * 1024;

/// Default measurement parameters shared by the bench and the threshold test.
/// Larger warmup absorbs cold-start (route propagation, CPU ramp); 1000 samples
/// keep the reported percentiles steady. The guard asserts the median, which is
/// stable well below this count.
pub const DEFAULT_WARMUP: u64 = 100;
pub const DEFAULT_SAMPLES: u64 = 1000;

/// Worker-thread cap for every spawned node's tokio runtime, set via the
/// `TOKIO_WORKER_THREADS` env var (honored by `Runtime::new()`). Without it each
/// node spawns one worker per core, so the harness + driver + responder + router
/// heavily oversubscribe the box and preemption-multiplexing adds tail jitter to
/// sub-millisecond roundtrips. A small fixed cap keeps total busy workers near
/// the physical core count.
const NODE_WORKER_THREADS: &str = "2";

/// All scenarios, in display order.
pub const ALL_SCENARIOS: &[BenchScenario] = &[
    BenchScenario {
        lang: Lang::Rust,
        transport: Transport::Topic,
        payload_bytes: 8,
        label: "rust/topic",
    },
    BenchScenario {
        lang: Lang::Rust,
        transport: Transport::Service,
        payload_bytes: 8,
        label: "rust/service",
    },
    BenchScenario {
        lang: Lang::Rust,
        transport: Transport::Topic,
        payload_bytes: LARGE_PAYLOAD_BYTES,
        label: "rust/topic-1m",
    },
    BenchScenario {
        lang: Lang::Rust,
        transport: Transport::TopicLoaned,
        payload_bytes: LARGE_PAYLOAD_BYTES,
        label: "rust/topic-loaned-1m",
    },
    BenchScenario {
        lang: Lang::Python,
        transport: Transport::Topic,
        payload_bytes: 8,
        label: "python/topic",
    },
    BenchScenario {
        lang: Lang::Python,
        transport: Transport::Service,
        payload_bytes: 8,
        label: "python/service",
    },
];

/// Per (lang, transport) **median (p50)** ceiling in milliseconds — the
/// threshold the guard test asserts and the bench's status column reports
/// against. The median is the gated metric because it is stable run-to-run;
/// p90/p99 are too sample-starved at these counts to gate on without flaking.
/// Sized at ~4-6x the median observed on stabilized release runs (capped node
/// threads, one reused scenario per language), so a real regression (an
/// accidental debug build, a synchronous discovery per call, or a serialization
/// blowup) trips it while ordinary jitter does not. Overridable via
/// `PEPPY_LATENCY_MAX_MS_<LABEL>`, where `<LABEL>` is the scenario label
/// uppercased with `/` and `-` replaced by `_` (e.g. `rust/topic-loaned-1m` →
/// `PEPPY_LATENCY_MAX_MS_RUST_TOPIC_LOANED_1M`), so a slower runner can
/// retune without a code change.
pub fn ceiling_ms(scenario: &BenchScenario) -> u64 {
    // Derived from the scenario label so new scenarios automatically get
    // their own override key: `rust/topic-loaned-1m` →
    // `PEPPY_LATENCY_MAX_MS_RUST_TOPIC_LOANED_1M`.
    let key = format!(
        "PEPPY_LATENCY_MAX_MS_{}",
        scenario.label.to_uppercase().replace(['/', '-'], "_")
    );
    if let Ok(value) = std::env::var(&key)
        && let Ok(parsed) = value.parse()
    {
        return parsed;
    }
    match scenario.label {
        "rust/topic" => 8,
        "rust/service" => 8,
        // 1 MiB each way; generous so only a real regression (a lost
        // zero-copy path degrading to multiple large copies, a stalled
        // fallback) trips it.
        "rust/topic-1m" => 25,
        "rust/topic-loaned-1m" => 25,
        "python/topic" => 6,
        "python/service" => 10,
        other => panic!("no default ceiling for scenario {other}"),
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
    /// Whether shared memory was observed in use on the measured iterations:
    /// the final RECEIVED payload (echo reply / pong) was SHM-backed — and,
    /// for the loaned scenario, the driver's own loans were SHM-backed too.
    /// A degraded leg reports `false` while still delivering; note the plain
    /// scenarios cannot observe a driver-side-only degradation (the flag
    /// then reflects the responder→driver leg).
    shm_used: u64,
}

impl LatencyStats {
    fn decode(bytes: &[u8]) -> Self {
        assert_eq!(bytes.len(), 48, "bench_control response must be 48 bytes");
        let read = |i: usize| u64::from_le_bytes(bytes[i * 8..i * 8 + 8].try_into().unwrap());
        Self {
            total_ns: read(0),
            p50_ns: read(1),
            p90_ns: read(2),
            mean_ns: read(3),
            count: read(4),
            shm_used: read(5),
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
    pub fn shm_used(&self) -> bool {
        self.shm_used != 0
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

/// Shared incremental target dir for node builds (so the vendored
/// peppylib/zenoh stack is compiled once and reused across scenarios).
fn shared_target_dir() -> PathBuf {
    config::consts::PeppyDirs::default()
        .root()
        .join("cache/rust/test-targets")
}

/// Build a node crate in **release**. This is deliberate: production peppy nodes
/// run release (`build_cmd: ["cargo","build","--release"]`), and a debug build
/// of the vendored peppylib + zenoh stack is ~10x slower, which would make the
/// measured latency meaningless (and unfairly slower than the Python responder,
/// whose peppylib extension is always built in release). Builds into the shared
/// target dir under a lock, then copies the binary into the node's own dir so it
/// can be spawned without holding the cargo lock.
fn compile_rust_node_release(dir: &Path) {
    let target = shared_target_dir();
    std::fs::create_dir_all(&target).expect("create shared target dir");

    let lock_file = std::fs::File::create(target.join(".compile-release.lock"))
        .expect("create compile lock file");
    lock_file.lock().expect("acquire compile lock");

    let output = Command::new("cargo")
        .arg("build")
        .arg("--release")
        .env("CARGO_NET_OFFLINE", "true")
        .env("CARGO_TARGET_DIR", &target)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .expect("invoke cargo build --release on node crate");
    assert!(
        output.status.success(),
        "release build failed for node at {} (status {:?})\nstdout:\n{}\nstderr:\n{}",
        dir.display(),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );

    let binary = target.join("release").join("user_node");
    if binary.exists() {
        let local_dir = dir.join("target").join("release");
        std::fs::create_dir_all(&local_dir).expect("create local release dir");
        std::fs::copy(&binary, local_dir.join("user_node")).expect("copy release binary");
    }
    // Lock released on drop.
}

/// Spawn a release-built Rust node binary directly (mirrors
/// `helpers::spawn_cargo_run` but for `target/release/user_node`).
fn spawn_rust_node_release(dir: &Path, env_vars: &[(&str, &str)]) -> std::process::Child {
    let binary = dir.join("target").join("release").join("user_node");
    let mut command = Command::new(&binary);
    command
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .current_dir(dir);
    for &(key, value) in env_vars {
        command.env(key, value);
    }
    command.spawn().expect("spawn release node binary")
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
    compile_rust_node_release(&user_node);

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

/// On hosts with a small `RLIMIT_MEMLOCK`, the library's safe default segment
/// sizing (an eighth of the budget, tuned for many-sessions processes) can
/// fall below [`LARGE_PAYLOAD_BYTES`], silently turning the large-payload
/// scenarios into network-path measurements. The bench knows its own shape —
/// exactly two single-session node processes — so it can safely hand each
/// node a bigger slice of the budget: half, minus headroom for the peer's
/// mapped segment and zenoh's metadata segments.
fn shm_segment_override() -> Option<usize> {
    let limits = std::fs::read_to_string("/proc/self/limits").ok()?;
    let bytes = shm_segment_override_from_limits(&limits)?;
    usize::try_from(bytes).ok()
}

fn shm_segment_override_from_limits(limits: &str) -> Option<u64> {
    let line = limits
        .lines()
        .find(|l| l.starts_with("Max locked memory"))?;
    let soft = parse_max_locked_memory_soft_limit(line)?;
    let segment = (soft / 2).saturating_sub(2 * 1024 * 1024);
    if segment < LARGE_PAYLOAD_BYTES {
        return None;
    }
    Some(segment.min(LARGE_PAYLOAD_BYTES * 2))
}

fn parse_max_locked_memory_soft_limit(line: &str) -> Option<u64> {
    let mut parts = line.strip_prefix("Max locked memory")?.split_whitespace();
    let soft = parts.next()?;
    let _hard = parts.next()?;
    let units = parts.next().unwrap_or("bytes");
    if soft == "unlimited" {
        return None;
    }
    let soft: u64 = soft.parse().ok()?;
    match units {
        "bytes" => Some(soft),
        "kB" | "KB" | "kb" => soft.checked_mul(1024),
        _ => None,
    }
}

fn write_runtime_config(
    cfg_dir: &Path,
    host: &str,
    port: u16,
    node_name: &str,
    instance_id: &str,
    shm_segment_bytes: Option<usize>,
) -> PathBuf {
    let mut runtime_config = RuntimeConfig::new(
        host,
        port,
        NodeInstanceConfig::new(Name::new(instance_id).expect("instance name")),
        node_name,
        TAG,
        CORE,
    )
    .expect("build runtime config");
    // The bench-shaped segment override travels the same path a daemon-set
    // `shm.segment_bytes` would: the node's runtime config discovery block.
    runtime_config.discovery.shm_segment_bytes = shm_segment_bytes;
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
    let shm_segment = shm_segment_override();
    let driver_cfg = write_runtime_config(
        cfg_dir.path(),
        &host,
        port,
        DRIVER_NODE,
        DRIVER_INST,
        shm_segment,
    );
    let responder_cfg = write_runtime_config(
        cfg_dir.path(),
        &host,
        port,
        RESPONDER_NODE,
        RESPONDER_INST,
        shm_segment,
    );

    // Spawn the responder first so its echo service / ping subscription are up
    // before the driver starts probing.
    let responder_env = vec![
        (RUNTIME_CONFIG_VAR_NAME, responder_cfg.to_str().unwrap()),
        ("TOKIO_WORKER_THREADS", NODE_WORKER_THREADS),
    ];
    let driver_env = vec![
        (RUNTIME_CONFIG_VAR_NAME, driver_cfg.to_str().unwrap()),
        ("TOKIO_WORKER_THREADS", NODE_WORKER_THREADS),
    ];
    let mut responder_child = match lang {
        Lang::Rust => spawn_rust_node_release(&responder_dir, &responder_env),
        Lang::Python => helpers::spawn_python_run(&responder_dir, &responder_env),
    };
    let mut driver_child = spawn_rust_node_release(&driver_dir, &driver_env);

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
    /// roundtrips of the scenario's transport at its payload size and return
    /// the reported distribution.
    pub async fn run(&self, scenario: &BenchScenario, warmup: u64, iters: u64) -> LatencyStats {
        let mut request = Vec::with_capacity(25);
        request.push(scenario.transport.wire_tag());
        request.extend_from_slice(&warmup.to_le_bytes());
        request.extend_from_slice(&iters.to_le_bytes());
        request.extend_from_slice(&scenario.payload_bytes.to_le_bytes());

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

        let stats = LatencyStats::decode(response.payload().as_ref());
        // A short count means the driver's measured loop ended early (e.g. the
        // pong stream closed mid-run), which would silently report percentiles
        // over a truncated sample set. Fail loudly instead of trusting it.
        assert_eq!(
            stats.count(),
            iters,
            "short sample count from driver: expected {iters} measured samples, got {} (stream closed early?)",
            stats.count()
        );
        stats
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

impl Drop for Scenario {
    /// Panic-safe backstop: `std::process::Child` does not kill on drop, so a
    /// test that panics before `shutdown()` would otherwise orphan the driver +
    /// responder. Kill and reap both here. This is a no-op after a normal
    /// `shutdown()` (the children are already reaped, so `kill()` returns `Ok`
    /// without signaling and `wait()` returns the cached status); errors are
    /// ignored so cleanup never panics during unwinding.
    fn drop(&mut self) {
        let _ = self.driver_child.kill();
        let _ = self.driver_child.wait();
        let _ = self.responder_child.kill();
        let _ = self.responder_child.wait();
    }
}

/// Convenience for one-shot measurements: full spawn -> measure -> shutdown.
pub async fn run_once(scenario: &BenchScenario, warmup: u64, iters: u64) -> LatencyStats {
    let running = start_scenario(scenario.lang).await;
    let stats = running.run(scenario, warmup, iters).await;
    running.shutdown().await;
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

/// A roundtrip payload: `len` bytes (min 8) with the sequence number in the
/// first 8 so stale pongs are recognizable without comparing whole buffers.
fn seq_payload(seq: u64, len: u64) -> Vec<u8> {
    let mut buf = vec![0u8; (len as usize).max(8)];
    buf[..8].copy_from_slice(&seq.to_le_bytes());
    buf
}

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

fn encode_stats(mut samples: Vec<u64>, shm_used: bool) -> Payload {
    samples.sort_unstable();
    let count = samples.len() as u64;
    let total: u64 = samples.iter().sum();
    let p50 = percentile(&samples, 0.50);
    let p90 = percentile(&samples, 0.90);
    let mean = if count == 0 { 0 } else { total / count };
    let mut out = Vec::with_capacity(48);
    for value in [total, p50, p90, mean, count, shm_used as u64] {
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
    payload_len: u64,
) -> Result<(Vec<u64>, bool)> {
    let messenger = node_runner.messenger();
    let mut samples = Vec::with_capacity(iters as usize);
    let mut shm_used = false;
    for i in 0..(warmup + iters) {
        let payload = Payload::from(seq_payload(i, payload_len));
        let start = Instant::now();
        let response = ServiceMessenger::poll(
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
            shm_used = response.payload_is_shm_backed();
        }
    }
    Ok((samples, shm_used))
}

enum Pong {
    Matched { shm: bool },
    Closed,
    TimedOut,
}

/// Awaits the pong matching `seq` (first 8 payload bytes; whole-buffer
/// compares would distort 1 MiB roundtrips).
async fn await_pong(sub: &mut Subscription, seq: u64) -> Pong {
    loop {
        match tokio::time::timeout(PONG_TIMEOUT, sub.on_next_message()).await {
            Ok(Some(msg)) => {
                let payload = msg.payload();
                if payload.len() >= 8 && payload[..8] == seq.to_le_bytes() {
                    return Pong::Matched {
                        shm: msg.payload_is_shm_backed(),
                    };
                }
                // Stale pong from an earlier seq; keep draining.
            }
            Ok(None) => return Pong::Closed,
            Err(_) => return Pong::TimedOut,
        }
    }
}

async fn run_topic(
    node_runner: &peppygen::NodeRunner,
    core: &str,
    inst: &str,
    sub: &mut Subscription,
    warmup: u64,
    iters: u64,
    payload_len: u64,
) -> Result<(Vec<u64>, bool)> {
    let messenger = node_runner.messenger();
    let mut samples = Vec::with_capacity(iters as usize);
    let mut shm_used = false;
    for i in 0..(warmup + iters) {
        let payload = Payload::from(seq_payload(i, payload_len));
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
            match await_pong(sub, i).await {
                Pong::Matched { shm } => {
                    if i >= warmup {
                        samples.push(start.elapsed().as_nanos() as u64);
                        shm_used = shm;
                    }
                    break;
                }
                Pong::Closed => return Ok((samples, shm_used)),
                Pong::TimedOut => {
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
    }
    Ok((samples, shm_used))
}

/// The zero-copy variant: a pre-bound publisher hands out loaned buffers the
/// driver fills in place. With shared memory on, the publish never copies the
/// payload again; with it off, the identical code runs over heap loans.
async fn run_topic_loaned(
    node_runner: &peppygen::NodeRunner,
    core: &str,
    inst: &str,
    sub: &mut Subscription,
    warmup: u64,
    iters: u64,
    payload_len: u64,
) -> Result<(Vec<u64>, bool)> {
    let publisher = TopicMessenger::declare_publisher(
        node_runner.messenger(),
        core,
        inst,
        SenderTarget::node(DRIVER_NODE, TAG)?,
        None,
        "ping",
        QoSProfile::Reliable,
    )
    .await?;
    let mut samples = Vec::with_capacity(iters as usize);
    let mut shm_used = false;
    for i in 0..(warmup + iters) {
        // The loan is taken and filled OUTSIDE the clock, mirroring the plain
        // variant whose payload is also built before the clock starts: both
        // scenarios measure publish-to-delivery, not frame production.
        let mut loan = publisher.loan((payload_len as usize).max(8));
        // The driver's own publish leg is what this scenario varies, so its
        // tier feeds the shm flag too — a driver-side-only degradation must
        // not report as zero-copy just because the pongs still arrive in SHM.
        let mut loan_shm = loan.is_shm();
        loan[..8].copy_from_slice(&i.to_le_bytes());
        let mut start = Instant::now();
        publisher.publish_loaned(loan).await?;
        loop {
            match await_pong(sub, i).await {
                Pong::Matched { shm } => {
                    if i >= warmup {
                        samples.push(start.elapsed().as_nanos() as u64);
                        shm_used = shm && loan_shm;
                    }
                    break;
                }
                Pong::Closed => return Ok((samples, shm_used)),
                Pong::TimedOut => {
                    // Lost ping: re-loan, re-publish, reset clock.
                    start = Instant::now();
                    let mut loan = publisher.loan((payload_len as usize).max(8));
                    loan_shm = loan.is_shm();
                    loan[..8].copy_from_slice(&i.to_le_bytes());
                    publisher.publish_loaned(loan).await?;
                }
            }
        }
    }
    Ok((samples, shm_used))
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
                        let payload_len = u64::from_le_bytes(bytes[17..25].try_into().unwrap());
                        let (samples, shm_used) = match transport {
                            0 | 2 => {
                                let mut sub = pong_slot
                                    .lock()
                                    .unwrap()
                                    .take()
                                    .expect("pong subscription present");
                                let result = if transport == 0 {
                                    run_topic(
                                        &runner, &core, &inst, &mut sub, warmup, iters,
                                        payload_len,
                                    )
                                    .await
                                } else {
                                    run_topic_loaned(
                                        &runner, &core, &inst, &mut sub, warmup, iters,
                                        payload_len,
                                    )
                                    .await
                                };
                                *pong_slot.lock().unwrap() = Some(sub);
                                result?
                            }
                            1 => {
                                run_service(&runner, &core, &inst, warmup, iters, payload_len)
                                    .await?
                            }
                            other => panic!("unknown bench transport tag {other}"),
                        };
                        Ok(encode_stats(samples, shm_used))
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
                    .handle_requests(|req| async move { Ok(req.message().payload().to_owned()) })
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
                        msg.payload().to_owned(),
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

#[cfg(test)]
mod tests {
    // Used by the test target. The `harness = false` bench `#[path]`-includes this
    // file too and reports the glob as unused there, so silence it for that build.
    #[allow(unused_imports)]
    use super::*;

    const PREFIX: &str = "\
Limit                     Soft Limit           Hard Limit           Units
Max cpu time              unlimited            unlimited            seconds
";

    #[test]
    fn shm_segment_override_uses_soft_memlock_limit_in_bytes() {
        let limits = format!(
            "{PREFIX}Max locked memory         {}             {}             bytes\n",
            8 * 1024 * 1024,
            64 * 1024 * 1024
        );

        assert_eq!(
            shm_segment_override_from_limits(&limits),
            Some(LARGE_PAYLOAD_BYTES * 2)
        );
    }

    #[test]
    fn shm_segment_override_converts_kb_soft_memlock_limit() {
        let limits = format!(
            "{PREFIX}Max locked memory         {}                {}               kB\n",
            8 * 1024,
            64 * 1024
        );

        assert_eq!(
            shm_segment_override_from_limits(&limits),
            Some(LARGE_PAYLOAD_BYTES * 2)
        );
    }

    #[test]
    fn shm_segment_override_caps_at_large_payload_double() {
        let limits = format!(
            "{PREFIX}Max locked memory         {}            {}            bytes\n",
            64 * 1024 * 1024,
            64 * 1024 * 1024
        );

        assert_eq!(
            shm_segment_override_from_limits(&limits),
            Some(LARGE_PAYLOAD_BYTES * 2)
        );
    }

    #[test]
    fn shm_segment_override_skips_unusable_or_unlimited_limits() {
        let too_small = format!(
            "{PREFIX}Max locked memory         {}             {}             bytes\n",
            5 * 1024 * 1024,
            64 * 1024 * 1024
        );
        let unlimited = format!(
            "{PREFIX}Max locked memory         unlimited            unlimited            bytes\n"
        );

        assert_eq!(shm_segment_override_from_limits(&too_small), None);
        assert_eq!(shm_segment_override_from_limits(&unlimited), None);
    }
}
