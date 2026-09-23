#![cfg(feature = "multi_daemon_e2e")]

use std::collections::{BTreeMap, BTreeSet};
use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::consts::{PEPPY_CONFIG_ENV, PEPPY_HOME_ENV};
use core_node::{TEARDOWN_REAP_BUDGET, force_kill_deadline};
use daemon_config::peppy_config::{
    ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
};
use peppy::test_support::CACHED_BUILD_REUSE_PREFIX;
use pmi::{RouterId, ZenohAdapter, ZenohNetProtocol, render_router_config};
use testcontainers::core::client::docker_client_instance;
use testcontainers::core::{AccessMode, CmdWaitFor, ExecCommand, Host, Mount};
use testcontainers::runners::{AsyncBuilder, AsyncRunner};
use testcontainers::{ContainerAsync, GenericBuildableImage, GenericImage, ImageExt};
use tokio::sync::OnceCell;

const TIMEOUT: Duration = Duration::from_secs(60);

const IMAGE_OVERRIDE_ENV: &str = "PEPPY_MULTI_DAEMON_E2E_IMAGE";
const MANAGED_ROUTER_PORT: u16 = 7447;
const CONTAINER_ROUTER_CONFIG: &str = "/etc/peppy/router.json5";
const CONTAINER_PEPPY_BINARY: &str = "/usr/local/bin/peppy";

/// `PEPPY_HOME` inside every daemon container. Used verbatim as the data root,
/// so a run log is `$CONTAINER_PEPPY_HOME/logs/run/<instance>.log` with no
/// `.peppy` segment in between.
const CONTAINER_PEPPY_HOME: &str = "/data";

/// How long the engine waits for a daemon to stop before it kills it.
///
/// A daemon catches the stop signal and tears its node stack down first:
/// every node is asked to stop cooperatively, a straggler is force-killed at
/// [`force_kill_deadline`], and the group is reaped inside
/// [`TEARDOWN_REAP_BUDGET`]. A shorter window kills the daemon partway
/// through, so what its container still has to unwind depends on where the
/// teardown had reached. Derived from the daemon's own deadline the way the
/// messaging router sizes its teardown budget, plus the same second of margin
/// so the kill cannot land exactly as the teardown ends.
fn stop_grace_secs() -> i32 {
    let grace = Duration::from_secs(PeppyConfig::default().lifecycle.shutdown_grace_secs);
    let budget = force_kill_deadline(grace) + TEARDOWN_REAP_BUDGET + Duration::from_secs(1);
    i32::try_from(budget.as_secs()).expect("the daemon's teardown budget fits a stop timeout")
}

/// The grace the engine gives a daemon has to outlast the daemon's own
/// teardown, or the kill lands on one that is still stopping its nodes.
#[test]
fn the_stop_grace_outlasts_the_daemons_own_teardown() {
    let grace = Duration::from_secs(PeppyConfig::default().lifecycle.shutdown_grace_secs);
    let teardown = force_kill_deadline(grace) + TEARDOWN_REAP_BUDGET;
    let engine = Duration::from_secs(stop_grace_secs().try_into().expect("a positive grace"));
    assert!(
        engine > teardown,
        "the engine kills a daemon after {engine:?}, and the daemon's own teardown runs to {teardown:?}"
    );
}

/// Name of the image this test builds for its daemon containers.
const E2E_IMAGE_NAME: &str = "peppy-multi-daemon-e2e";

/// Pinned `uv` release copied into the image. A moving `latest` would make a
/// green run depend on what Astral published that morning.
const UV_VERSION: &str = "0.12.13";

/// The interpreter the fixture repository's nodes ask for (`requires-python
/// ">=3.13,<3.14"`). Baked into the image so no node build has to fetch one.
const NODE_PYTHON_VERSION: &str = "3.13";

async fn require_docker() {
    let client = docker_client_instance()
        .await
        .expect("a Docker client must be constructible on the test host");
    client
        .ping()
        .await
        .expect("the Docker daemon must be reachable on the test host");
}

struct ExecOutput {
    exit_code: Option<i64>,
    /// Stdout followed by stderr, both lossily decoded.
    text: String,
}

impl ExecOutput {
    fn success(&self) -> bool {
        self.exit_code == Some(0)
    }
}

/// The output of a stack change that reached every set it changed. A set the
/// daemon could not deliver is reported and the change stands, so a test that
/// expects delivery has to look.
fn require_delivered(output: ExecOutput, operation: &str) -> String {
    let text = require_success(output, operation);
    assert!(
        !text.contains("did not reach their instances"),
        "{operation}: a changed set did not reach its instance:\n{text}"
    );
    text
}

fn require_success(output: ExecOutput, operation: &str) -> String {
    if !output.success() {
        panic!(
            "{operation} failed (exit code {:?}):\n{}",
            output.exit_code, output.text
        );
    }
    output.text
}

fn external_daemon_config(core_node: &str, router_port: u16) -> String {
    let mut config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        ..PeppyConfig::default()
    };
    config.zenoh = ZenohConfig::External(ExternalZenohConfig {
        endpoint: format!("tcp/host.docker.internal:{router_port}"),
    });
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

fn managed_daemon_config(core_node: &str) -> String {
    let mut managed = ManagedZenohConfig::default();
    // The pinned router links below do not need a backend. Keep the armed
    // federation task's failed backend resolution from delaying startup.
    managed.federation.connect_timeout_secs = 1;
    let config = PeppyConfig {
        core_node_name: Some(core_node.to_string()),
        zenoh: ZenohConfig::Managed(managed),
        ..PeppyConfig::default()
    };
    serde_json::to_string(&config).expect("full daemon config should serialize")
}

fn write_router_pin(path: &Path, connect_endpoints: Vec<String>) {
    let config = render_router_config(
        ZenohNetProtocol::Tcp,
        "0.0.0.0",
        MANAGED_ROUTER_PORT,
        true,
        connect_endpoints,
        None,
        // A pinned config is the operator's, identity included; the daemon's own
        // persisted identity is never rendered over it.
        &RouterId::generate(),
    );
    std::fs::write(path, config).expect("write pinned zenohd config");
}

fn executable_on_path(name: &str) -> PathBuf {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|directory| directory.join(name))
        .find(|candidate| candidate.is_file())
        .unwrap_or_else(|| panic!("{name} must be installed on the Docker test host"))
}

/// Locates the bundled zenohd built by pmi's `build_zenoh` feature. Cargo puts
/// it under this profile's `build/pmi-*/out` directory; a neighboring binary,
/// an explicit environment override, and PATH remain useful for packaged runs.
fn bundled_zenohd_binary() -> PathBuf {
    let current_exe = std::env::current_exe().expect("resolve current test executable");
    if let Some(directory) = current_exe.parent() {
        let candidate = directory.join("zenohd");
        if candidate.is_file() {
            return candidate;
        }
    }

    if let Some(candidate) = std::env::var_os("ZENOHD_BINARY_PATH").map(PathBuf::from)
        && candidate.is_file()
    {
        return candidate;
    }

    let mut built_candidates = current_exe
        .parent()
        .and_then(Path::parent)
        .map(|profile| profile.join("build"))
        .and_then(|build| std::fs::read_dir(build).ok())
        .into_iter()
        .flatten()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().starts_with("pmi-"))
        .map(|entry| entry.path().join("out/zenohd"))
        .filter(|candidate| candidate.is_file())
        .collect::<Vec<_>>();
    built_candidates.sort_by_key(|candidate| {
        candidate
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(UNIX_EPOCH)
    });
    built_candidates
        .pop()
        .unwrap_or_else(|| executable_on_path("zenohd"))
}

/// The Ubuntu release the e2e image is based on.
///
/// Matching the runner keeps a host-built binary from targeting a newer glibc
/// than the container provides. Non-Ubuntu runners must select a compatible
/// image explicitly with `PEPPY_MULTI_DAEMON_E2E_IMAGE`.
fn host_ubuntu_release() -> String {
    let release = std::fs::read_to_string("/etc/os-release")
        .expect("read /etc/os-release or set PEPPY_MULTI_DAEMON_E2E_IMAGE");
    let value = |key: &str| {
        release.lines().find_map(|line| {
            let (candidate, value) = line.split_once('=')?;
            (candidate == key).then(|| value.trim_matches('"').to_string())
        })
    };
    let distro = value("ID").expect("/etc/os-release must contain ID");
    let version = value("VERSION_ID").expect("/etc/os-release must contain VERSION_ID");
    assert_eq!(
        distro, "ubuntu",
        "set {IMAGE_OVERRIDE_ENV} to a container image compatible with this {distro} runner"
    );
    version
}

/// The image body. A bare Ubuntu image cannot run this repository's nodes:
///
/// - `squashfs-tools` is what apptainer calls to pack a SIF, so a container
///   node (`uvc_camera_python_mock`) cannot be built without it.
/// - `uv` builds every native Python node (`my_python_robot_arm` and friends
///   run `uv sync`). Peppy vendors `ruff`, not `uv`, so the image provides it.
/// - `ca-certificates` covers apptainer pulling a node's Docker base image and
///   `uv` resolving a node's dependencies from PyPI. `peppy repo refresh` needs
///   none of it: it reads the fixture repository over `file://`.
/// - `tzdata` exists for `/etc/localtime` alone. Apptainer binds it into every
///   container it starts, and a bare Ubuntu image does not have it, so the
///   node's own container fails to be created with a `mount source
///   /etc/localtime doesn't exist` that says nothing about time zones.
/// - The `safe.directory` entry covers the fixture repository. It is bind
///   mounted from the host, so it belongs to the user running the tests while
///   the daemon reads it as root, and libgit2 refuses to open a repository the
///   current user does not own with `is not owned by current user`. Named
///   explicitly rather than as a wildcard: this is the one repository in the
///   image that anybody else owns.
fn e2e_dockerfile(ubuntu_release: &str) -> String {
    format!(
        "FROM ubuntu:{ubuntu_release}\n\
         RUN apt-get update \\\n\
         \x20&& apt-get install -y --no-install-recommends \\\n\
         \x20     ca-certificates squashfs-tools tzdata \\\n\
         \x20&& ln -sf /usr/share/zoneinfo/UTC /etc/localtime \\\n\
         \x20&& rm -rf /var/lib/apt/lists/*\n\
         RUN printf '[safe]\\n\\tdirectory = {CONTAINER_FIXTURE_REPO}\\n' > /etc/gitconfig\n\
         COPY --from=ghcr.io/astral-sh/uv:{UV_VERSION} /uv /uvx /usr/local/bin/\n\
         ENV UV_PYTHON_INSTALL_DIR=/opt/uv-python\n\
         RUN uv python install {NODE_PYTHON_VERSION}\n"
    )
}

/// The daemon image, built once per test binary.
///
/// Every test starts several containers and they all want the same image;
/// building it per container would serialize seven redundant Docker builds
/// behind each other. `PEPPY_MULTI_DAEMON_E2E_IMAGE` bypasses the build
/// entirely for runs that supply a prepared image.
async fn e2e_image() -> (String, String) {
    static IMAGE: OnceCell<(String, String)> = OnceCell::const_new();
    IMAGE
        .get_or_init(|| async {
            if let Ok(image) = std::env::var(IMAGE_OVERRIDE_ENV)
                && !image.trim().is_empty()
            {
                return split_image_reference(image.trim());
            }

            let release = host_ubuntu_release();
            // The build leaves the image tagged `E2E_IMAGE_NAME:release` in
            // the local daemon, which is the part every container needs;
            // `start_daemon` builds its own request per container anyway.
            build_e2e_daemon_image(&release).await;
            (String::from(E2E_IMAGE_NAME), release)
        })
        .await
        .clone()
}

/// Builds the daemon image into the local Docker daemon.
///
/// Prefers the buildx client: `docker buildx build` resolves the configured
/// default builder, and on the self-hosted CI runner that builder belongs to a
/// Docker daemon that outlives the job, so its layer cache persists and the apt
/// and uv-python layers of [`e2e_dockerfile`] are built once and reused across
/// runs. On a machine that starts clean the cache starts empty and the image is
/// built from scratch. Where no buildx client is installed the testcontainers
/// build remains, driving the daemon's own embedded builder and building the
/// identical image body either way.
///
/// `--load` exports the built image into the local daemon, which is where
/// the containers below expect to find it. The Dockerfile arrives on stdin
/// (`--file -`) and the build context is an empty directory, so the image
/// body stays defined in exactly one place, [`e2e_dockerfile`].
async fn build_e2e_daemon_image(release: &str) {
    if buildx_available().await {
        let tag = format!("{E2E_IMAGE_NAME}:{release}");
        let dockerfile = e2e_dockerfile(release);
        let built_at = Instant::now();
        let tag_for_build = tag.clone();
        let output = tokio::task::spawn_blocking(move || {
            use std::io::Write;
            let context = std::env::temp_dir().join("peppy-multi-daemon-e2e-context");
            std::fs::create_dir_all(&context)
                .expect("create the e2e image build context directory");
            let mut child = std::process::Command::new("docker")
                .args([
                    "buildx",
                    "build",
                    "--load",
                    "--progress=plain",
                    "--tag",
                    &tag_for_build,
                    "--file",
                    "-",
                ])
                .arg(&context)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("spawn docker buildx build");
            let mut stdin = child.stdin.take().expect("piped stdin for buildx");
            let write = stdin.write_all(dockerfile.as_bytes());
            drop(stdin);
            write.expect("write the e2e Dockerfile to buildx stdin");
            child.wait_with_output().expect("run docker buildx build")
        })
        .await
        .expect("join the blocking e2e image build");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "docker buildx build of the e2e daemon image failed:\n{stderr}",
        );
        // Plain progress names every step BuildKit served from the layer
        // cache, so the count is the run-to-run reuse made visible.
        let cached_steps = stderr.lines().filter(|l| l.contains(" CACHED ")).count();
        let elapsed = built_at.elapsed().as_secs_f32();
        println!(
            "built {tag} through the default buildx builder in {elapsed:.1}s ({cached_steps} cached steps)"
        );
        // The test harness captures stdout, so surface the same numbers in the
        // GitHub step summary, where a passing run still shows them.
        if let Some(summary) = std::env::var_os("GITHUB_STEP_SUMMARY") {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&summary) {
                let _ = writeln!(
                    file,
                    "e2e daemon image: built in {elapsed:.1}s via buildx, {cached_steps} cached steps"
                );
            }
        }
        return;
    }

    let _tagged = GenericBuildableImage::new(E2E_IMAGE_NAME, release)
        .with_dockerfile_string(e2e_dockerfile(release))
        .build_image()
        .await
        .expect("building the e2e daemon image must succeed");
}

/// Whether a `docker buildx` client answers, ahead of choosing the build
/// path in [`build_e2e_daemon_image`].
async fn buildx_available() -> bool {
    tokio::task::spawn_blocking(|| {
        std::process::Command::new("docker")
            .args(["buildx", "version"])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success())
    })
    .await
    .unwrap_or(false)
}

/// `GenericImage` wants the name and tag separately and joins them back with a
/// colon. Splitting at the last colon (unless it belongs to a registry port)
/// reproduces the original reference, `name@sha256` digests included.
fn split_image_reference(reference: &str) -> (String, String) {
    match reference.rsplit_once(':') {
        Some((name, tag)) if !tag.contains('/') => (name.to_string(), tag.to_string()),
        _ => (reference.to_string(), String::from("latest")),
    }
}

/// Where every daemon container reads the fixture repository, and so the
/// `file://` URL its `repositories.json5` names.
///
/// The same path in every container deliberately. A peer materializes a pinned
/// item by cloning the pin's own `repo_url`, so a path that existed only on the
/// coordinator would fail the moment a launch placed anything off it.
const CONTAINER_FIXTURE_REPO: &str = "/etc/peppy/fixture-hub";

/// The fixture tree this crate commits, relative to its manifest directory.
const FIXTURE_REPO_SOURCE: &str = "tests/fixtures/hub";

/// The one repository every daemon in this suite resolves deployments from.
///
/// A git repository rather than an `fs` one because a deployment resolved from
/// an `fs` entry may not be placed on another core node, and placing one there
/// is what most of these tests do. Built per test, so nothing carries between
/// them; both of a test's containers mount the same directory, because the
/// commit the coordinator pins is the one the peer has to fetch.
struct FixtureRepository {
    /// The git repository, mounted at [`CONTAINER_FIXTURE_REPO`].
    repo: tempfile::TempDir,
    /// Holds the single-entry `repositories.json5` mounted over the daemon's
    /// own, so the bundled defaults are never written and no daemon here has a
    /// network repository to read.
    conf: tempfile::TempDir,
}

impl FixtureRepository {
    fn create() -> Self {
        let repo = tempfile::tempdir().expect("create the fixture repository directory");
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE_REPO_SOURCE);
        copy_tree(&source, repo.path());
        commit_worktree(repo.path());

        let conf = tempfile::tempdir().expect("create the fixture conf directory");
        std::fs::write(
            conf.path().join("repositories.json5"),
            // An explicit id, so reading this file never rewrites it: peppy
            // assigns missing ids and writes the result back, which a read-only
            // mount would refuse.
            format!("[{{ id: 1000, type: \"git\", url: \"file://{CONTAINER_FIXTURE_REPO}\" }}]\n"),
        )
        .expect("write the fixture repositories.json5");

        Self { repo, conf }
    }

    fn repositories_config(&self) -> PathBuf {
        self.conf.path().join("repositories.json5")
    }
}

/// Copies the contents of `from` into `to`, which must already exist.
fn copy_tree(from: &Path, to: &Path) {
    let entries = std::fs::read_dir(from)
        .unwrap_or_else(|error| panic!("reading {}: {error}", from.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|error| panic!("reading {}: {error}", from.display()));
        let source = entry.path();
        let target = to.join(entry.file_name());
        let file_type = entry
            .file_type()
            .unwrap_or_else(|error| panic!("stat {}: {error}", source.display()));
        if file_type.is_dir() {
            std::fs::create_dir(&target)
                .unwrap_or_else(|error| panic!("creating {}: {error}", target.display()));
            copy_tree(&source, &target);
        } else {
            std::fs::copy(&source, &target).unwrap_or_else(|error| {
                panic!(
                    "copying {} to {}: {error}",
                    source.display(),
                    target.display()
                )
            });
        }
    }
}

/// Commits everything under `dir` as a fresh git repository.
///
/// The identity and the timestamp are fixed here rather than read from the
/// host's git configuration, so a machine with no `user.email` configured, or
/// with commit signing turned on, produces the same repository as any other.
fn commit_worktree(dir: &Path) {
    let repository = git2::Repository::init(dir)
        .unwrap_or_else(|error| panic!("git init in {}: {error}", dir.display()));
    let mut index = repository
        .index()
        .unwrap_or_else(|error| panic!("opening the index in {}: {error}", dir.display()));
    index
        .add_all(["*"], git2::IndexAddOption::DEFAULT, None)
        .unwrap_or_else(|error| panic!("staging {}: {error}", dir.display()));
    index
        .write()
        .unwrap_or_else(|error| panic!("writing the index in {}: {error}", dir.display()));
    let tree_id = index
        .write_tree()
        .unwrap_or_else(|error| panic!("writing the tree in {}: {error}", dir.display()));
    let tree = repository
        .find_tree(tree_id)
        .unwrap_or_else(|error| panic!("reading back the tree in {}: {error}", dir.display()));
    let author = git2::Signature::new(
        "peppy multi-daemon e2e",
        "e2e@peppy.invalid",
        &git2::Time::new(0, 0),
    )
    .expect("the fixture commit signature should be valid");
    repository
        .commit(
            Some("HEAD"),
            &author,
            &author,
            "Fixture repository",
            &tree,
            &[],
        )
        .unwrap_or_else(|error| panic!("committing {}: {error}", dir.display()));
}

struct DaemonLaunch<'a> {
    image_name: &'a str,
    image_tag: &'a str,
    peppy_binary: &'a Path,
    apptainer_dir: &'a Path,
    newuidmap: &'a Path,
    fixture: &'a FixtureRepository,
}

#[derive(Clone, Copy)]
struct ManagedRouterMount<'a> {
    zenohd_binary: &'a Path,
    config: &'a Path,
}

fn read_only_bind(host_path: &Path, container_path: &str) -> Mount {
    Mount::bind_mount(host_path.display().to_string(), container_path)
        .with_access_mode(AccessMode::ReadOnly)
}

/// A running daemon container. The guard owns cleanup: dropping it removes the
/// container (panic unwinds included), so failed assertions cannot leak
/// containers.
struct Daemon {
    name: String,
    container: ContainerAsync<GenericImage>,
}

impl Daemon {
    /// Runs `peppy` inside this container and returns its combined output.
    async fn peppy(&self, args: &[&str]) -> ExecOutput {
        self.peppy_with_env(&[], args).await
    }

    /// Runs `peppy` with `KEY=value` entries layered over the container's own
    /// environment via `env(1)`: what an operator's shell does to the CLI's
    /// environment, done to this exec.
    async fn peppy_with_env(&self, env: &[&str], args: &[&str]) -> ExecOutput {
        let mut cmd = vec!["/usr/bin/env"];
        cmd.extend_from_slice(env);
        cmd.push(CONTAINER_PEPPY_BINARY);
        cmd.extend_from_slice(args);
        self.exec(cmd).await
    }

    /// Runs one command inside this container and returns its combined output.
    async fn exec(&self, cmd: Vec<&str>) -> ExecOutput {
        let mut result = self
            .container
            .exec(ExecCommand::new(cmd).with_cmd_ready_condition(CmdWaitFor::exit()))
            .await
            .unwrap_or_else(|error| panic!("failed to exec in {}: {error}", self.name));
        let exit_code = result
            .exit_code()
            .await
            .unwrap_or_else(|error| panic!("exec exit code in {}: {error}", self.name));
        let stdout = result
            .stdout_to_vec()
            .await
            .unwrap_or_else(|error| panic!("exec stdout in {}: {error}", self.name));
        let stderr = result
            .stderr_to_vec()
            .await
            .unwrap_or_else(|error| panic!("exec stderr in {}: {error}", self.name));
        ExecOutput {
            exit_code,
            text: format!(
                "{}{}",
                String::from_utf8_lossy(&stdout),
                String::from_utf8_lossy(&stderr)
            ),
        }
    }

    async fn stack_list(&self, target: Option<&str>) -> ExecOutput {
        let mut cmd = vec!["stack", "list"];
        if let Some(target) = target {
            cmd.extend(["--core-node", target]);
        }
        self.peppy(&cmd).await
    }

    async fn wait_for_stack(&self, predicate: impl Fn(&str) -> bool) -> String {
        let started = Instant::now();
        let mut last = String::new();
        while started.elapsed() < TIMEOUT {
            let output = self.stack_list(None).await;
            last = output.text;
            if output.exit_code == Some(0) && predicate(&last) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        let logs = self.logs().await;
        panic!(
            "timed out waiting for stack list in {}; last output:\n{last}\ncontainer logs:\n{logs}",
            self.name
        );
    }

    async fn wait_for_exit(&self) -> i64 {
        let started = Instant::now();
        while started.elapsed() < TIMEOUT {
            let exit =
                self.container.exit_code().await.unwrap_or_else(|error| {
                    panic!("inspecting exit state of {}: {error}", self.name)
                });
            if let Some(code) = exit {
                return code;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let logs = self.logs().await;
        panic!(
            "timed out waiting for {} to exit\ncontainer logs:\n{logs}",
            self.name
        );
    }

    async fn logs(&self) -> String {
        let stdout = self
            .container
            .stdout_to_vec()
            .await
            .unwrap_or_else(|error| panic!("reading stdout logs of {}: {error}", self.name));
        let stderr = self
            .container
            .stderr_to_vec()
            .await
            .unwrap_or_else(|error| panic!("reading stderr logs of {}: {error}", self.name));
        format!(
            "{}{}",
            String::from_utf8_lossy(&stdout),
            String::from_utf8_lossy(&stderr)
        )
    }

    async fn bridge_ip(&self) -> Ipv4Addr {
        match self.container.get_bridge_ip_address().await {
            Ok(IpAddr::V4(ip)) => ip,
            Ok(IpAddr::V6(ip)) => panic!(
                "container {} has IPv6 bridge address {ip}, expected IPv4",
                self.name
            ),
            Err(error) => panic!("inspecting bridge IP for {}: {error}", self.name),
        }
    }

    /// Stops the daemon and settles once its container has exited.
    ///
    /// The request runs under a client timeout of the Docker client
    /// testcontainers builds, two minutes that nothing here can configure,
    /// and an engine that answers late is not the same fact as a container
    /// that would not stop: the engine goes on stopping it after the client
    /// has given up waiting for the answer. The exit is the fact this waits
    /// on, so an unanswered request is carried into that wait instead of
    /// failing here. A container that really does not stop still fails, in
    /// [`Daemon::wait_for_exit`], with its logs attached.
    async fn stop(&self) {
        if let Err(error) = self
            .container
            .stop_with_timeout(Some(stop_grace_secs()))
            .await
        {
            eprintln!(
                "the stop request for {} went unanswered ({error}); waiting for its container to exit",
                self.name
            );
        }
        self.wait_for_exit().await;
    }

    /// Populates this daemon's node cache from the fixture repository.
    ///
    /// A fresh container has `repositories.json5` mounted in but no cache, and
    /// the cache is the only thing a launcher's `name:tag` sources resolve
    /// against. Skipping this fails a launch in preflight with "not found in
    /// `$PEPPY_HOME/cache/nodes.json5`" before any stack is touched.
    async fn refresh_repos(&self) {
        require_success(
            self.peppy(&["repo", "refresh"]).await,
            &format!("refreshing repositories in {}", self.name),
        );
    }

    /// Blocks until this daemon answers a request at all.
    ///
    /// `stack list` is the cheapest thing every daemon serves, and it is
    /// already how the restart test waits for a new generation to come up.
    async fn wait_until_serving(&self) {
        self.wait_for_stack(|_| true).await;
    }

    /// Restarts the daemon process by cycling its container.
    ///
    /// The container's command IS `peppy service serve`, so this is the whole
    /// daemon generation going away and a new one coming back: exactly what a
    /// coordinator-restart test needs, and the only form of restart available
    /// here. `peppy service stop` / `install` drive a systemd unit that this
    /// image does not run.
    async fn restart(&self) {
        self.stop().await;
        self.container
            .start()
            .await
            .unwrap_or_else(|error| panic!("restarting {}: {error}", self.name));
    }
}

async fn start_daemon(
    launch: &DaemonLaunch<'_>,
    name: &str,
    hostname: &str,
    config: &str,
    managed_router: Option<ManagedRouterMount<'_>>,
    // Extra read-only mount, used by the federated tests to make the
    // documented launcher openable inside the coordinator.
    extra_mount: Option<(&Path, &str)>,
) -> Daemon {
    let repositories_config = launch.fixture.repositories_config();
    let mut request = GenericImage::new(launch.image_name, launch.image_tag)
        .with_container_name(name)
        .with_hostname(hostname)
        // Apptainer builds and runs every container node through a user
        // namespace and a pile of mounts. Under Docker's default profile that
        // is blocked twice over: no CAP_SYS_ADMIN, and `docker-default`
        // AppArmor denies unprivileged userns on Ubuntu 24.04+ (the same
        // restriction `containers::apptainer` disables in peppy's Lima guest).
        // A test-only container on a self-hosted runner is the one place where
        // buying both with `privileged` is the proportionate answer; the
        // alternative is three security-opt knobs that each drift with the
        // host's kernel and AppArmor configuration.
        .with_privileged(true)
        .with_host("host.docker.internal", Host::HostGateway)
        .with_mount(read_only_bind(launch.peppy_binary, CONTAINER_PEPPY_BINARY))
        .with_mount(read_only_bind(launch.apptainer_dir, "/opt/peppy-apptainer"))
        .with_mount(read_only_bind(launch.newuidmap, "/usr/local/bin/newuidmap"))
        // Every daemon, not only the ones that refresh: mounting the config in
        // ahead of startup is what keeps the bundled defaults from ever being
        // written, so no daemon in this suite has a network repository to read.
        .with_mount(read_only_bind(
            launch.fixture.repo.path(),
            CONTAINER_FIXTURE_REPO,
        ))
        .with_mount(read_only_bind(
            &repositories_config,
            &format!("{CONTAINER_PEPPY_HOME}/conf/repositories.json5"),
        ))
        .with_env_var(PEPPY_HOME_ENV, CONTAINER_PEPPY_HOME)
        .with_env_var("PEPPY_APPTAINER_DIR", "/opt/peppy-apptainer")
        .with_env_var(PEPPY_CONFIG_ENV, config)
        .with_cmd([CONTAINER_PEPPY_BINARY, "service", "serve"]);
    if let Some((host_path, container_path)) = extra_mount {
        request = request.with_mount(read_only_bind(host_path, container_path));
    }
    if let Some(router) = managed_router {
        request = request
            .with_mount(read_only_bind(
                router.zenohd_binary,
                "/usr/local/bin/zenohd",
            ))
            .with_mount(read_only_bind(router.config, CONTAINER_ROUTER_CONFIG))
            .with_env_var("ZENOH_CONFIG", CONTAINER_ROUTER_CONFIG);
    }
    let container = request
        .start()
        .await
        .unwrap_or_else(|error| panic!("starting container {name} failed: {error}"));
    Daemon {
        name: name.to_string(),
        container,
    }
}

/// External mode is the shared-router architecture: both container daemons dial
/// one operator-run host router, and peppy owns none of its router lifecycle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_container_daemons_are_enumerated_and_collisions_are_refused() {
    let substrate = Substrate::create().await;

    let daemon_a = substrate
        .start_daemon("peppy-md", "a", "robo-a", "daemon-a")
        .await;
    let daemon_b = substrate
        .start_daemon("peppy-md", "b", "robo-b", "daemon-b")
        .await;

    let both = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-a)")
                && text.contains("Core node: daemon-b (host: robo-b)")
        })
        .await;
    let local_position = both.find("Core node: daemon-a").expect("local section");
    let remote_position = both.find("Core node: daemon-b").expect("remote section");
    assert!(
        local_position < remote_position,
        "local section must be first:\n{both}"
    );
    let local_section = &both[local_position..remote_position];
    let remote_section = &both[remote_position..];
    assert!(
        local_section.contains("daemon-a:"),
        "daemon-a root row missing:\n{both}"
    );
    assert!(
        remote_section.contains("daemon-b:"),
        "daemon-b root row missing:\n{both}"
    );

    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon-b from daemon-a",
    );
    assert!(targeted.contains("Core node: daemon-b (host: robo-b)"));
    assert!(
        !targeted.contains("Core node: daemon-a"),
        "explicit targeting must render one section:\n{targeted}"
    );

    let collision = substrate
        .start_daemon("peppy-md", "c", "robo-c", "daemon-a")
        .await;
    let collision_status = collision.wait_for_exit().await;
    assert_ne!(collision_status, 0, "colliding daemon must fail startup");
    let collision_logs = collision.logs().await;
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );

    daemon_b.stop().await;
    let only_a = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-a)")
                && !text.contains("Core node: daemon-b")
        })
        .await;
    assert!(
        !only_a.contains("daemon-b"),
        "stopped daemon presence must disappear:\n{only_a}"
    );
}

/// The multi-machine architecture: every "machine" (container) runs a genuinely
/// managed router and keeps the default peer topology. Operator-pinned
/// `ZENOH_CONFIG` files link those routers directly: B dials A, while the
/// collision daemon dials both A and B so its name claim does not depend on
/// multi-hop relay.
///
/// The federation task still boots armed, but this test has no backend and its
/// one-second resolution attempt fails before the daemon proceeds standalone.
/// The static cross-machine links belong to zenohd itself, not that task, and a
/// pinned config survives peppy's router watchdog restarts unchanged.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn federated_router_peer_topology_daemons_are_enumerated_and_collisions_are_refused() {
    require_docker().await;
    let (image_name, image_tag) = e2e_image().await;

    let peppy_binary = Path::new(env!("CARGO_BIN_EXE_peppy"));
    let zenohd_binary = bundled_zenohd_binary();
    let apptainer_dir = containers::Apptainer::resolve_apptainer_dir()
        .expect("the test-built daemon should have a host Apptainer installation");
    let newuidmap = executable_on_path("newuidmap");
    let fixture = FixtureRepository::create();
    let launch = DaemonLaunch {
        image_name: &image_name,
        image_tag: &image_tag,
        peppy_binary,
        apptainer_dir: &apptainer_dir,
        newuidmap: &newuidmap,
        fixture: &fixture,
    };
    let suffix = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after epoch")
            .as_millis()
    );

    let router_pins = tempfile::tempdir().expect("create pinned router config directory");
    let router_a_pin = router_pins.path().join("router-a.json5");
    write_router_pin(&router_a_pin, Vec::new());

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-fed-a-{suffix}"),
        "robo-fed-a",
        &managed_daemon_config("daemon-a"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &router_a_pin,
        }),
        None,
    )
    .await;
    let daemon_a_ip = daemon_a.bridge_ip().await;

    let router_b_pin = router_pins.path().join("router-b.json5");
    write_router_pin(
        &router_b_pin,
        vec![format!("tcp/{daemon_a_ip}:{MANAGED_ROUTER_PORT}")],
    );
    let daemon_b = start_daemon(
        &launch,
        &format!("peppy-fed-b-{suffix}"),
        "robo-fed-b",
        &managed_daemon_config("daemon-b"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &router_b_pin,
        }),
        None,
    )
    .await;
    let daemon_b_ip = daemon_b.bridge_ip().await;

    // Cross-visibility must hold from BOTH sides: a resolves b through
    // router A ← router B, and b resolves a through the same link dialed the
    // other way.
    let from_a = daemon_a
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;
    assert!(
        from_a.find("Core node: daemon-a").expect("local section")
            < from_a.find("Core node: daemon-b").expect("remote section"),
        "local section must be first:\n{from_a}"
    );
    daemon_b
        .wait_for_stack(|text| {
            text.contains("Core node: daemon-a (host: robo-fed-a)")
                && text.contains("Core node: daemon-b (host: robo-fed-b)")
        })
        .await;

    // The stack_list service itself must answer across the federation link,
    // not just the presence enumeration.
    let targeted = require_success(
        daemon_a.stack_list(Some("daemon-b")).await,
        "targeting daemon-b across the router federation",
    );
    assert!(targeted.contains("Core node: daemon-b (host: robo-fed-b)"));
    assert!(
        !targeted.contains("Core node: daemon-a"),
        "explicit targeting must render one section:\n{targeted}"
    );

    // Name claims must be enforced across the federation. C dials both live
    // routers explicitly, avoiding any dependence on multi-hop router relay.
    let collision_pin = router_pins.path().join("router-c.json5");
    write_router_pin(
        &collision_pin,
        vec![
            format!("tcp/{daemon_a_ip}:{MANAGED_ROUTER_PORT}"),
            format!("tcp/{daemon_b_ip}:{MANAGED_ROUTER_PORT}"),
        ],
    );
    let collision = start_daemon(
        &launch,
        &format!("peppy-fed-c-{suffix}"),
        "robo-fed-c",
        &managed_daemon_config("daemon-a"),
        Some(ManagedRouterMount {
            zenohd_binary: &zenohd_binary,
            config: &collision_pin,
        }),
        None,
    )
    .await;
    let collision_status = collision.wait_for_exit().await;
    assert_ne!(
        collision_status, 0,
        "colliding daemon must fail startup across the federation"
    );
    let collision_logs = collision.logs().await;
    assert!(
        collision_logs.contains("core node name 'daemon-a' is already in use"),
        "collision error missing:\n{collision_logs}"
    );
}

// ── Federated launch ──────────────────────────────────────────────────────
//
// Separate tests rather than one long one. The fixtures genuinely differ (a
// coordinator restart, a killed peer, and `--local` with no peer at all are
// three different worlds), and a single sequential test would hide every
// assertion after the first failure behind one stage number.
//
// Everything asserted here is read from stack state or from the launch's own
// ordered feedback. Nothing compares timestamps across containers: the two
// containers do not share a clock, and a test that leaned on that would be
// exactly as flaky as the host it ran on.

/// The launcher the `Federation` guide documents, driven straight from the docs
/// tree. Testing the guide's own file is the point: a launcher that only this
/// test can run would prove nothing about the documented one. Every node it
/// deploys is published by the fixture repository under the name and tag the
/// file states, which is what keeps it runnable unmodified.
const SPLIT_COMPUTE_LAUNCHER: &str =
    "docs/src/content/docs/guides/snippets/launchers/split_compute_manipulation.json5";

/// Where the federated tests' launchers are mounted inside a container. A
/// directory rather than a single file, so a test can pick which launcher it
/// drives without changing what any daemon mounts.
const CONTAINER_LAUNCHER_DIR: &str = "/etc/peppy/launchers";
const SPLIT_COMPUTE_LAUNCHER_FILE: &str = "split_compute_manipulation.json5";
const NODE_PROBE_LAUNCHER_FILE: &str = "node_probe.json5";
const CALLER_ENV_PROBE_LAUNCHER_FILE: &str = "caller_env_probe.json5";
const PEER_RUN_FAILURE_LAUNCHER_FILE: &str = "peer_run_failure.json5";
const FLEET_CLOCK_LAUNCHER_FILE: &str = "fleet_clock.json5";
const FULLY_PLACED_CLOCK_LAUNCHER_FILE: &str = "fully_placed_clock.json5";
const UNKNOWN_CLOCK_LAUNCHER_FILE: &str = "unknown_clock_fleet.json5";

fn container_launcher(file_name: &str) -> String {
    format!("{CONTAINER_LAUNCHER_DIR}/{file_name}")
}

/// One node per execution path the documented launcher needs:
/// `uvc_camera_python_mock` is a container node (apptainer builds a SIF) and
/// `my_python_robot_arm` is a native one (`uv` builds a venv). Written here
/// rather than kept in the docs snippets because it documents nothing; it only
/// proves the machine works.
const NODE_PROBE_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [
    {
      source: { name: "uvc_camera_python_mock", tag: "v1" },
      instances: [{ instance_id: "probe_cam_inst" }],
    },
    {
      source: { name: "my_python_robot_arm", tag: "v1" },
      instances: [{ instance_id: "probe_arm_inst" }],
    },
  ],
}
"#;

const NAMED_FLEET_LAUNCHER_FILE: &str = "named_fleet.json5";
const GROWING_GATES_LAUNCHER_FILE: &str = "growing_gates.json5";
const SET_WATCH_LAUNCHER_FILE: &str = "set_watch_fleet.json5";
const COPIES_ONLY_LAUNCHER_FILE: &str = "copies_only.json5";
const ISOLATION_FLEET_LAUNCHER_FILE: &str = "isolation_fleet.json5";
const MISSING_PUBLISHER_CLOCK_LAUNCHER_FILE: &str = "missing_publisher_clock_fleet.json5";
const NAMED_CLOCK_LAUNCHER_FILE: &str = "named_clock_fleet.json5";
const NAMED_FLEET_TWO_NODES_LAUNCHER_FILE: &str = "named_fleet_two_nodes.json5";
const NAMED_FLEET_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [{ source: { name: "my_python_robot_arm", tag: "v1" },
    instances: [{ instance_id: "shared_inst" }] }],
  components: [{ name: "robot", cardinality: "zero_or_more", options: {
    arm: { deployments: [{ source: { name: "my_python_robot_arm", tag: "v1" },
      instances: [{ instance_id: "arm_inst" }] }] }
  } }]
}"#;

/// The named fleet's robot axis alone: a launch that starts nothing and
/// takes copies.
const COPIES_ONLY_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [],
  components: [{ name: "robot", cardinality: "zero_or_more", options: {
    arm: { deployments: [{ source: { name: "my_python_robot_arm", tag: "v1" },
      instances: [{ instance_id: "arm_inst" }] }] }
  } }]
}"#;

const PAIRING_FLEET_LAUNCHER_FILE: &str = "pairing_fleet.json5";
/// One hub holding a pair per leader through a `zero_or_more` slot, and a
/// leader axis whose copies pair into it from whichever machine they are
/// placed on. Every leader needs a value of its own, which the join sets.
const PAIRING_FLEET_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [{ source: { name: "pairing_hub", tag: "v1" },
    instances: [{ instance_id: "hub_inst" }] }],
  components: [{ name: "leader", cardinality: "zero_or_more", options: {
    limb: { deployments: [{ source: { name: "pairing_leader", tag: "v1" },
      instances: [{ instance_id: "leader_inst", links: { hub: "hub_inst" } }] }] }
  } }]
}"#;

/// A launch that starts one idle gate per daemon, deploys the copy the test
/// launches, and takes the rest as joins:
/// one arm serving a heartbeat topic, echo and release services and a move
/// action, and one commander wired to that arm alone. Each side logs the
/// instance behind every message it handles, so traffic from another copy
/// would show up by name. Rendered from the same constants the test drives
/// the gates with, so the gate a round moves on and the gate the launch
/// starts cannot diverge.
fn isolation_fleet_launcher() -> String {
    let launched_label = Station::new(LAUNCHED_STATION, Placement::Coordinator).label();
    let launched = LAUNCHED_STATION;
    format!(
        r#"{{
  peppy_schema: "launcher/v1",
  core_nodes: ["{PEER_LABEL}"],
  deployments: [
    {{ source: {{ name: "{GATE_NODE}", tag: "{GATE_TAG}" }},
      instances: [
        {{ instance_id: "{COORDINATOR_GATE}", arguments: {{ verb: "release" }} }},
        {{ instance_id: "{PEER_GATE}", core_node: "{PEER_LABEL}", arguments: {{ verb: "release" }} }}
      ] }},
    {{ robot: "station", instances: [
      {{ instance_id: "{launched}", arguments: {{ commander_inst: {{ label: "{launched_label}" }} }} }}
    ] }}
  ],
  components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
    station: {{ deployments: [
      {{ source: {{ name: "isolation_arm", tag: "v1" }},
        instances: [{{ instance_id: "arm_inst" }}] }},
      {{ source: {{ name: "isolation_commander", tag: "v1" }},
        instances: [{{ instance_id: "commander_inst", links: {{ arm: "arm_inst" }} }}] }}
    ] }}
  }} }}]
}}"#
    )
}

/// The named fleet with a second robot option deploying a second node, so a
/// copy can bring a node a peer does not hold yet.
const NAMED_FLEET_TWO_NODES_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  deployments: [{ source: { name: "my_python_robot_arm", tag: "v1" },
    instances: [{ instance_id: "shared_inst" }] }],
  components: [{ name: "robot", cardinality: "zero_or_more", options: {
    arm: { deployments: [{ source: { name: "my_python_robot_arm", tag: "v1" },
      instances: [{ instance_id: "arm_inst" }] }] },
    cam: { deployments: [{ source: { name: "uvc_camera_python_mock", tag: "v1" },
      instances: [{ instance_id: "cam_inst" }] }] }
  } }]
}"#;

/// One native Python node placed wholly on the peer. The launcher driven when
/// what matters is WHERE a build resolves its tools, not the topology: the
/// peer must build `my_python_robot_arm` with its own environment, whatever
/// the caller's looks like.
const CALLER_ENV_PROBE_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  core_nodes: ["remote_worker"],
  deployments: [
    {
      source: { name: "my_python_robot_arm", tag: "v1" },
      instances: [{ instance_id: "env_probe_arm_inst", core_node: "remote_worker" }],
    },
  ],
}
"#;

/// The instance the launcher places on the peer, and whose working directory
/// the test makes unusable there.
const PEER_RUN_FAILURE_INSTANCE: &str = "peer_fail_recon_inst";

/// A launch that gets as far as the peer's RUN and fails there.
///
/// The peer adds its node, builds it, accepts the run goal, opens its log file,
/// and then cannot materialize [`PEER_RUN_FAILURE_INSTANCE`]'s working
/// directory, which the test has made uncreatable on the peer alone (see
/// `a_peer_phase_that_fails_still_names_the_peers_log_file`). The camera stays
/// on the coordinator, which is what makes the marked entry evidence of
/// attribution rather than of every entry belonging to one machine.
const PEER_RUN_FAILURE_LAUNCHER: &str = r#"{
  peppy_schema: "launcher/v1",
  core_nodes: ["remote_worker"],
  deployments: [
    {
      source: { name: "uvc_camera_python_mock", tag: "v1" },
      instances: [{ instance_id: "peer_fail_cam_inst" }],
    },
    {
      source: { name: "uvc_camera_video_reconstruction_python", tag: "v1" },
      instances: [
        {
          instance_id: "peer_fail_recon_inst",
          core_node: "remote_worker",
          arguments: { video_duration_seconds: 5 },
          links: { camera: "peer_fail_cam_inst" },
        },
      ],
    },
  ],
}
"#;

/// The ticket-shaped fleet: a launch spanning the simulator machine and two
/// more. The launcher declares one clock domain and names the scripted source
/// its publisher, which is what assigns that instance the domain; every
/// machine, the coordinator included, runs a probe bound to it. One more probe
/// binds nothing and reads its machine's own clock, so the launch puts a
/// simulated domain and wall time on one daemon. Rendered from the same
/// constants the test asserts with, so the asserted ramp and the launched ramp
/// cannot diverge.
fn fleet_clock_launcher() -> String {
    format!(
        r#"{{
  peppy_schema: "launcher/v1",
  core_nodes: ["station_a", "station_b"],
  framework: {{
    clocks: {{ {FLEET_CLOCK_DOMAIN}: {{ publisher: "{FLEET_CLOCK_SOURCE_INSTANCE}" }} }},
  }},
  deployments: [
    {{
      source: {{ name: "sim_clock_source", tag: "v1" }},
      instances: [
        {{
          instance_id: "{FLEET_CLOCK_SOURCE_INSTANCE}",
          arguments: {{
            tick_step_ns: {SCRIPTED_TICK_STEP_NS},
            tick_count: {SCRIPTED_TICK_COUNT},
            tick_interval_ms: 50,
          }},
        }},
      ],
    }},
    {{
      source: {{ name: "sim_clock_probe", tag: "v1" }},
      instances: [
        {{ instance_id: "{FLEET_PROBE_COORD_INSTANCE}",
          framework: {{ clock: "{FLEET_CLOCK_DOMAIN}" }},
          arguments: {{ poll_interval_ms: 50 }} }},
        {{ instance_id: "{FLEET_PROBE_STATION_A_INSTANCE}", core_node: "station_a",
          framework: {{ clock: "{FLEET_CLOCK_DOMAIN}" }},
          arguments: {{ poll_interval_ms: 50 }} }},
        {{ instance_id: "{FLEET_PROBE_STATION_B_INSTANCE}", core_node: "station_b",
          framework: {{ clock: "{FLEET_CLOCK_DOMAIN}" }},
          arguments: {{ poll_interval_ms: 50 }} }},
        {{ instance_id: "{FLEET_PROBE_WALL_INSTANCE}", core_node: "station_b",
          arguments: {{ poll_interval_ms: 50 }} }},
      ],
    }},
  ],
}}
"#
    )
}

/// The fleet's domain and its publisher alone, with the probes taken as
/// copies: what a stack that is joined rather than launched into looks like.
fn named_clock_launcher() -> String {
    let mut launcher: serde_json::Value = serde_json5::from_str(&fleet_clock_launcher()).unwrap();
    launcher.as_object_mut().unwrap().remove("core_nodes");
    launcher["deployments"].as_array_mut().unwrap().truncate(1);
    launcher["components"] = serde_json::json!([{
        "name": "robot", "cardinality": "zero_or_more", "options": {
            "probe": { "deployments": [{ "source": { "name": "sim_clock_probe", "tag": "v1" },
                "instances": [{ "instance_id": "probe_inst",
                    "framework": { "clock": FLEET_CLOCK_DOMAIN },
                    "arguments": { "poll_interval_ms": 50 } }]
            }] }
        }
    }]);
    serde_json::to_string(&launcher).unwrap()
}

/// The named clock fleet with its publisher taken away: a launcher declaring a
/// domain supplied by an instance no deployment carries.
fn missing_publisher_clock_launcher() -> String {
    let mut launcher: serde_json::Value = serde_json5::from_str(&named_clock_launcher()).unwrap();
    // Swap the publisher's deployment for a probe's. The launch still starts
    // an instance, so the declaration naming an absent publisher is the only
    // thing left to refuse.
    launcher["deployments"] = serde_json::json!([{
        "source": { "name": "sim_clock_probe", "tag": "v1" },
        "instances": [{
            "instance_id": FLEET_PROBE_COORD_INSTANCE,
            "arguments": { "poll_interval_ms": 50 }
        }]
    }]);
    serde_json::to_string(&launcher).unwrap()
}

/// The fleet launcher with its coordinator probe bound to a domain the
/// document never declares. Coherent in every other respect, so the binding is
/// the only thing left to refuse.
fn unknown_clock_launcher() -> String {
    let mut launcher: serde_json::Value = serde_json5::from_str(&fleet_clock_launcher()).unwrap();
    launcher["deployments"][1]["instances"][0]["framework"]["clock"] =
        serde_json::json!(UNDECLARED_CLOCK_DOMAIN);
    serde_json::to_string(&launcher).unwrap()
}

/// The fully-placed sibling of [`fleet_clock_launcher`]: the same domain and
/// probes, but everything lives on the stations and nothing on the
/// coordinator, so the machine the launch is typed from is a bystander.
fn fully_placed_clock_launcher() -> String {
    format!(
        r#"{{
  peppy_schema: "launcher/v1",
  core_nodes: ["station_a", "station_b"],
  framework: {{
    clocks: {{ {FLEET_CLOCK_DOMAIN}: {{ publisher: "{FLEET_CLOCK_SOURCE_INSTANCE}" }} }},
  }},
  deployments: [
    {{
      source: {{ name: "sim_clock_source", tag: "v1" }},
      instances: [
        {{
          instance_id: "{FLEET_CLOCK_SOURCE_INSTANCE}",
          core_node: "station_a",
          arguments: {{
            tick_step_ns: {SCRIPTED_TICK_STEP_NS},
            tick_count: {SCRIPTED_TICK_COUNT},
            tick_interval_ms: 50,
          }},
        }},
      ],
    }},
    {{
      source: {{ name: "sim_clock_probe", tag: "v1" }},
      instances: [
        {{ instance_id: "{FLEET_PROBE_STATION_A_INSTANCE}", core_node: "station_a",
          framework: {{ clock: "{FLEET_CLOCK_DOMAIN}" }},
          arguments: {{ poll_interval_ms: 50 }} }},
        {{ instance_id: "{FLEET_PROBE_STATION_B_INSTANCE}", core_node: "station_b",
          framework: {{ clock: "{FLEET_CLOCK_DOMAIN}" }},
          arguments: {{ poll_interval_ms: 50 }} }},
      ],
    }},
  ],
}}
"#
    )
}

/// The domain the clock launchers declare, and the one every bound probe
/// reads.
const FLEET_CLOCK_DOMAIN: &str = "fleet";

/// A domain no launcher here declares, bound by one probe of an otherwise
/// coherent fleet.
const UNDECLARED_CLOCK_DOMAIN: &str = "nonexistent";

const FLEET_CLOCK_SOURCE_INSTANCE: &str = "clock_source_inst";
const FLEET_PROBE_COORD_INSTANCE: &str = "probe_coord_inst";
const FLEET_PROBE_STATION_A_INSTANCE: &str = "probe_station_a_inst";
const FLEET_PROBE_STATION_B_INSTANCE: &str = "probe_station_b_inst";

/// The probe that binds no domain, beside a bound one on the same machine.
const FLEET_PROBE_WALL_INSTANCE: &str = "probe_wall_inst";

/// Every instance the clock launchers deploy, for the refusals that must leave
/// each machine holding none of them.
const FLEET_INSTANCES: [&str; 5] = [
    FLEET_CLOCK_SOURCE_INSTANCE,
    FLEET_PROBE_COORD_INSTANCE,
    FLEET_PROBE_STATION_A_INSTANCE,
    FLEET_PROBE_STATION_B_INSTANCE,
    FLEET_PROBE_WALL_INSTANCE,
];

/// What the scripted source logs once it holds its domain's publisher.
const CLOCK_SOURCE_PUBLISHING_MARKER: &str = "[clock-source] publishing clock";

/// The ramp [`fleet_clock_launcher`] scripts: ticks `STEP`, `2 * STEP`, ...,
/// `COUNT * STEP`, then the final tick republished at the same cadence, so a
/// probe whose subscription came up after the ramp still converges on it.
const SCRIPTED_TICK_STEP_NS: u64 = 100_000_000;
const SCRIPTED_TICK_COUNT: u64 = 50;
const SCRIPTED_FINAL_NS: u64 = SCRIPTED_TICK_STEP_NS * SCRIPTED_TICK_COUNT;

/// Instances the launcher places on `robot_onboard`, i.e. everything the
/// control loop touches.
const ROBOT_INSTANCES: [&str; 3] = ["wrist_cam_inst", "arm_inst", "reflex_inst"];

/// Instances the launcher places on `cloud_inference`.
const CLOUD_INSTANCES: [&str; 2] = ["planner_inst", "recorder_inst"];

impl Daemon {
    /// The current contents of `instance_id`'s run log on this daemon.
    ///
    /// Reads `$PEPPY_HOME/logs/run/<instance>.log`, the node's own output,
    /// which is the evidence that a node is carrying messages over its slots
    /// and not merely Running.
    async fn node_log(&self, instance_id: &str) -> String {
        let path = format!("{CONTAINER_PEPPY_HOME}/logs/run/{instance_id}.log");
        // `cat`'s own complaint rides along, so a log this daemon does not
        // hold reads as the missing file it is.
        self.exec(vec!["cat", path.as_str()]).await.text
    }

    /// Polls `instance_id`'s log until it holds `marker`, returning the log.
    ///
    /// Polling every 250 ms under the same `TIMEOUT` as every other wait here
    /// keeps the wait independent of how fast the host is.
    async fn find_node_log(&self, instance_id: &str, marker: &str) -> Option<String> {
        let started = Instant::now();
        while started.elapsed() < TIMEOUT {
            let log = self.node_log(instance_id).await;
            if log.contains(marker) {
                return Some(log);
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        None
    }

    /// [`Self::find_node_log`], with the log in the panic when it times out.
    async fn wait_for_node_log(&self, instance_id: &str, marker: &str) -> String {
        match self.find_node_log(instance_id, marker).await {
            Some(log) => log,
            None => panic!(
                "timed out waiting for `{marker}` in {}'s `{instance_id}` log; last contents:\n{}",
                self.name,
                self.node_log(instance_id).await
            ),
        }
    }
}

/// Asserts that `earlier` appears before `later` in the launch's feedback.
///
/// This is how cross-boundary ordering is checked. The coordinator's feedback
/// stream is ordered by construction, so the INDEX of each "Starting ..." line
/// is a deterministic record of what started when. Comparing wall-clock times
/// from two containers would be neither.
fn assert_starts_before(feedback: &str, earlier: &str, later: &str) {
    let earlier_at = feedback
        .find(&format!("instance {earlier}"))
        .unwrap_or_else(|| panic!("`{earlier}` never started; launch output:\n{feedback}"));
    let later_at = feedback
        .find(&format!("instance {later}"))
        .unwrap_or_else(|| panic!("`{later}` never started; launch output:\n{feedback}"));
    assert!(
        earlier_at < later_at,
        "`{earlier}` must start before `{later}` (it is bound as its producer), \
         but the launch started them the other way round:\n{feedback}"
    );
}

/// Whether `stack` lists `instance_id` as one of its own instances.
///
/// Substring matching cannot answer this, and gets it backwards in exactly the
/// case that matters. A daemon's rendering names the instances it holds AND the
/// remote ones they are wired to: the planner's row on `cn-cloud` reads
/// `scene → wrist_cam_inst@cn-robot`, so `contains("wrist_cam_inst")` reports
/// the camera as held by the very daemon the launcher placed it away from. That
/// reference is the federation working, not a placement error.
///
/// An instance a daemon holds fills a table cell by itself. A reference to one
/// it does not hold is always qualified (`<instance>@<core-node>`) and sits
/// inside a larger cell alongside the slot it feeds. So the question is cell
/// equality, not containment.
fn holds_instance(stack: &str, instance_id: &str) -> bool {
    stack
        .split(['│', '\n'])
        .any(|cell| cell.trim() == instance_id)
}

/// The coordinator's section of a `stack list --json` report.
fn coordinator_section(listed: &str, core_node: &str) -> serde_json::Value {
    let document = listed
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("missing stack JSON: {listed}"));
    let report: serde_json::Value = serde_json::from_str(document).unwrap();
    report["core_nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|section| section["core_node"] == core_node)
        .unwrap_or_else(|| panic!("no section for `{core_node}`: {listed}"))
        .clone()
}

/// Every node one daemon's slice holds, keyed `name:tag`, with its stage and
/// the number of instances it tracks, read from a `stack list --json` report.
fn slice_nodes(listed: &str, core_node: &str) -> BTreeMap<String, (String, usize)> {
    coordinator_section(listed, core_node)["stack"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|node| node["stage"] != "Root")
        .map(|node| {
            (
                format!(
                    "{}:{}",
                    node["name"].as_str().unwrap(),
                    node["tag"].as_str().unwrap()
                ),
                (
                    node["stage"].as_str().unwrap().to_owned(),
                    node["instances"].as_array().unwrap().len(),
                ),
            )
        })
        .collect()
}

/// The names of the copies a `stack list --json` section lists.
fn copy_names(section: &serde_json::Value) -> Vec<String> {
    section["copies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|copy| copy["name"].as_str().unwrap().to_owned())
        .collect()
}

fn assert_holds_exactly(stack: &str, daemon: &str, expected: &[&str], forbidden: &[&str]) {
    for instance in expected {
        assert!(
            holds_instance(stack, instance),
            "`{daemon}` must hold `{instance}`; stack list was:\n{stack}"
        );
    }
    for instance in forbidden {
        assert!(
            !holds_instance(stack, instance),
            "`{daemon}` must NOT hold `{instance}`: it is placed on the other daemon. \
             Stack list was:\n{stack}"
        );
    }
}

/// What every set of daemon containers stands on: the built image, the shared
/// host router, the fixture repository, and the launcher mount. One per test;
/// every daemon of that test borrows from it, and it is held for their
/// lifetime because the containers read the mounted directories off the host
/// while they run.
struct Substrate {
    image_name: String,
    image_tag: String,
    router: pmi::ZenohdInstance,
    apptainer_dir: PathBuf,
    newuidmap: PathBuf,
    fixture: FixtureRepository,
    launcher_dir: tempfile::TempDir,
    /// Uniquifies container names across concurrent tests in one run.
    suffix: String,
}

impl Substrate {
    async fn create() -> Self {
        require_docker().await;
        let (image_name, image_tag) = e2e_image().await;

        let router = ZenohAdapter::start_router_ephemeral_in_mode(
            "0.0.0.0",
            None,
            false,
            pmi::SubscriberBufferSizes::default(),
            None,
        )
        .await
        .expect("host zenohd should start");

        let apptainer_dir = containers::Apptainer::resolve_apptainer_dir()
            .expect("the test-built daemon should have a host Apptainer installation");
        let newuidmap = executable_on_path("newuidmap");
        let fixture = FixtureRepository::create();
        let suffix = format!(
            "{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock should be after epoch")
                .as_millis()
        );

        // Every launcher these tests drive, in one mountable directory: the
        // file the guide documents, plus the probe launchers beside it.
        let launcher_dir = tempfile::tempdir().expect("create launcher mount directory");
        let documented_launcher = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(SPLIT_COMPUTE_LAUNCHER);
        std::fs::copy(
            &documented_launcher,
            launcher_dir.path().join(SPLIT_COMPUTE_LAUNCHER_FILE),
        )
        .unwrap_or_else(|error| {
            panic!(
                "copying {} into the launcher mount: {error}",
                documented_launcher.display()
            )
        });
        for (file_name, launcher) in [
            (GROWING_GATES_LAUNCHER_FILE, growing_gates_launcher()),
            (SET_WATCH_LAUNCHER_FILE, set_watch_launcher()),
            (NAMED_FLEET_LAUNCHER_FILE, NAMED_FLEET_LAUNCHER.to_owned()),
            (COPIES_ONLY_LAUNCHER_FILE, COPIES_ONLY_LAUNCHER.to_owned()),
            (
                PAIRING_FLEET_LAUNCHER_FILE,
                PAIRING_FLEET_LAUNCHER.to_owned(),
            ),
            (ISOLATION_FLEET_LAUNCHER_FILE, isolation_fleet_launcher()),
            (
                MISSING_PUBLISHER_CLOCK_LAUNCHER_FILE,
                missing_publisher_clock_launcher(),
            ),
            (
                NAMED_FLEET_TWO_NODES_LAUNCHER_FILE,
                NAMED_FLEET_TWO_NODES_LAUNCHER.to_owned(),
            ),
            (NAMED_CLOCK_LAUNCHER_FILE, named_clock_launcher()),
            (NODE_PROBE_LAUNCHER_FILE, NODE_PROBE_LAUNCHER.to_owned()),
            (
                CALLER_ENV_PROBE_LAUNCHER_FILE,
                CALLER_ENV_PROBE_LAUNCHER.to_owned(),
            ),
            (
                PEER_RUN_FAILURE_LAUNCHER_FILE,
                PEER_RUN_FAILURE_LAUNCHER.to_owned(),
            ),
            (FLEET_CLOCK_LAUNCHER_FILE, fleet_clock_launcher()),
            (
                FULLY_PLACED_CLOCK_LAUNCHER_FILE,
                fully_placed_clock_launcher(),
            ),
            (UNKNOWN_CLOCK_LAUNCHER_FILE, unknown_clock_launcher()),
        ] {
            std::fs::write(launcher_dir.path().join(file_name), launcher).unwrap_or_else(|error| {
                panic!("writing {file_name} into the launcher mount: {error}")
            });
        }

        Self {
            image_name,
            image_tag,
            router,
            apptainer_dir,
            newuidmap,
            fixture,
            launcher_dir,
            suffix,
        }
    }

    /// Starts one daemon container against this substrate's router, launcher
    /// mount, and fixture repository.
    async fn start_daemon(
        &self,
        prefix: &str,
        role: &str,
        hostname: &str,
        core_node: &str,
    ) -> Daemon {
        let launch = DaemonLaunch {
            image_name: &self.image_name,
            image_tag: &self.image_tag,
            peppy_binary: Path::new(env!("CARGO_BIN_EXE_peppy")),
            apptainer_dir: &self.apptainer_dir,
            newuidmap: &self.newuidmap,
            fixture: &self.fixture,
        };
        start_daemon(
            &launch,
            &format!("{prefix}-{role}-{}", self.suffix),
            hostname,
            &external_daemon_config(core_node, self.router.port),
            None,
            Some((self.launcher_dir.path(), CONTAINER_LAUNCHER_DIR)),
        )
        .await
    }

    /// Starts the first spec as the coordinator and the rest as peers, then
    /// runs the one bring-up sequence every multi-daemon test uses: the
    /// coordinator serves and refreshes its repositories, the peers only
    /// serve. Only the coordinator's cache decides what a launch runs: it
    /// resolves every deployment once and ships pinned content to each peer,
    /// which fetches whatever it does not already hold, so the peers
    /// deliberately get NO refresh. Their empty caches are what prove the
    /// pins carry the launch.
    async fn start_coordinated(&self, prefix: &str, specs: &[DaemonSpec<'_>]) -> Vec<Daemon> {
        let mut daemons = Vec::with_capacity(specs.len());
        for spec in specs {
            daemons.push(
                self.start_daemon(prefix, spec.role, spec.hostname, spec.core_node)
                    .await,
            );
        }
        for (index, daemon) in daemons.iter().enumerate() {
            daemon.wait_until_serving().await;
            if index == 0 {
                daemon.refresh_repos().await;
            }
        }
        daemons
    }
}

/// One daemon of a coordinated set: its container-name role, hostname, and
/// wired core-node name.
struct DaemonSpec<'a> {
    role: &'a str,
    hostname: &'a str,
    core_node: &'a str,
}

/// Two daemons on a shared router in one namespace, plus the launcher mounted
/// into the coordinator. The shape every two-machine federated test needs.
struct Federation {
    robot: Daemon,
    cloud: Daemon,
    robot_core_node: String,
    cloud_core_node: String,
    _substrate: Substrate,
}

async fn start_federation(prefix: &str) -> Federation {
    let substrate = Substrate::create().await;
    let mut daemons = substrate
        .start_coordinated(
            prefix,
            &[
                DaemonSpec {
                    role: "robot",
                    hostname: "robo-robot",
                    core_node: "cn-robot",
                },
                DaemonSpec {
                    role: "cloud",
                    hostname: "robo-cloud",
                    core_node: "cn-cloud",
                },
            ],
        )
        .await;
    let cloud = daemons.pop().expect("two daemons started");
    let robot = daemons.pop().expect("two daemons started");

    Federation {
        robot,
        cloud,
        robot_core_node: "cn-robot".to_owned(),
        cloud_core_node: "cn-cloud".to_owned(),
        _substrate: substrate,
    }
}

/// The three machines of the fleet clock tests: the coordinator (the simulator
/// machine) and two stations.
struct Fleet {
    coordinator: Daemon,
    station_a: Daemon,
    station_b: Daemon,
    _substrate: Substrate,
}

const FLEET_COORDINATOR_CORE_NODE: &str = "cn-fleet-coord";
const FLEET_STATION_A_CORE_NODE: &str = "cn-station-a";
const FLEET_STATION_B_CORE_NODE: &str = "cn-station-b";

/// The three machines every clock test runs on, coordinated from the first.
async fn start_fleet(prefix: &str) -> Fleet {
    let substrate = Substrate::create().await;
    let mut daemons = substrate
        .start_coordinated(
            prefix,
            &[
                DaemonSpec {
                    role: "coord",
                    hostname: "robo-fleet-coord",
                    core_node: FLEET_COORDINATOR_CORE_NODE,
                },
                DaemonSpec {
                    role: "station-a",
                    hostname: "robo-station-a",
                    core_node: FLEET_STATION_A_CORE_NODE,
                },
                DaemonSpec {
                    role: "station-b",
                    hostname: "robo-station-b",
                    core_node: FLEET_STATION_B_CORE_NODE,
                },
            ],
        )
        .await;
    let station_b = daemons.pop().expect("three daemons started");
    let station_a = daemons.pop().expect("three daemons started");
    let coordinator = daemons.pop().expect("three daemons started");

    Fleet {
        coordinator,
        station_a,
        station_b,
        _substrate: substrate,
    }
}

impl Federation {
    /// Launches the split-compute launcher from the robot, placing the cloud
    /// half on the peer.
    async fn launch_split(&self) -> ExecOutput {
        self.robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                "robot_onboard@self",
                "--place",
                &format!("cloud_inference@{}", self.cloud_core_node),
                &container_launcher(SPLIT_COMPUTE_LAUNCHER_FILE),
            ])
            .await
    }
}

/// The substrate every federated test stands on: a node built and started
/// INSIDE a daemon container.
///
/// Deliberately narrow. It covers the two execution paths the documented
/// launcher needs and nothing else: `uvc_camera_python_mock` is a container
/// node, so apptainer has to build a SIF under Docker, and `my_python_robot_arm`
/// is a native one, so `uv` has to build a venv from the image's toolchain.
///
/// Both assertions read the node's own run log rather than its status, because
/// an instance can reach Running and still be silent. When a federated launch
/// test fails, this test is what says whether the cause is the federation or
/// the machine underneath it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fixture_nodes_build_and_run_inside_a_daemon_container() {
    let federation = start_federation("peppy-probe").await;

    let launch = federation
        .robot
        .peppy(&[
            "stack",
            "launch",
            &container_launcher(NODE_PROBE_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        launch.success(),
        "a fixture node must build and start inside the daemon container:\n{}",
        launch.text
    );

    federation
        .robot
        .wait_for_node_log("probe_cam_inst", "[uvc_camera] Emitted frame")
        .await;
    federation
        .robot
        .wait_for_node_log("probe_arm_inst", "[arm] published joint_states")
        .await;
}

/// A second launch of the same launcher reuses the container image the first
/// one built, under a real apptainer: the image is keyed by the staged
/// sources, so the relaunch spends no time in `apptainer build`. Read off the
/// node's build logs on the daemon's disk, which is where the reuse is
/// recorded whatever the CLI shows.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunch_reuses_the_cached_container_build() {
    let federation = start_federation("peppy-relaunch").await;
    let launcher = container_launcher(NODE_PROBE_LAUNCHER_FILE);

    let first = federation
        .robot
        .peppy(&["stack", "launch", &launcher])
        .await;
    assert!(
        first.success(),
        "the first launch must build and start the fixture nodes:\n{}",
        first.text
    );

    let second = federation
        .robot
        .peppy(&["stack", "launch", &launcher])
        .await;
    assert!(
        second.success(),
        "the relaunch must start the fixture nodes from the cached image:\n{}",
        second.text
    );
    federation
        .robot
        .wait_for_node_log("probe_cam_inst", "[uvc_camera] Emitted frame")
        .await;

    let build_logs = format!("{CONTAINER_PEPPY_HOME}/logs/build/uvc_camera_python_mock_v1_*.log");
    let oldest = require_success(
        federation
            .robot
            .exec(vec![
                "/bin/sh",
                "-c",
                &format!("ls {build_logs} | sort | head -n 1 | xargs cat"),
            ])
            .await,
        "reading the first build log",
    );
    let newest = require_success(
        federation
            .robot
            .exec(vec![
                "/bin/sh",
                "-c",
                &format!("ls {build_logs} | sort | tail -n 1 | xargs cat"),
            ])
            .await,
        "reading the second build log",
    );
    assert!(
        oldest.contains("Starting build..."),
        "the first launch ran apptainer build:\n{oldest}"
    );
    assert!(
        newest.contains(CACHED_BUILD_REUSE_PREFIX),
        "the relaunch reused the image:\n{newest}"
    );
    assert!(
        !newest.contains("Starting build..."),
        "the relaunch never ran apptainer build:\n{newest}"
    );
}

/// The caller's environment stays on the caller's machine.
///
/// The scenario is a field failure: an operator launches from a machine whose
/// `PATH` does not name the directory the peer keeps its tools in, and the
/// peer's `uv sync` dies with `No such file or directory` on a host that has
/// `uv` installed, because the build resolves programs through the caller's
/// forwarded `PATH` instead of the peer's own. So this launch runs under a
/// `PATH` of directories that exist on every machine here but hold no `uv`,
/// and the peer must still build and start the node. The canary closes the
/// other half: no variable from the operator's shell appears in any process
/// on the peer, so what a node sees is decided by its launcher file and the
/// machine it runs on, not by whoever typed the launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_builds_with_its_own_environment_not_the_callers() {
    let federation = start_federation("peppy-caller-env").await;

    let launch = federation
        .robot
        .peppy_with_env(
            &[
                "PATH=/usr/sbin:/usr/bin:/sbin:/bin",
                "CALLER_CANARY=from_the_operators_shell",
            ],
            &[
                "stack",
                "launch",
                "--place",
                &format!("remote_worker@{}", federation.cloud_core_node),
                &container_launcher(CALLER_ENV_PROBE_LAUNCHER_FILE),
            ],
        )
        .await;
    assert!(
        launch.success(),
        "a peer must resolve build tools on its own machine, not through the caller's PATH:\n{}",
        launch.text
    );

    federation
        .cloud
        .wait_for_node_log("env_probe_arm_inst", "[arm] published joint_states")
        .await;

    let leaked = federation
        .cloud
        .exec(vec![
            "/bin/sh",
            "-c",
            "cat /proc/[0-9]*/environ 2>/dev/null | tr '\\0' '\\n' | grep ^CALLER_CANARY= || true",
        ])
        .await;
    assert!(
        !leaked.text.contains("CALLER_CANARY"),
        "the caller's environment must not reach any process on the peer:\n{}",
        leaked.text
    );
}

/// The whole thing, end to end: one command on the robot, two machines running
/// the halves the launcher describes, ordering preserved across the boundary,
/// and one `stack reset --federated` clearing both.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_federated_launch_places_each_instance_on_its_wired_core_node() {
    let federation = start_federation("peppy-fed-place").await;

    let launch = federation.launch_split().await;
    assert!(
        launch.success(),
        "the federated launch must succeed:\n{}",
        launch.text
    );

    // The operator typed one command, and it replaced a stack on a machine
    // they never named directly. That must be said out loud.
    assert!(
        launch.text.contains("REPLACE the node stack")
            && launch.text.contains(&federation.cloud_core_node),
        "the launch must name the remote daemons it is about to replace:\n{}",
        launch.text
    );

    // Ordering across the boundary: the planner's `scene` slot is bound to the
    // camera on the robot, so the camera starts first even though the two are
    // on different machines.
    assert_starts_before(&launch.text, "wrist_cam_inst", "planner_inst");

    // The peer's own output reaches the operator's terminal, attributed. A
    // launch that ran half its work on a machine you cannot see the output of
    // is not one you can debug.
    assert!(
        launch
            .text
            .contains(&format!("[{}]", federation.cloud_core_node)),
        "the peer's feedback must be relayed and attributed:\n{}",
        launch.text
    );

    // The final log listing names every machine's files, each entry stamped
    // with the core node whose filesystem holds the path. The planner lives
    // only on the peer, so its label@core-node prefix appears exactly three
    // times: once each under Add, Build and Run.
    assert!(
        launch.text.contains(&format!(
            "uvc_camera_python_mock:v1@{}: ",
            federation.robot_core_node
        )),
        "the log listing must name the coordinator's own entries:\n{}",
        launch.text
    );
    let planner_entry = format!("deliberative_planner:v1@{}: ", federation.cloud_core_node);
    assert_eq!(
        launch.text.matches(&planner_entry).count(),
        3,
        "Add, Build and Run must each list the peer-held planner log:\n{}",
        launch.text
    );

    // An untargeted `stack list` fans out over the whole federation and prints
    // every machine's section, so proving an instance is NOT on a daemon takes
    // that daemon's own slice. Wait for the fan-out to settle, then ask each
    // daemon about itself.
    federation
        .robot
        .wait_for_stack(|text| ROBOT_INSTANCES.iter().all(|id| holds_instance(text, id)))
        .await;
    federation
        .cloud
        .wait_for_stack(|text| CLOUD_INSTANCES.iter().all(|id| holds_instance(text, id)))
        .await;

    let robot_stack = federation
        .robot
        .stack_list(Some(&federation.robot_core_node))
        .await;
    assert_holds_exactly(
        &robot_stack.text,
        &federation.robot_core_node,
        &ROBOT_INSTANCES,
        &CLOUD_INSTANCES,
    );

    let cloud_stack = federation
        .cloud
        .stack_list(Some(&federation.cloud_core_node))
        .await;
    assert_holds_exactly(
        &cloud_stack.text,
        &federation.cloud_core_node,
        &CLOUD_INSTANCES,
        &ROBOT_INSTANCES,
    );

    // The data plane, which is the only proof the wiring survived the boundary:
    // a slot can be bound, and reported bound, while carrying nothing. Each of
    // the three cross-daemon mechanisms appears exactly once in this launcher.
    //
    // Producer link: the planner's `scene` slot is bound to the camera, which
    // runs on the other machine.
    federation
        .cloud
        .wait_for_node_log("planner_inst", "first frame received across the boundary")
        .await;
    // Pairing: the policy and the planner hold each other across the boundary,
    // and each side sees what the other sent.
    federation
        .robot
        .wait_for_node_log("reflex_inst", "adopted subgoal")
        .await;
    // Observation: the recorder taps the executor side of that pairing from a
    // third machine-local vantage, without joining it.
    federation
        .cloud
        .wait_for_node_log("recorder_inst", "observing execution")
        .await;

    // A `stack reset` on the coordinator tears down both slices, because the
    // participants are rediscovered from the launch id each slice carries.
    let reset = federation
        .robot
        .peppy(&["stack", "reset", "--federated"])
        .await;
    assert!(
        reset.success(),
        "federated reset must succeed:\n{}",
        reset.text
    );

    for (daemon, instances) in [
        (&federation.robot, ROBOT_INSTANCES.as_slice()),
        (&federation.cloud, CLOUD_INSTANCES.as_slice()),
    ] {
        daemon
            .wait_for_stack(|text| instances.iter().all(|id| !holds_instance(text, id)))
            .await;
    }
}

/// A second launch of the same launcher must work: the first one released every
/// reservation it took when it finished.
///
/// Without the release, a federated stack could be launched exactly once per
/// daemon lifetime, and the second attempt would fail with "already reserved"
/// naming a launch that had long since completed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_finished_launch_releases_its_participants_so_the_next_one_can_run() {
    let federation = start_federation("peppy-fed-relaunch").await;

    let first = federation.launch_split().await;
    assert!(
        first.success(),
        "the first launch must succeed:\n{}",
        first.text
    );

    let second = federation.launch_split().await;
    assert!(
        second.success(),
        "a second launch must not be blocked by the first one's reservation:\n{}",
        second.text
    );
    assert!(
        !second.text.contains("already reserved"),
        "a finished launch must not still hold its participants:\n{}",
        second.text
    );

    federation
        .cloud
        .wait_for_stack(|text| CLOUD_INSTANCES.iter().all(|id| holds_instance(text, id)))
        .await;
}

/// The case the rejected ownership model could not serve: a coordinator that
/// restarted has no memory of who took part, and must find its own launch again
/// by asking the federation.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_restarted_coordinator_rediscovers_its_participants_and_can_reset_them() {
    let federation = start_federation("peppy-fed-restart").await;

    let launch = federation.launch_split().await;
    assert!(launch.success(), "launch must succeed:\n{}", launch.text);
    federation
        .cloud
        .wait_for_stack(|text| CLOUD_INSTANCES.iter().all(|id| holds_instance(text, id)))
        .await;

    // Restart the coordinator's daemon. Everything it knew in RAM is gone.
    federation.robot.restart().await;
    // Wait for the new generation to answer before asking it anything: an
    // untargeted `stack list` needs the coordinator's own daemon up to fan out
    // at all.
    federation.robot.wait_for_stack(|_| true).await;

    // The peer still holds its slice, and that slice still names the launch.
    let cloud_stack = federation
        .cloud
        .wait_for_stack(|text| CLOUD_INSTANCES.iter().all(|id| holds_instance(text, id)))
        .await;
    assert!(
        !cloud_stack.is_empty(),
        "the peer's slice must survive a coordinator restart"
    );

    // A reset from a participant, with no coordinator memory anywhere, still
    // tears down the whole launch.
    let reset = federation
        .cloud
        .peppy(&["stack", "reset", "--federated"])
        .await;
    assert!(
        reset.success(),
        "a federated reset must work from a participant too:\n{}",
        reset.text
    );

    federation
        .cloud
        .wait_for_stack(|text| CLOUD_INSTANCES.iter().all(|id| !holds_instance(text, id)))
        .await;
}

/// A peer that goes away after the launch has been validated. The launch must
/// fail loudly and name the machine, rather than hanging or reporting success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unreachable_peer_fails_the_launch_and_is_named() {
    let federation = start_federation("peppy-fed-partial").await;

    federation.cloud.stop().await;
    federation.cloud.wait_for_exit().await;

    let launch = federation.launch_split().await;
    assert!(
        !launch.success(),
        "a launch naming a dead peer must fail:\n{}",
        launch.text
    );
    assert!(
        launch.text.contains(&federation.cloud_core_node),
        "the failure must name the machine that could not be reached:\n{}",
        launch.text
    );

    // Nothing was torn down on the coordinator: preflight refuses before the
    // destructive phase, so an unreachable peer cannot cost you the stack you
    // already had.
    let robot_stack = federation.robot.stack_list(None).await;
    assert!(
        ROBOT_INSTANCES
            .iter()
            .all(|id| !holds_instance(&robot_stack.text, id)),
        "a refused preflight must not have started anything:\n{}",
        robot_stack.text
    );
}

// ── Clock domains across a federation ─────────────────────────────────────
//
// A launcher declares the domains its launch runs on and each instance names
// the one it reads, so every rule about a domain is a rule about the
// document: a binding names a domain the document declares, and a declaration
// names a publisher the launch deploys. Both are settled while the plan is
// still on the coordinator, which is what makes them assertable without
// building or starting anything: the refusal arrives before the launch turns
// destructive, so every machine's stack is untouched afterwards.

/// Asserts no machine of the fleet holds any instance the clock launchers
/// deploy.
///
/// Each daemon's OWN slice, because an untargeted listing renders every
/// machine's section and a reference to a remote instance reads as a local one
/// there.
async fn assert_fleet_started_nothing(fleet: &Fleet) {
    for (daemon, core_node) in [
        (&fleet.coordinator, FLEET_COORDINATOR_CORE_NODE),
        (&fleet.station_a, FLEET_STATION_A_CORE_NODE),
        (&fleet.station_b, FLEET_STATION_B_CORE_NODE),
    ] {
        let stack = daemon.stack_list(Some(core_node)).await;
        assert_holds_exactly(&stack.text, core_node, &[], &FLEET_INSTANCES);
    }
}

/// A probe naming a domain the document never declares is refused, and the
/// refusal names the instance and the domain it asked for. An undeclared name
/// is an error, so the launch stops while the plan is still on the
/// coordinator and no machine holds any of it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launch_binding_an_unknown_domain_is_refused_before_any_machine_is_touched() {
    let fleet = start_fleet("peppy-clock-unknown").await;

    let launch = fleet
        .coordinator
        .peppy(&[
            "stack",
            "launch",
            "--place",
            &format!("station_a@{FLEET_STATION_A_CORE_NODE}"),
            "--place",
            &format!("station_b@{FLEET_STATION_B_CORE_NODE}"),
            &container_launcher(UNKNOWN_CLOCK_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        !launch.success(),
        "a launch binding a domain nothing declares must fail:\n{}",
        launch.text
    );
    assert!(
        launch.text.contains(&format!(
            "instance `{FLEET_PROBE_COORD_INSTANCE}` binds clock \
             `{UNDECLARED_CLOCK_DOMAIN}`"
        )),
        "the refusal must name the instance and the domain it asked for:\n{}",
        launch.text
    );

    assert_fleet_started_nothing(&fleet).await;
}

/// The samples a probe's run log reports, in order: each ordinal with the
/// value served, or `None` while the clock was not ready. Reads complete
/// lines only: the log is read while the node writes it, and a tick value
/// truncated mid-line must not be mistaken for a smaller instant.
fn probe_samples(log: &str) -> Vec<(u64, Option<u64>)> {
    let complete = match log.rfind('\n') {
        Some(last_newline) => &log[..last_newline],
        None => "",
    };
    complete
        .lines()
        .filter_map(|line| {
            // Anywhere in the line, not a line prefix: the daemon may stamp
            // its own prefix onto every captured stdout line.
            let (_, rest) = line.split_once("[clock-probe] sample=")?;
            let (sample, tail) = rest.split_once(' ')?;
            let sample = sample.parse().ok()?;
            let value = tail
                .strip_prefix("now_ns=")
                .map(|value| value.parse().expect("probe values are integers"));
            Some((sample, value))
        })
        .collect()
}

/// How many samples past a reference ordinal the probe must report before a
/// held-cap claim is judged: a hold cannot be asserted on the sample that
/// just arrived.
const SETTLE_SAMPLES: u64 = 20;

/// Waits until `probe_instance`'s parsed history satisfies `satisfied`, using
/// `marker` as the raw-log wait. `wait_for_node_log` matches the raw log,
/// which can end mid-write, so a marker can be present while the complete
/// lines do not carry it yet; the parse decides, the marker only paces.
async fn wait_for_parsed_samples(
    daemon: &Daemon,
    probe_instance: &str,
    marker: &str,
    satisfied: impl Fn(&[(u64, Option<u64>)]) -> bool,
) -> Vec<(u64, Option<u64>)> {
    let started = Instant::now();
    loop {
        let log = daemon.wait_for_node_log(probe_instance, marker).await;
        let samples = probe_samples(&log);
        if satisfied(&samples) {
            return samples;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "`{probe_instance}` never satisfied the parsed wait for `{marker}`:\n{log}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Asserts one machine's whole clock history: nothing but scripted instants,
/// never a regression, never unready again once a tick was served, and every
/// sample past `capped_after` holding exactly the final instant.
///
/// The held tail is deterministic because no instant past the final one is
/// ever published: any advance is a machine keeping its own time (a wall
/// value would be nineteen digits) and any regression is a stale tick
/// overtaking the ramp.
fn assert_scripted_history(
    daemon: &Daemon,
    probe_instance: &str,
    samples: &[(u64, Option<u64>)],
    capped_after: u64,
) {
    let mut previous = None;
    for (sample, value) in samples {
        match value {
            Some(value) => {
                let tick = value / SCRIPTED_TICK_STEP_NS;
                assert!(
                    value % SCRIPTED_TICK_STEP_NS == 0 && (1..=SCRIPTED_TICK_COUNT).contains(&tick),
                    "{}'s `{probe_instance}` served {value}, which no scripted tick ever was; \
                     its machine is keeping its own time",
                    daemon.name
                );
                if let Some(previous) = previous {
                    assert!(
                        *value >= previous,
                        "{}'s `{probe_instance}` regressed from {previous} to {value} (sample \
                         {sample}); a stale tick overtook the ramp",
                        daemon.name
                    );
                }
                previous = Some(*value);
                if *sample > capped_after {
                    assert_eq!(
                        *value, SCRIPTED_FINAL_NS,
                        "{}'s `{probe_instance}` moved after the ramp topped out (sample \
                         {sample}); a capped source must hold every machine",
                        daemon.name
                    );
                }
            }
            None => assert!(
                previous.is_none(),
                "{}'s `{probe_instance}` went unready after serving ticks (sample {sample})",
                daemon.name
            ),
        }
    }
}

/// Waits for the final scripted instant on one machine, lets the probe sample
/// well past it, and asserts the whole history. Returns the last sample
/// ordinal seen, so a later phase can anchor "still holding" after it.
async fn assert_probe_capped_at_final(daemon: &Daemon, probe_instance: &str) -> u64 {
    let final_marker = format!("now_ns={SCRIPTED_FINAL_NS}");
    let samples = wait_for_parsed_samples(daemon, probe_instance, &final_marker, |samples| {
        samples
            .iter()
            .any(|(_, value)| *value == Some(SCRIPTED_FINAL_NS))
    })
    .await;
    let first_final = samples
        .iter()
        .find(|(_, value)| *value == Some(SCRIPTED_FINAL_NS))
        .map(|(sample, _)| *sample)
        .expect("the wait returned only once the final instant parsed");
    assert_probe_holds_past(daemon, probe_instance, first_final).await
}

/// Waits for [`SETTLE_SAMPLES`] more samples past `after`, asserts the whole
/// history with its cap anchored at `after`, and returns the last sample
/// ordinal seen.
async fn assert_probe_holds_past(daemon: &Daemon, probe_instance: &str, after: u64) -> u64 {
    let settle = after + SETTLE_SAMPLES;
    let settle_marker = format!("sample={settle} ");
    let samples = wait_for_parsed_samples(daemon, probe_instance, &settle_marker, |samples| {
        samples.iter().any(|(sample, _)| *sample >= settle)
    })
    .await;
    assert_scripted_history(daemon, probe_instance, &samples, after);
    samples
        .last()
        .map(|(sample, _)| *sample)
        .expect("a satisfied wait returns a non-empty history")
}

/// Asserts a probe binding no domain reads its own machine's clock: every
/// sample served, none unready, and every instant past anything the scripted
/// ramp reaches. A wall instant is nineteen digits where the whole ramp is
/// ten, so the two are told apart by size alone.
async fn assert_probe_reads_wall(daemon: &Daemon, probe_instance: &str) {
    let settle_marker = format!("sample={SETTLE_SAMPLES} ");
    let samples = wait_for_parsed_samples(daemon, probe_instance, &settle_marker, |samples| {
        samples.iter().any(|(sample, _)| *sample >= SETTLE_SAMPLES)
    })
    .await;
    for (sample, value) in &samples {
        let Some(value) = value else {
            panic!(
                "{}'s `{probe_instance}` reads its own machine's clock, which is ready from \
                 its first sample (sample {sample})",
                daemon.name
            );
        };
        assert!(
            *value > SCRIPTED_FINAL_NS,
            "{}'s `{probe_instance}` served {value} (sample {sample}), which is no later than \
             the scripted ramp's last instant; it is reading the simulated domain",
            daemon.name
        );
    }
}

/// How a domain published on `core_node` is named once it is running.
fn domain_id(core_node: &str) -> String {
    format!("{FLEET_CLOCK_DOMAIN}@{core_node}")
}

/// The line a launch prints for each domain it starts: the instance supplying
/// it, and the identity its consumers address.
fn publishes_clock_line(instance: &str, core_node: &str) -> String {
    format!(
        "instance `{instance}` publishes clock `{FLEET_CLOCK_DOMAIN}` ({})",
        domain_id(core_node)
    )
}

/// The `peppy clock list --json` report, read from `daemon`. One daemon
/// answers for itself alone, so this asks the coordinator, which gathers the
/// whole federation's answer.
async fn clock_list(daemon: &Daemon) -> serde_json::Value {
    let listed = require_success(
        daemon.peppy(&["clock", "list", "--json"]).await,
        &format!("listing clock domains from {}", daemon.name),
    );
    let document = listed
        .lines()
        .find(|line| line.starts_with('{'))
        .unwrap_or_else(|| panic!("missing clock JSON: {listed}"));
    serde_json::from_str(document).expect("`clock list --json` prints one JSON document")
}

/// The report's entry for `clock`, if the federation is running one.
fn listed_domain<'a>(report: &'a serde_json::Value, clock: &str) -> Option<&'a serde_json::Value> {
    report["domains"]
        .as_array()
        .unwrap_or_else(|| panic!("a clock report lists its domains: {report}"))
        .iter()
        .find(|domain| domain["clock"] == clock)
}

/// The report's entry for the domain published on `core_node`.
fn domain_of<'a>(report: &'a serde_json::Value, core_node: &str) -> &'a serde_json::Value {
    let clock = domain_id(core_node);
    listed_domain(report, &clock)
        .unwrap_or_else(|| panic!("no `{clock}` among the listed domains: {report}"))
}

/// The lifetime the report carries for the domain published on `core_node`.
fn domain_incarnation(report: &serde_json::Value, core_node: &str) -> u64 {
    let domain = domain_of(report, core_node);
    domain["incarnation"]
        .as_u64()
        .unwrap_or_else(|| panic!("a domain's lifetime is a number: {domain}"))
}

/// Polls `peppy clock list --json` until the domain published on `core_node`
/// is ticking, and returns the whole report.
///
/// The listing is what opens a daemon's subscription to a domain it hosts, so
/// the first one after a launch can report a ticking domain as waiting; the
/// next, a subscription later, reports it.
async fn wait_for_ticking_domain(fleet: &Fleet, core_node: &str) -> serde_json::Value {
    let clock = domain_id(core_node);
    let started = Instant::now();
    loop {
        let report = clock_list(&fleet.coordinator).await;
        if listed_domain(&report, &clock).is_some_and(|domain| domain["ready"] == true) {
            return report;
        }
        assert!(
            started.elapsed() < TIMEOUT,
            "`{clock}` never reported ready:\n{report}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// Launches the fleet clock launcher, placing each station on its machine, and
/// returns the launch's feedback.
async fn launch_fleet_clock(fleet: &Fleet) -> String {
    let launch = fleet
        .coordinator
        .peppy(&[
            "stack",
            "launch",
            "--place",
            &format!("station_a@{FLEET_STATION_A_CORE_NODE}"),
            "--place",
            &format!("station_b@{FLEET_STATION_B_CORE_NODE}"),
            &container_launcher(FLEET_CLOCK_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        launch.success(),
        "the fleet clock launch must succeed:\n{}",
        launch.text
    );
    launch.text
}

/// The ticket's testable shape, positive path: a launch spanning the simulator
/// machine and two more. One scripted source on the coordinator supplies the
/// domain a probe on each machine binds, and a fourth probe beside them binds
/// none, so one daemon carries the domain and wall time at once. Every bound
/// machine serves nothing but scripted instants, never regressing, reaches the
/// same final instant, and holds there once the ramp tops out; then the source
/// is stopped outright and, with no tick arriving at all, every bound machine
/// still holds the same instant rather than drifting anywhere. Exact values,
/// not cross-container timestamp comparison, which the header of this file
/// rules out for good reason.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fleet_of_three_machines_shares_one_scripted_clock_and_holds_its_cap() {
    let fleet = start_fleet("peppy-fleet-clock").await;

    let launch = launch_fleet_clock(&fleet).await;
    assert!(
        launch.contains(&publishes_clock_line(
            FLEET_CLOCK_SOURCE_INSTANCE,
            FLEET_COORDINATOR_CORE_NODE
        )),
        "the launch names the domain and the machine supplying it:\n{launch}"
    );

    fleet
        .coordinator
        .wait_for_node_log(FLEET_CLOCK_SOURCE_INSTANCE, CLOCK_SOURCE_PUBLISHING_MARKER)
        .await;
    fleet
        .coordinator
        .wait_for_node_log(
            FLEET_CLOCK_SOURCE_INSTANCE,
            &format!("final tick {SCRIPTED_FINAL_NS} published"),
        )
        .await;

    let probes = [
        (&fleet.coordinator, FLEET_PROBE_COORD_INSTANCE),
        (&fleet.station_a, FLEET_PROBE_STATION_A_INSTANCE),
        (&fleet.station_b, FLEET_PROBE_STATION_B_INSTANCE),
    ];
    let mut held_through = Vec::with_capacity(probes.len());
    for &(daemon, probe_instance) in &probes {
        held_through.push(assert_probe_capped_at_final(daemon, probe_instance).await);
    }

    // The probe that binds nothing reads its machine's own clock, on the very
    // daemon hosting a bound one.
    assert_probe_reads_wall(&fleet.station_b, FLEET_PROBE_WALL_INSTANCE).await;

    // The federation's own account of the domain: one entry, named for the
    // machine its publisher runs on, ticking, and read by exactly the probes
    // bound to it, with wall listed beside it as the built-in it is.
    let report = wait_for_ticking_domain(&fleet, FLEET_COORDINATOR_CORE_NODE).await;
    assert_eq!(
        report["wall"]["clock"], "wall",
        "wall is listed beside every simulated domain:\n{report}"
    );
    let domain = domain_of(&report, FLEET_COORDINATOR_CORE_NODE);
    assert_eq!(
        domain["publisher"], FLEET_CLOCK_SOURCE_INSTANCE,
        "the domain names the instance supplying it:\n{domain}"
    );
    assert_eq!(
        domain["last_tick_ns"].as_u64(),
        Some(SCRIPTED_FINAL_NS),
        "the domain holds the ramp's last instant:\n{domain}"
    );
    let mut consumers: Vec<String> = domain["consumers"]
        .as_array()
        .unwrap_or_else(|| panic!("a domain lists its consumers: {domain}"))
        .iter()
        .map(|consumer| {
            consumer
                .as_str()
                .unwrap_or_else(|| panic!("a consumer reads as text: {domain}"))
                .to_owned()
        })
        .collect();
    consumers.sort_unstable();
    assert_eq!(
        consumers,
        [
            format!("{FLEET_PROBE_COORD_INSTANCE}@{FLEET_COORDINATOR_CORE_NODE}"),
            format!("{FLEET_PROBE_STATION_A_INSTANCE}@{FLEET_STATION_A_CORE_NODE}"),
            format!("{FLEET_PROBE_STATION_B_INSTANCE}@{FLEET_STATION_B_CORE_NODE}"),
        ],
        "the domain is read by every probe bound to it and by nothing else"
    );

    // The cap above held under a source still republishing its final instant.
    // Now make the loss real: stop the source, and with no tick arriving at
    // all every bound machine must keep the very same instant. Frozen, never
    // drifting back to wall time: today's single-robot staleness behavior,
    // fleet-wide.
    let stopped = fleet
        .coordinator
        .peppy(&["node", "stop", FLEET_CLOCK_SOURCE_INSTANCE])
        .await;
    assert!(
        stopped.success(),
        "stopping the source must succeed:\n{}",
        stopped.text
    );
    for (&(daemon, probe_instance), seen) in probes.iter().zip(held_through) {
        assert_probe_holds_past(daemon, probe_instance, seen).await;
    }
}

/// A domain's identity carries a lifetime minted for each launch of it, so the
/// same launcher run twice runs two timelines. The probes of the second launch
/// converge on its ramp, which they reach only by reading the lifetime it
/// minted: the ticks of the first address a stream nothing in the second
/// subscribes to, so a probe left on it would wait at "clock not ready" for as
/// long as it ran.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relaunch_mints_a_fresh_domain() {
    let fleet = start_fleet("peppy-clock-relaunch").await;

    launch_fleet_clock(&fleet).await;
    let first = domain_incarnation(
        &wait_for_ticking_domain(&fleet, FLEET_COORDINATOR_CORE_NODE).await,
        FLEET_COORDINATOR_CORE_NODE,
    );

    require_success(
        fleet
            .coordinator
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "resetting the fleet before the relaunch",
    );

    launch_fleet_clock(&fleet).await;
    let second = domain_incarnation(
        &wait_for_ticking_domain(&fleet, FLEET_COORDINATOR_CORE_NODE).await,
        FLEET_COORDINATOR_CORE_NODE,
    );
    assert_ne!(
        first, second,
        "a relaunch mints a lifetime of its own under the same domain name"
    );

    // Each probe of the second launch starts before its domain has ticked and
    // converges on the new ramp: unready first, then scripted instants only,
    // to the same cap.
    for (daemon, probe_instance) in [
        (&fleet.coordinator, FLEET_PROBE_COORD_INSTANCE),
        (&fleet.station_a, FLEET_PROBE_STATION_A_INSTANCE),
        (&fleet.station_b, FLEET_PROBE_STATION_B_INSTANCE),
    ] {
        assert_probe_capped_at_final(daemon, probe_instance).await;
    }
}

/// A launch typed from a machine that hosts none of it: the publisher runs on
/// a station, so the domain is named for that station, and both stations read
/// its scripted instants to their cap. The coordinator is a bystander
/// throughout, which is what makes the domain's identity a fact about the
/// launch rather than about the machine the launch was typed from.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_fully_placed_launch_runs_its_publisher_on_a_station_and_both_follow() {
    let fleet = start_fleet("peppy-fleet-hosts").await;

    let launch = fleet
        .coordinator
        .peppy(&[
            "stack",
            "launch",
            "--place",
            &format!("station_a@{FLEET_STATION_A_CORE_NODE}"),
            "--place",
            &format!("station_b@{FLEET_STATION_B_CORE_NODE}"),
            &container_launcher(FULLY_PLACED_CLOCK_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        launch.success(),
        "a launch hosted wholly on its stations must succeed:\n{}",
        launch.text
    );
    assert!(
        launch.text.contains(&publishes_clock_line(
            FLEET_CLOCK_SOURCE_INSTANCE,
            FLEET_STATION_A_CORE_NODE
        )),
        "the domain is named for the station its publisher runs on:\n{}",
        launch.text
    );

    fleet
        .station_a
        .wait_for_node_log(FLEET_CLOCK_SOURCE_INSTANCE, CLOCK_SOURCE_PUBLISHING_MARKER)
        .await;

    for (daemon, probe_instance) in [
        (&fleet.station_a, FLEET_PROBE_STATION_A_INSTANCE),
        (&fleet.station_b, FLEET_PROBE_STATION_B_INSTANCE),
    ] {
        assert_probe_capped_at_final(daemon, probe_instance).await;
    }
}

/// A phase that fails on a machine the operator cannot see must still name the
/// file that explains it, on the machine whose filesystem holds it.
///
/// The peer accepts the run goal and fails it, which is the only shape that
/// produces a failed remote log entry: a goal the peer never accepts has no log
/// file to name, and `an_unreachable_peer_fails_the_launch_and_is_named` covers
/// that one. Everything asserted here is read from the launch's own listing,
/// because that listing is the operator's whole route to a log file sitting on
/// another machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_phase_that_fails_still_names_the_peers_log_file() {
    let federation = start_federation("peppy-fed-peer-fail").await;

    // Planted on the peer only, so the coordinator's own instance gets through
    // every phase.
    plant_uncreatable_instance_dir(&federation.cloud, PEER_RUN_FAILURE_INSTANCE).await;

    let launch = federation
        .robot
        .peppy(&[
            "stack",
            "launch",
            "--place",
            &format!("remote_worker@{}", federation.cloud_core_node),
            &container_launcher(PEER_RUN_FAILURE_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        !launch.success(),
        "a peer that cannot start its container must fail the launch:\n{}",
        launch.text
    );

    // One phase failed on one machine, so exactly one entry carries the marker,
    // and it is the peer's.
    assert_eq!(
        launch.text.matches("[FAILED]").count(),
        1,
        "only the phase that failed may be marked:\n{}",
        launch.text
    );
    let peer_node = "uvc_camera_video_reconstruction_python:v1";
    assert!(
        launch.text.contains(&format!(
            "{peer_node}@{} [FAILED]: ",
            federation.cloud_core_node
        )),
        "the failed run must be listed against the peer holding its log file:\n{}",
        launch.text
    );

    // The phases that DID succeed on that same peer keep their entries. A run
    // that failed is not the whole story of the machine it failed on, and the
    // add and build logs are where an operator looks next.
    assert_eq!(
        launch
            .text
            .matches(&format!("{peer_node}@{}: ", federation.cloud_core_node))
            .count(),
        2,
        "the peer's successful Add and Build must still be listed:\n{}",
        launch.text
    );
}

/// Makes `instance` impossible to start on `daemon`.
///
/// A plain file occupies the name: the start first clears any leftover
/// working directory of that name, that clear refuses on a non-directory,
/// and the directory can never be created. It goes on the working directory
/// because bind sources are prepared when a peer takes over its slice,
/// before the phases these tests are about; nothing prepares this one ahead
/// of the start.
async fn plant_uncreatable_instance_dir(daemon: &Daemon, instance: &str) {
    let instance_dir = instance_dir(instance);
    require_success(
        daemon
            .exec(vec![
                "sh",
                "-c",
                &format!(
                    "mkdir -p {CONTAINER_PEPPY_HOME}/instances \
                     && printf '' > {instance_dir}"
                ),
            ])
            .await,
        &format!("planting an uncreatable {instance_dir}"),
    );
}

/// Frees the name [`plant_uncreatable_instance_dir`] took, so the instance
/// starts on the next attempt.
async fn clear_planted_instance_dir(daemon: &Daemon, instance: &str) {
    let instance_dir = instance_dir(instance);
    require_success(
        daemon.exec(vec!["rm", &instance_dir]).await,
        &format!("clearing the planted {instance_dir}"),
    );
}

/// The working directory a daemon materializes for `instance`.
fn instance_dir(instance: &str) -> String {
    format!("{CONTAINER_PEPPY_HOME}/instances/{instance}")
}

/// The newest build log `node:tag` left on `daemon`, which is where a reuse
/// is recorded whatever the CLI showed.
async fn newest_build_log(daemon: &Daemon, node: &str, tag: &str) -> String {
    require_success(
        daemon
            .exec(vec![
                "/bin/sh",
                "-c",
                &format!(
                    "ls {CONTAINER_PEPPY_HOME}/logs/build/{node}_{tag}_*.log \
                     | sort | tail -n 1 | xargs cat"
                ),
            ])
            .await,
        &format!("reading the newest {node}:{tag} build log"),
    )
}

/// A federated build adds and builds each node on the machine the launch
/// places it, and starts nothing anywhere: the peer's instance could not
/// start, and the build succeeds all the same. A launch with the same
/// arguments then starts the stack from the peer's build.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_federated_build_builds_each_node_where_the_launch_places_it() {
    let federation = start_federation("peppy-fed-build").await;
    plant_uncreatable_instance_dir(&federation.cloud, PEER_RUN_FAILURE_INSTANCE).await;
    let place = format!("remote_worker@{}", federation.cloud_core_node);
    let launcher = container_launcher(PEER_RUN_FAILURE_LAUNCHER_FILE);

    let build = federation
        .robot
        .peppy(&["stack", "build", "--place", &place, &launcher])
        .await;
    assert!(
        build.success(),
        "a build starts no instance, so the peer's unstartable one cannot fail it:\n{}",
        build.text
    );
    assert!(
        build.text.contains(&format!(
            "This build will REPLACE the node stack on 1 remote daemon(s): {}",
            federation.cloud_core_node
        )),
        "the build must name the remote daemons it is about to replace:\n{}",
        build.text
    );
    let peer_node = "uvc_camera_video_reconstruction_python:v1";
    assert_eq!(
        build
            .text
            .matches(&format!("{peer_node}@{}: ", federation.cloud_core_node))
            .count(),
        2,
        "the peer's node has an Add and a Build log and no Run log:\n{}",
        build.text
    );

    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "listing the built federation",
    );
    assert_eq!(
        slice_nodes(&listed, &federation.robot_core_node),
        BTreeMap::from([(
            "uvc_camera_python_mock:v1".to_owned(),
            ("Ready".to_owned(), 0)
        )]),
        "{listed}"
    );
    assert_eq!(
        slice_nodes(&listed, &federation.cloud_core_node),
        BTreeMap::from([(peer_node.to_owned(), ("Ready".to_owned(), 0))]),
        "{listed}"
    );
    // Both machines record the build as the launch their slice came from,
    // which is how `stack reset --federated` finds them afterwards, and the
    // finished build holds neither of them.
    let build_launch =
        coordinator_section(&listed, &federation.robot_core_node)["launch"]["launch_id"]
            .as_str()
            .unwrap_or_else(|| panic!("the coordinator records the build's launch: {listed}"))
            .to_owned();
    let peer_section = coordinator_section(&listed, &federation.cloud_core_node);
    assert_eq!(
        peer_section["launch"]["launch_id"].as_str(),
        Some(build_launch.as_str()),
        "the peer records the same build: {listed}"
    );
    assert!(
        peer_section["reservation"].is_null(),
        "a finished build holds no peer: {listed}"
    );

    clear_planted_instance_dir(&federation.cloud, PEER_RUN_FAILURE_INSTANCE).await;
    let launch = federation
        .robot
        .peppy(&["stack", "launch", "--place", &place, &launcher])
        .await;
    assert!(
        launch.success(),
        "the launch must start the built stack:\n{}",
        launch.text
    );
    let newest_peer_build_log = newest_build_log(
        &federation.cloud,
        "uvc_camera_video_reconstruction_python",
        "v1",
    )
    .await;
    assert!(
        newest_peer_build_log.contains(CACHED_BUILD_REUSE_PREFIX),
        "the launch reused the peer's build:\n{newest_peer_build_log}"
    );
}

/// A build that fails on the peer fails the whole build, names the peer's
/// failed log, and clears the slice the build started there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_peer_build_that_fails_fails_the_build_and_clears_its_slice() {
    let federation = start_federation("peppy-fed-build-fail").await;
    // The peer's native node builds with `uv sync`; a `uv` that always fails
    // makes that build, and nothing on the coordinator, fail.
    require_success(
        federation
            .cloud
            .exec(vec![
                "sh",
                "-c",
                "printf '#!/bin/sh\nexit 7\n' > /usr/local/bin/uv \
                 && chmod +x /usr/local/bin/uv \
                 && ! uv --version",
            ])
            .await,
        "planting a failing uv on the peer",
    );

    let build = federation
        .robot
        .peppy(&[
            "stack",
            "build",
            "--place",
            &format!("remote_worker@{}", federation.cloud_core_node),
            &container_launcher(CALLER_ENV_PROBE_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        !build.success(),
        "a peer whose build fails must fail the build:\n{}",
        build.text
    );
    assert!(
        build
            .text
            .contains("Build failed: failed to build node my_python_robot_arm")
            && build.text.contains(&format!(
                "my_python_robot_arm:v1@{} [FAILED]: ",
                federation.cloud_core_node
            )),
        "the failure names the operation, the node, and the peer's failed build log:\n{}",
        build.text
    );
    assert!(
        build.text.contains(&format!(
            "Clearing the slice this build started on: `{}`",
            federation.cloud_core_node
        )),
        "the build clears the slice it started on the peer:\n{}",
        build.text
    );

    let listed = require_success(
        federation.cloud.peppy(&["stack", "list", "--json"]).await,
        "listing the peer after the failed build",
    );
    assert!(
        slice_nodes(&listed, &federation.cloud_core_node).is_empty(),
        "the peer's slice is cleared:\n{listed}"
    );
}

/// `--local` collapses a two-machine topology onto one box, unmodified. This is
/// how you develop against a federated launcher with no second machine.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_runs_the_whole_topology_on_one_daemon() {
    let federation = start_federation("peppy-fed-local").await;

    let launch = federation
        .robot
        .peppy(&[
            "stack",
            "launch",
            "--local",
            &container_launcher(SPLIT_COMPUTE_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        launch.success(),
        "`--local` must run the unmodified launcher on one daemon:\n{}",
        launch.text
    );

    // No remote daemon is touched, so nothing is announced as being replaced.
    assert!(
        !launch.text.contains("REPLACE the node stack"),
        "`--local` touches no remote daemon, so it must announce none:\n{}",
        launch.text
    );

    let all_instances: Vec<&str> = ROBOT_INSTANCES
        .iter()
        .chain(CLOUD_INSTANCES.iter())
        .copied()
        .collect();
    federation
        .robot
        .wait_for_stack(|text| all_instances.iter().all(|id| holds_instance(text, id)))
        .await;
    let robot_stack = federation
        .robot
        .stack_list(Some(&federation.robot_core_node))
        .await;
    assert_holds_exactly(
        &robot_stack.text,
        &federation.robot_core_node,
        &all_instances,
        &[],
    );

    // The peer stayed empty throughout. Its OWN slice, not the federation-wide
    // listing, which would show the coordinator's instances too.
    let cloud_stack = federation
        .cloud
        .stack_list(Some(&federation.cloud_core_node))
        .await;
    assert!(
        all_instances
            .iter()
            .all(|id| !holds_instance(&cloud_stack.text, id)),
        "`--local` must leave the peer untouched:\n{}",
        cloud_stack.text
    );
}

/// A declaration names the instance that supplies its domain, so a launcher
/// declaring one whose publisher it never deploys is refused: the refusal
/// names the domain and the instance it is missing, and no machine is touched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_launcher_whose_clock_names_a_missing_publisher_is_refused() {
    let fleet = start_fleet("peppy-missing-publisher").await;

    let launch = fleet
        .coordinator
        .peppy(&[
            "stack",
            "launch",
            &container_launcher(MISSING_PUBLISHER_CLOCK_LAUNCHER_FILE),
        ])
        .await;
    assert!(
        !launch.success(),
        "a declaration naming an instance the launch does not deploy must fail:\n{}",
        launch.text
    );
    assert!(
        launch.text.contains(&format!(
            "clock domain `{FLEET_CLOCK_DOMAIN}` names `{FLEET_CLOCK_SOURCE_INSTANCE}` as its \
             publisher, which this launch does not deploy"
        )),
        "the refusal must name the domain and the publisher it is missing:\n{}",
        launch.text
    );

    assert_fleet_started_nothing(&fleet).await;
}

/// A copy that joins after the stack is up binds the domain the stack is
/// already publishing: the lifetime it finds running, not one of its own, and
/// its probe converges on the instants every other reader sees. The publisher
/// outlives every copy that read it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn copies_joining_late_bind_the_stacks_running_domain() {
    let fleet = start_fleet("peppy-named-clock").await;
    require_success(
        fleet
            .coordinator
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_CLOCK_LAUNCHER_FILE),
            ])
            .await,
        "launch clock source",
    );
    let running = domain_incarnation(
        &wait_for_ticking_domain(&fleet, FLEET_COORDINATOR_CORE_NODE).await,
        FLEET_COORDINATOR_CORE_NODE,
    );
    let source_pid = require_success(
        fleet
            .coordinator
            .exec(vec!["pgrep", "-f", "sim_clock_source"])
            .await,
        "clock source PID",
    );
    for (name, host, daemon) in [
        ("alpha", FLEET_STATION_A_CORE_NODE, &fleet.station_a),
        ("bravo", FLEET_STATION_B_CORE_NODE, &fleet.station_b),
    ] {
        require_success(
            fleet
                .coordinator
                .peppy(&["stack", "join", "probe", "-i", name, "--place", host])
                .await,
            "join late clock consumer",
        );
        assert_probe_capped_at_final(daemon, &format!("{name}_probe_inst")).await;
    }
    assert_eq!(
        running,
        domain_incarnation(
            &wait_for_ticking_domain(&fleet, FLEET_COORDINATOR_CORE_NODE).await,
            FLEET_COORDINATOR_CORE_NODE,
        ),
        "a join reads the lifetime the stack is already publishing"
    );
    require_success(
        fleet.coordinator.peppy(&["stack", "remove", "alpha"]).await,
        "remove clock consumer",
    );
    assert_probe_capped_at_final(&fleet.station_b, "bravo_probe_inst").await;
    require_success(
        fleet.coordinator.peppy(&["stack", "remove", "bravo"]).await,
        "remove final consumer",
    );
    assert_eq!(
        source_pid,
        require_success(
            fleet
                .coordinator
                .exec(vec!["pgrep", "-f", "sim_clock_source"])
                .await,
            "clock source stays running"
        )
    );
    require_success(
        fleet
            .coordinator
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset clock fleet",
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_join_and_remove_with_an_offline_neighbor() {
    let federation = start_federation("peppy-named-offline").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_FLEET_LAUNCHER_FILE),
            ])
            .await,
        "launch shared node",
    );
    for (name, host) in [
        ("alpha", &federation.robot_core_node),
        ("bravo", &federation.cloud_core_node),
    ] {
        require_success(
            federation
                .robot
                .peppy(&["stack", "join", "arm", "-i", name, "--place", host])
                .await,
            "join copy",
        );
    }
    federation.cloud.stop().await;
    require_success(
        federation
            .robot
            .peppy(&["stack", "join", "arm", "-i", "charlie"])
            .await,
        "join on a healthy host while an unrelated neighbor is offline",
    );
    require_success(
        federation
            .robot
            .peppy(&["stack", "remove", "charlie"])
            .await,
        "remove the newly joined member",
    );
    require_success(
        federation.robot.peppy(&["stack", "remove", "alpha"]).await,
        "remove healthy member while its neighbor is offline",
    );
    let removed = federation.robot.peppy(&["stack", "remove", "bravo"]).await;
    assert!(removed.success(), "{}", removed.text);
    assert!(
        removed
            .text
            .contains(&format!("`{}` is not live", federation.cloud_core_node)),
        "{}",
        removed.text
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the copy on the offline host leaves the record",
    );
    let coordinator = coordinator_section(&listed, &federation.robot_core_node);
    assert!(copy_names(&coordinator).is_empty(), "{listed}");
    let instances: Vec<_> = coordinator["stack"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|node| node["instances"].as_array().unwrap())
        .map(|instance| instance["instance_id"].as_str().unwrap().to_owned())
        .collect();
    assert!(instances.contains(&"shared_inst".to_owned()));
    assert!(!instances.contains(&"alpha_arm_inst".to_owned()));
}

/// A machine that coordinates its own stack refuses to take a slice of
/// another launch, and the join that asked leaves that stack alone: the
/// refusal is reported, and the machine keeps the launch it coordinates.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_refused_by_a_machine_with_its_own_stack_leaves_that_stack_alone() {
    let federation = start_federation("peppy-refused-slice").await;
    require_success(
        federation
            .cloud
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(COPIES_ONLY_LAUNCHER_FILE),
            ])
            .await,
        "launch the copies-only launcher on cloud",
    );
    let own_launch =
        |listed: &str| coordinator_section(listed, &federation.cloud_core_node)["launch"].clone();
    let before = own_launch(&require_success(
        federation.cloud.peppy(&["stack", "list", "--json"]).await,
        "cloud's launch before the refused join",
    ));
    assert_eq!(before["coordinator_core_node"], federation.cloud_core_node);
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_FLEET_LAUNCHER_FILE),
            ])
            .await,
        "launch shared node on robot",
    );
    let refused = federation
        .robot
        .peppy(&[
            "stack",
            "join",
            "arm",
            "-i",
            "alpha",
            "--place",
            &federation.cloud_core_node,
        ])
        .await;
    assert!(!refused.success(), "{}", refused.text);
    assert!(refused.text.contains("different stack"), "{}", refused.text);
    assert!(
        !refused.text.contains("Clearing the slice"),
        "{}",
        refused.text
    );
    let after = own_launch(&require_success(
        federation.cloud.peppy(&["stack", "list", "--json"]).await,
        "cloud's launch after the refused join",
    ));
    assert_eq!(
        after, before,
        "the refused join must leave cloud's launch as it was"
    );
}

/// A copy whose run fails on a peer that already holds part of the launch
/// leaves nothing behind there: the node the join added is removed under
/// the launch's reservation, the copy is not listed, and the name is free
/// to join again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_remote_join_removes_the_node_it_added_from_the_peer() {
    let federation = start_federation("peppy-two-node-fleet").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_FLEET_TWO_NODES_LAUNCHER_FILE),
            ])
            .await,
        "launch shared node",
    );
    let place_on_cloud = federation.cloud_core_node.clone();
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "join",
                "arm",
                "-i",
                "bravo",
                "--place",
                &federation.cloud_core_node,
            ])
            .await,
        "join remote arm",
    );
    plant_uncreatable_instance_dir(&federation.cloud, "failed_cam_inst").await;
    let failed = federation
        .robot
        .peppy(&[
            "stack",
            "join",
            "cam",
            "-i",
            "failed",
            "--place",
            &place_on_cloud,
        ])
        .await;
    assert!(!failed.success(), "{}", failed.text);
    assert!(failed.text.contains("failed_cam_inst"), "{}", failed.text);
    assert!(!failed.text.contains("cleanup failed"), "{}", failed.text);
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack after failure",
    );
    assert_holds_exactly(
        &remote,
        &federation.cloud_core_node,
        &["bravo_arm_inst"],
        &["failed_cam_inst"],
    );
    assert!(
        !remote.contains("uvc_camera_python_mock"),
        "the peer keeps no node the failed join added:\n{remote}"
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata",
    );
    assert!(!listed.contains("\"failed\""), "{listed}");
    clear_planted_instance_dir(&federation.cloud, "failed_cam_inst").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "join",
                "cam",
                "-i",
                "failed",
                "--place",
                &place_on_cloud,
            ])
            .await,
        "rejoin under the freed name",
    );
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack after rejoin",
    );
    assert_holds_exactly(
        &remote,
        &federation.cloud_core_node,
        &["bravo_arm_inst", "failed_cam_inst"],
        &["shared_inst"],
    );
}

/// A join whose work on a peer outlives the join's deadline leaves nothing
/// there: the peer is told to cancel, and the rollback removes what the
/// peer still holds for the add.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_whose_peer_work_outlives_its_deadline_leaves_nothing_on_the_peer() {
    let federation = start_federation("peppy-deadline-join").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_FLEET_TWO_NODES_LAUNCHER_FILE),
            ])
            .await,
        "launch shared node",
    );
    let expired = federation
        .robot
        .peppy(&[
            "stack",
            "join",
            "cam",
            "-i",
            "late",
            "--place",
            &federation.cloud_core_node,
            "--max-timeout-secs",
            "5",
        ])
        .await;
    assert!(!expired.success(), "{}", expired.text);
    assert!(expired.text.contains("max timeout"), "{}", expired.text);
    assert!(!expired.text.contains("cleanup failed"), "{}", expired.text);
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack after the deadline",
    );
    assert!(
        !remote.contains("uvc_camera_python_mock"),
        "the peer keeps no node the expired join added:\n{remote}"
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata",
    );
    assert!(
        copy_names(&coordinator_section(&listed, &federation.robot_core_node)).is_empty(),
        "{listed}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_join_and_remove_across_daemons_with_failure_isolation() {
    let federation = start_federation("peppy-named-fleet").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(NAMED_FLEET_LAUNCHER_FILE),
            ])
            .await,
        "launch shared node",
    );
    require_success(
        federation
            .robot
            .peppy(&["stack", "join", "arm", "-i", "alpha"])
            .await,
        "join local alpha",
    );
    for name in ["bravo", "charlie"] {
        require_success(
            federation
                .robot
                .peppy(&[
                    "stack",
                    "join",
                    "arm",
                    "-i",
                    name,
                    "--place",
                    &federation.cloud_core_node,
                ])
                .await,
            "join remote arm",
        );
    }
    let local = require_success(
        federation
            .robot
            .stack_list(Some(&federation.robot_core_node))
            .await,
        "local stack",
    );
    assert_holds_exactly(
        &local,
        &federation.robot_core_node,
        &["shared_inst", "alpha_arm_inst"],
        &["bravo_arm_inst", "charlie_arm_inst"],
    );
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack",
    );
    assert_holds_exactly(
        &remote,
        &federation.cloud_core_node,
        &["bravo_arm_inst", "charlie_arm_inst"],
        &["shared_inst", "alpha_arm_inst"],
    );

    let local_pids = require_success(
        federation
            .robot
            .exec(vec!["pgrep", "-f", "my_python_robot_arm"])
            .await,
        "local process IDs",
    );
    let remote_pids = require_success(
        federation
            .cloud
            .exec(vec!["pgrep", "-f", "my_python_robot_arm"])
            .await,
        "remote process IDs",
    );
    // A file at the instance directory makes run preparation fail on this peer.
    plant_uncreatable_instance_dir(&federation.cloud, "failed_arm_inst").await;
    let failed = federation
        .robot
        .peppy(&[
            "stack",
            "join",
            "arm",
            "-i",
            "failed",
            "--place",
            &federation.cloud_core_node,
        ])
        .await;
    assert!(!failed.success(), "{}", failed.text);
    assert_eq!(
        local_pids,
        require_success(
            federation
                .robot
                .exec(vec!["pgrep", "-f", "my_python_robot_arm"])
                .await,
            "local processes survive failure"
        )
    );
    assert_eq!(
        remote_pids,
        require_success(
            federation
                .cloud
                .exec(vec!["pgrep", "-f", "my_python_robot_arm"])
                .await,
            "remote processes survive failure"
        )
    );
    assert!(failed.text.contains("failed_arm_inst"), "{}", failed.text);
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack after failure",
    );
    assert_holds_exactly(
        &remote,
        &federation.cloud_core_node,
        &["bravo_arm_inst", "charlie_arm_inst"],
        &["failed_arm_inst"],
    );
    clear_planted_instance_dir(&federation.cloud, "failed_arm_inst").await;

    require_success(
        federation.robot.peppy(&["stack", "remove", "bravo"]).await,
        "remove remote bravo",
    );
    let remote = require_success(
        federation
            .cloud
            .stack_list(Some(&federation.cloud_core_node))
            .await,
        "remote stack after removal",
    );
    assert_holds_exactly(
        &remote,
        &federation.cloud_core_node,
        &["charlie_arm_inst"],
        &["bravo_arm_inst"],
    );
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "join",
                "arm",
                "-i",
                "bravo",
                "--place",
                &federation.cloud_core_node,
            ])
            .await,
        "rejoin remote bravo",
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata",
    );
    assert_eq!(
        copy_names(&coordinator_section(&listed, &federation.robot_core_node)),
        ["alpha", "bravo", "charlie"]
    );
    for name in ["alpha", "bravo", "charlie"] {
        require_success(
            federation.robot.peppy(&["stack", "remove", name]).await,
            "remove copy",
        );
    }
    let local = require_success(
        federation
            .robot
            .stack_list(Some(&federation.robot_core_node))
            .await,
        "shared node survives",
    );
    assert_holds_exactly(
        &local,
        &federation.robot_core_node,
        &["shared_inst"],
        &["alpha_arm_inst", "bravo_arm_inst", "charlie_arm_inst"],
    );
    require_success(
        federation
            .robot
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset fleet",
    );
}

/// The gates the launch starts: one beside the coordinator, one placed on
/// the peer through the `peer` core node label. A round re-runs the gate of
/// the daemon whose copies it moves on. Neither name contains the other, so
/// one gate's lines never read as the other's.
const COORDINATOR_GATE: &str = "coordinator_gate_inst";
const PEER_GATE: &str = "peer_gate_inst";
const PEER_LABEL: &str = "peer";
const GATE_NODE: &str = "isolation_gate";
const GATE_TAG: &str = "v1";

/// The three goals of a round, by the suffix their tokens carry: one the arm
/// completes when its hold is up, one the commander cancels itself, and one
/// the arm holds until the gate moves it on.
const TIMED_SUFFIX: &str = "";
const SELF_CANCELLED_SUFFIX: &str = "c";
const HELD_SUFFIX: &str = "h";

/// The copy the launch deploys, and the one the test removes and joins
/// again; the rest join the running stack once.
const LAUNCHED_STATION: &str = "alpha";
const REJOINED_STATION: &str = "bravo";

/// The instances one copy owns, under the name the copy carries.
fn arm_of(copy: &str) -> String {
    format!("{copy}_arm_inst")
}

fn commander_of(copy: &str) -> String {
    format!("{copy}_commander_inst")
}

/// Where a station copy runs: which daemon holds its instances, which word
/// its join places it with, and which gate reaches its commander.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Placement {
    Coordinator,
    Peer,
}

impl Placement {
    fn daemon<'a>(&self, federation: &'a Federation) -> &'a Daemon {
        match self {
            Placement::Coordinator => &federation.robot,
            Placement::Peer => &federation.cloud,
        }
    }

    fn core_node<'a>(&self, federation: &'a Federation) -> Option<&'a str> {
        match self {
            Placement::Coordinator => None,
            Placement::Peer => Some(&federation.cloud_core_node),
        }
    }

    fn gate(&self) -> &'static str {
        match self {
            Placement::Coordinator => COORDINATOR_GATE,
            Placement::Peer => PEER_GATE,
        }
    }
}

/// One station copy: its name on the stack, how many times it has joined,
/// where it runs, and the round its commander is on.
struct Station {
    name: &'static str,
    /// Every join takes the next generation, and the generation is what the
    /// commander's label carries, so the tokens of a copy's second life
    /// carry a prefix its first life never sent.
    generation: usize,
    placement: Placement,
    /// The round a fresh commander is on is 1, as its own counter starts there.
    round: usize,
}

impl Station {
    fn new(name: &'static str, placement: Placement) -> Self {
        Station {
            name,
            generation: 1,
            placement,
            round: 1,
        }
    }

    fn arm(&self) -> String {
        arm_of(self.name)
    }

    fn commander(&self) -> String {
        commander_of(self.name)
    }

    /// The prefix this copy's commander puts on every token it sends.
    fn label(&self) -> String {
        format!("{}{}", self.name, self.generation)
    }

    fn token(&self, goal: &str) -> String {
        format!("{}-{}{goal}", self.label(), self.round)
    }

    fn daemon<'a>(&self, federation: &'a Federation) -> &'a Daemon {
        self.placement.daemon(federation)
    }

    fn gate(&self) -> &'static str {
        self.placement.gate()
    }

    /// This copy's current round, as the audit reads it.
    fn current_round(&self, held: HeldOutcome, heartbeat_floor: u32) -> Round {
        Round {
            copy: self.name.to_owned(),
            label: self.label(),
            gate: self.gate().to_owned(),
            number: self.round,
            held,
            heartbeat_floor,
        }
    }

    /// Takes the copy into its next life: a new label, and a commander whose
    /// round counter starts over.
    fn rejoined(&mut self) {
        self.generation += 1;
        self.round = 1;
    }
}

/// What a round's held goal came to.
#[derive(Clone, Copy, Debug)]
enum HeldOutcome {
    Cancelled,
    Released,
}

/// One copy's round, as the audit reads it.
struct Round {
    copy: String,
    label: String,
    gate: String,
    number: usize,
    held: HeldOutcome,
    /// The `seq` this copy's commander had heard when the gate phase opened;
    /// the audit demands a later one, so a round proves its own heartbeat.
    heartbeat_floor: u32,
}

impl Round {
    fn arm(&self) -> String {
        arm_of(&self.copy)
    }

    fn commander(&self) -> String {
        commander_of(&self.copy)
    }

    fn token(&self, goal: &str) -> String {
        format!("{}-{}{goal}", self.label, self.number)
    }
}

/// The line a copy's commander logs when its held goal ends.
fn held_result_line(round: &Round) -> String {
    let held = round.token(HELD_SUFFIX);
    let status = match round.held {
        HeldOutcome::Cancelled => "CANCELLED",
        HeldOutcome::Released => "COMPLETED",
    };
    format!("goal {held} {status} by {} token={held}\n", round.arm())
}

/// Every line `round` leaves in its commander's log, and every line it
/// leaves in its arm's, in the order they are logged. Each line ends at its
/// newline, so one goal's token never matches another's.
fn round_lines(round: &Round) -> (Vec<String>, Vec<String>) {
    let arm = round.arm();
    let commander = round.commander();
    let timed = round.token(TIMED_SUFFIX);
    let self_cancelled = round.token(SELF_CANCELLED_SUFFIX);
    let held = round.token(HELD_SUFFIX);
    let mut commander_lines = vec![
        format!("echo answered by {arm} token={timed}\n"),
        format!("feedback token={timed} from {arm}\n"),
        format!("goal {timed} COMPLETED by {arm} token={timed}\n"),
        format!("feedback token={self_cancelled} from {arm}\n"),
        format!("cancel {self_cancelled} SIGNALLED by {arm}\n"),
        format!("goal {self_cancelled} CANCELLED by {arm} token={self_cancelled}\n"),
        format!("feedback token={held} from {arm}\n"),
        format!("holding {held}\n"),
    ];
    let mut arm_lines = vec![
        format!("echo from {commander} token={timed} answered by {arm}\n"),
        format!("goal {timed} from {commander} accepted by {arm}\n"),
        format!("goal {timed} completed by {arm}\n"),
        format!("goal {self_cancelled} from {commander} accepted by {arm}\n"),
        format!("goal {self_cancelled} cancelled by {arm}\n"),
        format!("goal {held} from {commander} accepted by {arm}\n"),
    ];
    match round.held {
        HeldOutcome::Cancelled => {
            commander_lines.push(format!("move_on cancel from {} for {held}\n", round.gate));
            commander_lines.push(format!("cancel {held} SIGNALLED by {arm}\n"));
            commander_lines.push(held_result_line(round));
            arm_lines.push(format!("goal {held} cancelled by {arm}\n"));
        }
        HeldOutcome::Released => {
            commander_lines.push(format!("move_on release from {} for {held}\n", round.gate));
            commander_lines.push(format!("release {held} relayed to {arm}\n"));
            commander_lines.push(held_result_line(round));
            arm_lines.push(format!("release {held} from {commander} to {arm}\n"));
            arm_lines.push(format!("goal {held} completed by {arm}\n"));
        }
    }
    (commander_lines, arm_lines)
}

/// The highest `seq` this log carries from `arm`'s heartbeat, 0 when it
/// carries none.
fn highest_heartbeat_seq(log: &str, arm: &str) -> u32 {
    let marker = format!("heartbeat from {arm} seq=");
    log.match_indices(&marker)
        .filter_map(|(at, _)| log[at + marker.len()..].lines().next())
        .filter_map(|seq| seq.trim().parse::<u32>().ok())
        .max()
        .unwrap_or(0)
}

/// The lines that would show another copy's traffic in `copy`'s logs: the
/// instance ids of every other copy and of the gate that reaches them, and
/// the token prefix of every label the stack has carried, the retired labels
/// of `copy` itself included.
fn foreign_markers(stations: &[Station], retired: &[String], copy: &str) -> Vec<String> {
    let token_markers = |label: &str| {
        [
            format!("token={label}-"),
            format!("goal {label}-"),
            format!("holding {label}-"),
            format!("release {label}-"),
            format!("cancel {label}-"),
            format!("for {label}-"),
        ]
    };
    let own_gate = stations
        .iter()
        .find(|station| station.name == copy)
        .map(|station| station.gate());
    let mut markers: BTreeSet<String> = stations
        .iter()
        .filter(|station| station.name != copy)
        .flat_map(|station| {
            [arm_of(station.name), commander_of(station.name)]
                .into_iter()
                .chain(token_markers(&station.label()))
        })
        .collect();
    markers.extend(
        [COORDINATOR_GATE, PEER_GATE]
            .into_iter()
            .filter(|gate| Some(*gate) != own_gate)
            .map(str::to_owned),
    );
    markers.extend(retired.iter().flat_map(|label| token_markers(label)));
    markers.into_iter().collect()
}

/// What `round` left undone or let in: every line the round owes that its
/// logs do not carry, a heartbeat no later than the round's floor, and every
/// foreign marker either log carries. An empty list is the round proven to
/// have run inside its copy.
fn copy_traffic_violations(
    round: &Round,
    foreign: &[String],
    commander_log: &str,
    arm_log: &str,
) -> Vec<String> {
    let commander = round.commander();
    let arm = round.arm();
    let (commander_lines, arm_lines) = round_lines(round);
    let mut violations: Vec<String> = commander_lines
        .iter()
        .filter(|line| !commander_log.contains(line.as_str()))
        .map(|line| format!("`{commander}` never logged `{}`", line.trim_end()))
        .chain(
            arm_lines
                .iter()
                .filter(|line| !arm_log.contains(line.as_str()))
                .map(|line| format!("`{arm}` never logged `{}`", line.trim_end())),
        )
        .collect();
    let seq = highest_heartbeat_seq(commander_log, &arm);
    if seq <= round.heartbeat_floor {
        violations.push(format!(
            "`{commander}` heard no heartbeat from `{arm}` after seq {} (highest is {seq})",
            round.heartbeat_floor
        ));
    }
    violations.extend(foreign_violations(
        &round.copy,
        foreign,
        commander_log,
        arm_log,
    ));
    violations
}

/// The foreign markers either of a copy's logs carries.
fn foreign_violations(
    copy: &str,
    foreign: &[String],
    commander_log: &str,
    arm_log: &str,
) -> Vec<String> {
    foreign
        .iter()
        .filter(|marker| {
            commander_log.contains(marker.as_str()) || arm_log.contains(marker.as_str())
        })
        .map(|marker| format!("`{copy}` handled foreign traffic (`{marker}`)"))
        .collect()
}

/// Reads a copy's two logs until its round is complete and clean, or until
/// `TIMEOUT` leaves it the violations the round still has. Re-reading is what
/// keeps the audit independent of the order the commander's two coroutines
/// reach their logs in.
async fn audit_round(daemon: &Daemon, round: &Round, foreign: &[String]) -> Result<(), String> {
    let started = Instant::now();
    loop {
        let commander_log = daemon.node_log(&round.commander()).await;
        let arm_log = daemon.node_log(&round.arm()).await;
        let violations = copy_traffic_violations(round, foreign, &commander_log, &arm_log);
        if violations.is_empty() {
            return Ok(());
        }
        if started.elapsed() >= TIMEOUT {
            return Err(format!(
                "copy `{}`, round {}:\n{}\n\n{} log:\n{commander_log}\n{} log:\n{arm_log}",
                round.copy,
                round.number,
                violations.join("\n"),
                round.commander(),
                round.arm()
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

/// A peer's logs at the moment another copy's held goal is cancelled: its
/// own goal still held, its commander with no outcome for it yet, and its
/// arm with no trace of the cancelled copy's token.
fn peer_hold_violations(
    held_token: &str,
    cancelled_token: &str,
    commander_log: &str,
    arm_log: &str,
) -> Vec<String> {
    let mut violations = Vec::new();
    if !commander_log.contains(&format!("holding {held_token}\n")) {
        violations.push(format!("never held `{held_token}`"));
    }
    if commander_log.contains(&format!("goal {held_token} ")) {
        violations.push(format!("stopped holding `{held_token}`"));
    }
    if arm_log.contains(cancelled_token) {
        violations.push(format!("its arm saw `{cancelled_token}`"));
    }
    violations
}

/// Joins one station copy, labelling its commander's payloads with the
/// copy's label so the arm's log tells one copy's requests from another's,
/// and one join's from the next's.
async fn join_station(federation: &Federation, station: &Station) {
    let label = format!("commander_inst.label=\"{}\"", station.label());
    let mut args = vec![
        "stack",
        "join",
        "station",
        "-i",
        station.name,
        "--set-arguments",
        &label,
    ];
    if let Some(core_node) = station.placement.core_node(federation) {
        args.extend(["--place", core_node]);
    }
    require_success(
        federation.robot.peppy(&args).await,
        &format!("join station {}", station.name),
    );
}

/// Every copy's two logs and both gates', for a failure that one log cannot
/// explain.
async fn station_logs(federation: &Federation, stations: &[Station]) -> String {
    let mut dump = String::new();
    for station in stations {
        let daemon = station.daemon(federation);
        for instance in [station.commander(), station.arm()] {
            let log = daemon.node_log(&instance).await;
            dump.push_str(&format!("--- {instance} ---\n{log}\n"));
        }
    }
    for placement in [Placement::Coordinator, Placement::Peer] {
        let gate = placement.gate();
        let log = placement.daemon(federation).node_log(gate).await;
        dump.push_str(&format!("--- {gate} ---\n{log}\n"));
    }
    dump
}

/// Waits for `marker` in `instance`'s log on `daemon`, dumping every copy's
/// logs when it never arrives.
async fn wait_for_marker(
    federation: &Federation,
    stations: &[Station],
    daemon: &Daemon,
    instance: &str,
    marker: &str,
) -> String {
    if let Some(log) = daemon.find_node_log(instance, marker).await {
        return log;
    }
    let dump = station_logs(federation, stations).await;
    panic!("timed out waiting for `{marker}` in `{instance}`:\n{dump}");
}

/// Re-runs one daemon's gate linked to the given copies' commanders with
/// `verb`, `cancel` or `release`, and waits for the gate to log the number
/// of commanders it bound and then each one's answer: the token of the goal
/// it moved on. The count is what holds a gate to the copies it was linked
/// to, since a copy beside it on the same daemon is not foreign to it.
async fn move_held_goals_on(
    federation: &Federation,
    stations: &[Station],
    placement: Placement,
    verb: &str,
    moving: &[&Station],
) {
    let daemon = placement.daemon(federation);
    let gate = placement.gate();
    require_success(
        daemon.peppy(&["node", "stop", gate]).await,
        &format!("stop {gate}"),
    );
    let links = moving
        .iter()
        .map(|station| format!("commander@{}", station.commander()))
        .collect::<Vec<_>>()
        .join(",");
    let node_reference = format!("{GATE_NODE}:{GATE_TAG}");
    let verb_argument = format!("verb={verb}");
    require_success(
        daemon
            .peppy(&[
                "node",
                "run",
                &node_reference,
                "-i",
                gate,
                "--link",
                &links,
                &verb_argument,
            ])
            .await,
        &format!("run {gate} with {verb_argument}"),
    );
    let bound = format!("up, verb {verb}, {} commander(s)\n", moving.len());
    wait_for_marker(federation, stations, daemon, gate, &bound).await;
    for station in moving {
        let answer = format!(
            "{verb} {} token={}\n",
            station.commander(),
            station.token(HELD_SUFFIX)
        );
        wait_for_marker(federation, stations, daemon, gate, &answer).await;
    }
}

/// One gate beside the coordinator and one placed on the peer through the
/// `peer` label, each with an empty `zero_or_more` commander slot that every
/// station copy adds its own commander to.
fn growing_gates_launcher() -> String {
    format!(
        r#"{{
  peppy_schema: "launcher/v1",
  core_nodes: ["{PEER_LABEL}"],
  deployments: [
    {{ source: {{ name: "{GATE_NODE}", tag: "{GATE_TAG}" }},
      instances: [
        {{ instance_id: "{COORDINATOR_GATE}", arguments: {{ verb: "release" }} }},
        {{ instance_id: "{PEER_GATE}", core_node: "{PEER_LABEL}", arguments: {{ verb: "release" }} }}
      ] }}
  ],
  components: [{{ name: "robot", cardinality: "zero_or_more", options: {{
    station: {{
      deployments: [
        {{ source: {{ name: "isolation_arm", tag: "v1" }},
          instances: [{{ instance_id: "arm_inst" }}] }},
        {{ source: {{ name: "isolation_commander", tag: "v1" }},
          instances: [{{ instance_id: "commander_inst", links: {{ arm: "arm_inst" }} }}] }}
      ],
      adjustments: [
        {{ target: "{COORDINATOR_GATE}", add_links: {{ commander: ["commander_inst"] }} }},
        {{ target: "{PEER_GATE}", add_links: {{ commander: ["commander_inst"] }} }}
      ]
    }}
  }} }}]
}}"#
    )
}

/// A hub every leader pairs into, a consuming instance on the coordinator and
/// an observing instance on the peer, both `set_watch` with empty
/// `zero_or_more` slots, and a `fleet` axis whose copies bring a leader and an
/// arm and add them to the two watches' sets.
fn set_watch_launcher() -> String {
    format!(
        r#"{{
  peppy_schema: "launcher/v1",
  core_nodes: ["{PEER_LABEL}"],
  deployments: [
    {{ source: {{ name: "pairing_hub", tag: "v1" }}, instances: [{{ instance_id: "hub_inst" }}] }},
    {{ source: {{ name: "my_python_robot_arm", tag: "v1" }},
      instances: [{{ instance_id: "fixed_arm_inst" }}] }},
    {{ source: {{ name: "set_watch", tag: "v1" }}, instances: [
      {{ instance_id: "consumer_inst", links: {{ arms: ["fixed_arm_inst"] }} }},
      {{ instance_id: "observer_inst", core_node: "{PEER_LABEL}", links: {{ arms: ["fixed_arm_inst"] }} }}
    ] }}
  ],
  components: [{{ name: "fleet", cardinality: "zero_or_more", options: {{
    member: {{
      deployments: [
        {{ source: {{ name: "pairing_leader", tag: "v1" }},
          instances: [{{ instance_id: "leader_inst", arguments: {{ value: 1.0 }}, links: {{ hub: "hub_inst" }} }}] }},
        {{ source: {{ name: "my_python_robot_arm", tag: "v1" }},
          instances: [{{ instance_id: "arm_inst" }}] }}
      ],
      adjustments: [
        {{ target: "consumer_inst",
           add_links: {{ arms: ["arm_inst"], leaders: ["leader_inst/hub"] }} }},
        {{ target: "observer_inst",
           add_links: {{ arms: ["arm_inst"], leaders: ["leader_inst/hub"] }} }}
      ]
    }}
  }} }}]
}}"#
    )
}

/// How many times `marker` appears in `instance_id`'s log on `daemon`, polled
/// until it reaches `count`.
async fn wait_for_log_count(daemon: &Daemon, instance_id: &str, marker: &str, count: usize) {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if daemon.node_log(instance_id).await.matches(marker).count() >= count {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{instance_id} logged `{marker}` fewer than {count} times");
}

/// The members slot `link_id` of `instance_id` holds, as a `stack list --json`
/// section records them, each as `instance@core_node` in plan order followed
/// by ` [copy NAME/IN_COPY]` for a member a copy brought: the copy's name and
/// the id its fragment wrote for the instance.
fn recorded_bindings(section: &serde_json::Value, instance_id: &str, link_id: &str) -> Vec<String> {
    let instance = section["stack"]["nodes"]
        .as_array()
        .unwrap_or_else(|| panic!("a section lists its stack: {section}"))
        .iter()
        .flat_map(|node| node["instances"].as_array().into_iter().flatten())
        .find(|instance| instance["instance_id"] == instance_id)
        .unwrap_or_else(|| panic!("`{instance_id}` is in the section's stack: {section}"));
    instance["slot_bindings"][link_id]
        .as_array()
        .into_iter()
        .flatten()
        .map(|member| {
            let copy = member["copy"]["name"]
                .as_str()
                .map(|name| {
                    let in_copy = member["copy"]["instance_id"]
                        .as_str()
                        .expect("a member of a copy names the id inside it");
                    format!(" [copy {name}/{in_copy}]")
                })
                .unwrap_or_default();
            format!(
                "{}@{}{copy}",
                member["producer"]["instance_id"]
                    .as_str()
                    .expect("a member names its producer's instance"),
                member["producer"]["core_node"]
                    .as_str()
                    .expect("a member names its producer's core node")
            )
        })
        .collect()
}

/// The instances each copy owns, as a `stack list --json` section lists them.
fn copy_instances(section: &serde_json::Value) -> BTreeMap<String, serde_json::Value> {
    section["copies"]
        .as_array()
        .unwrap_or_else(|| panic!("a section lists its copies: {section}"))
        .iter()
        .map(|copy| {
            (
                copy["name"].as_str().expect("a copy is named").to_owned(),
                copy["instance_ids"].clone(),
            )
        })
        .collect()
}

/// One gated round across every copy: it opens by recording the heartbeat
/// each commander has heard, waits for every copy to hold its own goal,
/// which is the moment all three have traffic in flight at once, cancels
/// `cancelled`'s through that copy's gate, checks at that point that every
/// peer still holds its own and that no peer's arm saw the cancelled token,
/// releases the peers, and audits each copy's logs for the round. Every
/// copy's round then advances.
async fn exercise_gated_round(
    federation: &Federation,
    stations: &mut [Station],
    cancelled: &str,
    retired: &[String],
) {
    let mut floors = Vec::with_capacity(stations.len());
    for station in stations.iter() {
        let log = station
            .daemon(federation)
            .node_log(&station.commander())
            .await;
        floors.push(highest_heartbeat_seq(&log, &station.arm()));
    }
    for station in stations.iter() {
        let holding = format!("holding {}\n", station.token(HELD_SUFFIX));
        wait_for_marker(
            federation,
            stations,
            station.daemon(federation),
            &station.commander(),
            &holding,
        )
        .await;
    }

    let (named, peers): (Vec<&Station>, Vec<&Station>) = stations
        .iter()
        .partition(|station| station.name == cancelled);
    let [cancelling] = named[..] else {
        panic!("`{cancelled}` is not one of this stack's copies");
    };
    let cancelled_token = cancelling.token(HELD_SUFFIX);
    move_held_goals_on(
        federation,
        stations,
        cancelling.placement,
        "cancel",
        &[cancelling],
    )
    .await;
    wait_for_marker(
        federation,
        stations,
        cancelling.daemon(federation),
        &cancelling.commander(),
        &held_result_line(&cancelling.current_round(HeldOutcome::Cancelled, 0)),
    )
    .await;

    for peer in &peers {
        let daemon = peer.daemon(federation);
        let commander_log = daemon.node_log(&peer.commander()).await;
        let arm_log = daemon.node_log(&peer.arm()).await;
        let violations = peer_hold_violations(
            &peer.token(HELD_SUFFIX),
            &cancelled_token,
            &commander_log,
            &arm_log,
        );
        assert!(
            violations.is_empty(),
            "copy `{}` through `{cancelled_token}`'s cancel: {}\n\n{} log:\n{commander_log}\n{} log:\n{arm_log}",
            peer.name,
            violations.join("; "),
            peer.commander(),
            peer.arm()
        );
    }

    for placement in [Placement::Coordinator, Placement::Peer] {
        let held: Vec<&Station> = peers
            .iter()
            .copied()
            .filter(|peer| peer.placement == placement)
            .collect();
        if !held.is_empty() {
            move_held_goals_on(federation, stations, placement, "release", &held).await;
        }
    }

    for (station, floor) in stations.iter().zip(&floors) {
        let outcome = if station.name == cancelled {
            HeldOutcome::Cancelled
        } else {
            HeldOutcome::Released
        };
        let foreign = foreign_markers(stations, retired, station.name);
        audit_round(
            station.daemon(federation),
            &station.current_round(outcome, *floor),
            &foreign,
        )
        .await
        .unwrap_or_else(|report| panic!("{report}"));
    }
    for station in stations.iter_mut() {
        station.round += 1;
    }
}

/// Reads every copy's logs one last time: nothing foreign reached any of
/// them after the round each was last audited on.
async fn assert_no_foreign_traffic(
    federation: &Federation,
    stations: &[Station],
    retired: &[String],
) {
    for station in stations {
        let daemon = station.daemon(federation);
        let commander_log = daemon.node_log(&station.commander()).await;
        let arm_log = daemon.node_log(&station.arm()).await;
        let foreign = foreign_markers(stations, retired, station.name);
        let violations = foreign_violations(station.name, &foreign, &commander_log, &arm_log);
        assert!(
            violations.is_empty(),
            "{}\n\n{} log:\n{commander_log}\n{} log:\n{arm_log}",
            violations.join("\n"),
            station.commander(),
            station.arm()
        );
    }
}

/// Three copies of one station, one beside the coordinator and two on the
/// peer, each exchanging topic samples, service requests and action goals
/// (completed, cancelled and held for the gate) at the same time, with
/// every payload labelled by its copy. One copy's held goal is cancelled
/// while its peers hold theirs, the peers then complete normally, and each
/// copy hears only its own instances and its own labels, before and after
/// one of the peer's copies is removed and joined again. The rejoined copy
/// sends a label its first life never sent and is the copy cancelled in the
/// second pass; its peers keep the instances they came up with, and the
/// round each of their tokens names is what proves their commanders never
/// restarted.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_exchange_topics_services_and_actions_only_within_themselves() {
    let federation = start_federation("peppy-isolation").await;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                &format!("{PEER_LABEL}@{}", federation.cloud_core_node),
                &container_launcher(ISOLATION_FLEET_LAUNCHER_FILE),
            ])
            .await,
        "launch station fleet",
    );
    let mut stations = [
        Station::new(LAUNCHED_STATION, Placement::Coordinator),
        Station::new(REJOINED_STATION, Placement::Peer),
        Station::new("charlie", Placement::Peer),
    ];
    for station in stations.iter().filter(|s| s.name != LAUNCHED_STATION) {
        join_station(&federation, station).await;
    }
    let mut retired: Vec<String> = Vec::new();
    exercise_gated_round(&federation, &mut stations, LAUNCHED_STATION, &retired).await;

    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata before the removal",
    );
    let before = copy_instances(&coordinator_section(&listed, &federation.robot_core_node));
    require_success(
        federation
            .robot
            .peppy(&["stack", "remove", REJOINED_STATION])
            .await,
        "remove the copy the test rejoins",
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata after the removal",
    );
    assert!(
        !copy_names(&coordinator_section(&listed, &federation.robot_core_node))
            .contains(&REJOINED_STATION.to_owned()),
        "`{REJOINED_STATION}` is still on the stack after its removal:\n{listed}"
    );

    let rejoining = stations
        .iter_mut()
        .find(|station| station.name == REJOINED_STATION)
        .expect("the copy the test rejoins is one of this stack's copies");
    retired.push(rejoining.label());
    rejoining.rejoined();
    join_station(&federation, rejoining).await;
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "copy metadata after the rejoin",
    );
    let after = copy_instances(&coordinator_section(&listed, &federation.robot_core_node));
    for station in stations.iter().filter(|s| s.name != REJOINED_STATION) {
        let copy = station.name;
        assert_eq!(
            before[copy], after[copy],
            "`{copy}` did not keep its instances through `{REJOINED_STATION}`'s removal and rejoin"
        );
        assert!(
            before[copy]
                .as_array()
                .is_some_and(|ids| ids.iter().any(|id| id == &station.commander())),
            "`{copy}` never listed its commander: {}",
            before[copy]
        );
    }
    exercise_gated_round(&federation, &mut stations, REJOINED_STATION, &retired).await;
    assert_no_foreign_traffic(&federation, &stations, &retired).await;

    require_success(
        federation
            .robot
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset station fleet",
    );
}

/// The oracles behind the isolation test, against logs written by hand: a
/// complete round passes, a foreign line in either log fails it even when
/// the copy's own round is complete, a round is never proven by an earlier
/// one, and a peer that stopped holding is caught at the cancel.
mod copy_traffic_oracle {
    use super::{
        COORDINATOR_GATE, HeldOutcome, PEER_GATE, Placement, Round, Station,
        copy_traffic_violations, foreign_markers, highest_heartbeat_seq, peer_hold_violations,
        round_lines,
    };

    /// The copy a label belongs to: a label is the copy's name and the
    /// generation of its join.
    fn copy_of(label: &str) -> String {
        label
            .trim_end_matches(|character: char| character.is_ascii_digit())
            .to_owned()
    }

    fn round(label: &str, number: usize, held: HeldOutcome, placement: Placement) -> Round {
        Round {
            copy: copy_of(label),
            label: label.to_owned(),
            gate: placement.gate().to_owned(),
            number,
            held,
            heartbeat_floor: 0,
        }
    }

    /// The three copies as the live test places them.
    fn stations() -> [Station; 3] {
        [
            Station::new("alpha", Placement::Coordinator),
            Station::new("bravo", Placement::Peer),
            Station::new("charlie", Placement::Peer),
        ]
    }

    /// The commander's and the arm's logs of one round that ran inside its copy.
    fn logs(round: &Round) -> (String, String) {
        let (commander_lines, arm_lines) = round_lines(round);
        (
            format!(
                "heartbeat from {} seq={}\n{}",
                round.arm(),
                round.heartbeat_floor + 1,
                commander_lines.concat()
            ),
            arm_lines.concat(),
        )
    }

    /// The markers the live test hands the oracle for `copy`, from the same
    /// builder, so a marker added there is exercised here.
    fn foreign(copy: &str) -> Vec<String> {
        foreign_markers(&stations(), &[], copy)
    }

    #[test]
    fn a_round_inside_the_copy_has_no_violations() {
        for held in [HeldOutcome::Cancelled, HeldOutcome::Released] {
            let round = round("alpha1", 2, held, Placement::Coordinator);
            let (commander_log, arm_log) = logs(&round);
            assert_eq!(
                copy_traffic_violations(&round, &foreign("alpha"), &commander_log, &arm_log),
                Vec::<String>::new(),
                "{held:?}"
            );
        }
    }

    #[test]
    fn a_foreign_answer_fails_a_complete_round() {
        let round = round("alpha1", 1, HeldOutcome::Released, Placement::Coordinator);
        let (mut commander_log, arm_log) = logs(&round);
        commander_log.push_str("echo answered by bravo_arm_inst token=alpha1-1\n");
        assert_eq!(
            copy_traffic_violations(&round, &foreign("alpha"), &commander_log, &arm_log),
            vec!["`alpha` handled foreign traffic (`bravo_arm_inst`)".to_owned()]
        );
    }

    #[test]
    fn a_foreign_request_handled_by_the_arm_fails_a_complete_round() {
        let round = round("alpha1", 1, HeldOutcome::Cancelled, Placement::Coordinator);
        let (commander_log, mut arm_log) = logs(&round);
        arm_log.push_str("echo from charlie_commander_inst token=charlie1-1\n");
        assert_eq!(
            copy_traffic_violations(&round, &foreign("alpha"), &commander_log, &arm_log),
            vec![
                "`alpha` handled foreign traffic (`charlie_commander_inst`)".to_owned(),
                "`alpha` handled foreign traffic (`token=charlie1-`)".to_owned(),
            ]
        );
    }

    #[test]
    fn a_retired_label_of_the_copy_itself_is_foreign() {
        let round = round("bravo2", 1, HeldOutcome::Cancelled, Placement::Peer);
        let (mut commander_log, arm_log) = logs(&round);
        commander_log.push_str("goal bravo1-4h CANCELLED by bravo_arm_inst token=bravo1-4h\n");
        let markers = foreign_markers(
            &[Station::new("bravo", Placement::Peer)],
            &["bravo1".to_owned()],
            "bravo",
        );
        let violations = copy_traffic_violations(&round, &markers, &commander_log, &arm_log);
        assert_eq!(
            violations,
            vec![
                "`bravo` handled foreign traffic (`goal bravo1-`)".to_owned(),
                "`bravo` handled foreign traffic (`token=bravo1-`)".to_owned(),
            ]
        );
    }

    #[test]
    fn the_held_goal_must_come_to_what_the_round_expects() {
        let released = round("bravo1", 1, HeldOutcome::Released, Placement::Peer);
        let (commander_log, arm_log) = logs(&released);
        let cancelled = round("bravo1", 1, HeldOutcome::Cancelled, Placement::Peer);
        assert_eq!(
            copy_traffic_violations(&cancelled, &[], &commander_log, &arm_log),
            vec![
                format!(
                    "`bravo_commander_inst` never logged `move_on cancel from {PEER_GATE} for bravo1-1h`"
                ),
                "`bravo_commander_inst` never logged `cancel bravo1-1h SIGNALLED by bravo_arm_inst`"
                    .to_owned(),
                "`bravo_commander_inst` never logged `goal bravo1-1h CANCELLED by bravo_arm_inst token=bravo1-1h`"
                    .to_owned(),
                "`bravo_arm_inst` never logged `goal bravo1-1h cancelled by bravo_arm_inst`"
                    .to_owned(),
            ]
        );
    }

    #[test]
    fn a_later_round_is_not_proven_by_an_earlier_one() {
        let first = round("charlie1", 1, HeldOutcome::Released, Placement::Peer);
        let (commander_log, arm_log) = logs(&first);
        let second = round("charlie1", 2, HeldOutcome::Released, Placement::Peer);
        let violations = copy_traffic_violations(&second, &[], &commander_log, &arm_log);
        assert_eq!(
            violations.len(),
            round_lines(&second).0.len() + round_lines(&second).1.len()
        );
        assert!(
            violations
                .iter()
                .all(|violation| violation.contains("charlie1-2")),
            "{violations:?}"
        );
    }

    #[test]
    fn a_round_needs_a_heartbeat_of_its_own() {
        let mut round = round("alpha1", 3, HeldOutcome::Released, Placement::Coordinator);
        let (commander_log, arm_log) = logs(&round);
        round.heartbeat_floor = highest_heartbeat_seq(&commander_log, &round.arm());
        assert_eq!(
            copy_traffic_violations(&round, &[], &commander_log, &arm_log),
            vec![
                "`alpha_commander_inst` heard no heartbeat from `alpha_arm_inst` after seq 1 (highest is 1)"
                    .to_owned()
            ]
        );
    }

    #[test]
    fn neither_gate_name_contains_the_other() {
        assert!(
            !COORDINATOR_GATE.contains(PEER_GATE) && !PEER_GATE.contains(COORDINATOR_GATE),
            "`{COORDINATOR_GATE}` and `{PEER_GATE}` must not read as each other"
        );
    }

    #[test]
    fn every_foreign_marker_matches_a_line_of_the_copy_it_names() {
        let lines: String = ["bravo1", "charlie1"]
            .into_iter()
            .map(|label| {
                let (commander_lines, arm_lines) =
                    round_lines(&round(label, 1, HeldOutcome::Released, Placement::Peer));
                format!("{}{}", commander_lines.concat(), arm_lines.concat())
            })
            .collect();
        for marker in foreign("alpha") {
            assert!(
                lines.contains(&marker),
                "marker `{marker}` matches no line the copies it names log:\n{lines}"
            );
        }
    }

    #[test]
    fn a_peer_that_kept_holding_has_no_violations() {
        assert_eq!(
            peer_hold_violations(
                "charlie1-1h",
                "alpha1-1h",
                "[isolation-commander] holding charlie1-1h\n",
                "[isolation-arm] goal charlie1-1h from charlie_commander_inst accepted by charlie_arm_inst\n"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn a_peer_whose_goal_ended_or_whose_arm_saw_the_cancel_is_caught() {
        assert_eq!(
            peer_hold_violations(
                "charlie1-1h",
                "alpha1-1h",
                "[isolation-commander] holding charlie1-1h\n\
                 [isolation-commander] goal charlie1-1h CANCELLED by charlie_arm_inst token=charlie1-1h\n",
                "[isolation-arm] goal alpha1-1h from alpha_commander_inst accepted by charlie_arm_inst\n"
            ),
            vec![
                "stopped holding `charlie1-1h`".to_owned(),
                "its arm saw `alpha1-1h`".to_owned(),
            ]
        );
        assert_eq!(
            peer_hold_violations("charlie1-1h", "alpha1-1h", "", ""),
            vec!["never held `charlie1-1h`".to_owned()]
        );
    }
}

/// How many states a leader has heard carrying `value`, from its log.
async fn hearings(daemon: &Daemon, instance_id: &str, value: f64) -> usize {
    daemon
        .node_log(instance_id)
        .await
        .matches(&format!("heard positions=[{value:?}, "))
        .count()
}

/// Polls until `instance_id` has heard more states carrying `value` than
/// `before`: the pair is still carrying that leader's own stream.
async fn wait_for_more_hearings(daemon: &Daemon, instance_id: &str, value: f64, before: usize) {
    let started = Instant::now();
    while started.elapsed() < TIMEOUT {
        if hearings(daemon, instance_id, value).await > before {
            return;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    panic!("{instance_id} heard no state carrying {value:?} after its neighbour left");
}

/// FRAMEWORK-220 across machines: a `zero_or_more` follower slot starts
/// empty on the coordinator, three leaders pair into it one at a time, two
/// of them from the second daemon, the hub lists them in join order and
/// each leader hears only the states answered to it, removing the second
/// leaves the other two paired and streaming, and rejoining under the same
/// name pairs once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_multi_slot_holds_one_pair_per_leader_across_daemons() {
    let federation = start_federation("peppy-pairing-fleet").await;
    let cloud = federation.cloud_core_node.as_str();
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                &container_launcher(PAIRING_FLEET_LAUNCHER_FILE),
            ])
            .await,
        "launch the hub",
    );
    // The slot starts empty, with no link and no vacancy written.
    federation
        .robot
        .wait_for_node_log("hub_inst", "[pairing-hub] up, peers=\n")
        .await;

    let leaders: [(&str, f64, Option<&str>); 3] = [
        ("alpha", 1.0, None),
        ("bravo", 2.0, Some(cloud)),
        ("charlie", 3.0, Some(cloud)),
    ];
    for (name, value, place) in leaders {
        let argument = format!("leader_inst.value={value}");
        let mut args = vec![
            "stack",
            "join",
            "limb",
            "-i",
            name,
            "--set-arguments",
            &argument,
        ];
        if let Some(core_node) = place {
            args.extend(["--place", core_node]);
        }
        require_success(federation.robot.peppy(&args).await, "join a leader");
        // The hub hears this leader on a pair tagged with its copy.
        federation
            .robot
            .wait_for_node_log(
                "hub_inst",
                &format!("setpoint from {name}_leader_inst copy={name} positions=[{value:?}, "),
            )
            .await;
    }
    federation
        .robot
        .wait_for_node_log(
            "hub_inst",
            "peers=alpha_leader_inst/alpha,bravo_leader_inst/bravo,charlie_leader_inst/charlie",
        )
        .await;

    // Every leader hears its own value back, and nothing of another's.
    let daemon_of = |name: &str| {
        if name == "alpha" {
            &federation.robot
        } else {
            &federation.cloud
        }
    };
    for (name, value, _) in leaders {
        let instance = format!("{name}_leader_inst");
        let log = daemon_of(name)
            .wait_for_node_log(&instance, &format!("heard positions=[{value:?}, "))
            .await;
        for (other, other_value, _) in leaders {
            if other != name {
                assert!(
                    !log.contains(&format!("heard positions=[{other_value:?}, ")),
                    "{instance} heard {other}'s states:\n{log}"
                );
            }
        }
    }

    // The coordinator lists every pair of the slot, each with the machine
    // its leader runs on.
    let listed = require_success(
        federation
            .robot
            .stack_list(Some(&federation.robot_core_node))
            .await,
        "list the hub's pairs",
    );
    for (name, place) in [
        ("alpha", federation.robot_core_node.as_str()),
        ("bravo", cloud),
        ("charlie", cloud),
    ] {
        let row = format!("limbs ⇌ {name}_leader_inst:hub@{place}");
        assert_eq!(listed.matches(&row).count(), 1, "{row} in:\n{listed}");
    }

    // Removing the second leaves the other two paired and streaming.
    let alpha_before = hearings(&federation.robot, "alpha_leader_inst", 1.0).await;
    let charlie_before = hearings(&federation.cloud, "charlie_leader_inst", 3.0).await;
    require_success(
        federation.robot.peppy(&["stack", "remove", "bravo"]).await,
        "remove a leader",
    );
    federation
        .robot
        .wait_for_node_log(
            "hub_inst",
            "peers=alpha_leader_inst/alpha,charlie_leader_inst/charlie\n",
        )
        .await;
    wait_for_more_hearings(&federation.robot, "alpha_leader_inst", 1.0, alpha_before).await;
    wait_for_more_hearings(
        &federation.cloud,
        "charlie_leader_inst",
        3.0,
        charlie_before,
    )
    .await;

    // Rejoining under the same name pairs once, at the end of the set.
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "join",
                "limb",
                "-i",
                "bravo",
                "--set-arguments",
                "leader_inst.value=2",
                "--place",
                cloud,
            ])
            .await,
        "rejoin a leader",
    );
    federation
        .robot
        .wait_for_node_log(
            "hub_inst",
            "peers=alpha_leader_inst/alpha,charlie_leader_inst/charlie,bravo_leader_inst/bravo",
        )
        .await;
    federation
        .cloud
        .wait_for_node_log("bravo_leader_inst", "heard positions=[2.0, ")
        .await;
    let listed = require_success(
        federation
            .robot
            .stack_list(Some(&federation.robot_core_node))
            .await,
        "list the hub's pairs after the rejoin",
    );
    assert_eq!(
        listed.matches("limbs ⇌ bravo_leader_inst:hub@").count(),
        1,
        "the rejoined leader holds one pair:\n{listed}"
    );

    require_success(
        federation
            .robot
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset the pairing fleet",
    );
}

/// Copies grow and shrink the set slots of gates on both machines: a copy on
/// the peer adds its commander to the gate the coordinator runs, a copy on the
/// coordinator adds its commander to the gate the peer runs, each gate records
/// its members in join order, `stack list` names them under the copy, removing
/// a copy takes its commander out of both, and a federated reset clears the
/// launch.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn copies_grow_and_shrink_sets_on_both_machines() {
    let federation = start_federation("peppy-growing-gates").await;
    let robot = federation.robot_core_node.as_str();
    let cloud = federation.cloud_core_node.as_str();
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                &format!("{PEER_LABEL}@{cloud}"),
                &container_launcher(GROWING_GATES_LAUNCHER_FILE),
            ])
            .await,
        "launch the gates",
    );
    let gates = |listed: &str| {
        (
            recorded_bindings(
                &coordinator_section(listed, robot),
                COORDINATOR_GATE,
                "commander",
            ),
            recorded_bindings(&coordinator_section(listed, cloud), PEER_GATE, "commander"),
        )
    };
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the gates before any copy",
    );
    assert_eq!(gates(&listed), (Vec::new(), Vec::new()), "{listed}");

    require_delivered(
        federation
            .robot
            .peppy(&["stack", "join", "station", "-i", "alpha", "--place", cloud])
            .await,
        "join a station on the peer",
    );
    require_delivered(
        federation
            .robot
            .peppy(&["stack", "join", "station", "-i", "bravo"])
            .await,
        "join a station on the coordinator",
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the grown gates",
    );
    let grown = vec![
        format!("alpha_commander_inst@{cloud} [copy alpha/commander_inst]"),
        format!("bravo_commander_inst@{robot} [copy bravo/commander_inst]"),
    ];
    assert_eq!(gates(&listed), (grown.clone(), grown), "{listed}");
    let bravo = coordinator_section(&listed, robot)["copies"]
        .as_array()
        .unwrap_or_else(|| panic!("the coordinator lists its copies: {listed}"))
        .iter()
        .find(|copy| copy["name"] == "bravo")
        .unwrap_or_else(|| panic!("bravo is listed: {listed}"))
        .clone();
    assert_eq!(
        bravo["set_members"],
        serde_json::json!([
            { "instance_id": COORDINATOR_GATE, "link_id": "commander", "target": "bravo_commander_inst" },
            { "instance_id": PEER_GATE, "link_id": "commander", "target": "bravo_commander_inst" }
        ]),
        "{listed}"
    );

    require_delivered(
        federation.robot.peppy(&["stack", "remove", "alpha"]).await,
        "remove the peer's station",
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the shrunken gates",
    );
    let shrunk = vec![format!(
        "bravo_commander_inst@{robot} [copy bravo/commander_inst]"
    )];
    assert_eq!(gates(&listed), (shrunk.clone(), shrunk), "{listed}");

    require_success(
        federation
            .robot
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset the federation",
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the reset coordinator",
    );
    assert!(
        copy_names(&coordinator_section(&listed, robot)).is_empty(),
        "{listed}"
    );
}

/// FRAMEWORK-221 with real nodes on two machines: a consuming instance on the
/// coordinator and an observing instance on the peer both start with empty
/// `zero_or_more` slots; two copies join, one of them on the peer, and both
/// instances read the copies' members in join order and receive their
/// emissions tagged with the member that sent them; removing one copy leaves
/// the other; rejoining under the same name appears once, at the end, and is
/// heard again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn joined_copies_grow_the_sets_a_consumer_and_an_observer_read() {
    let federation = start_federation("peppy-set-watch").await;
    let cloud = federation.cloud_core_node.clone();
    let consumer = &federation.robot;
    let observer = &federation.cloud;
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                &format!("{PEER_LABEL}@{cloud}"),
                &container_launcher(SET_WATCH_LAUNCHER_FILE),
            ])
            .await,
        "launch the watches",
    );
    for (daemon, instance) in [(consumer, "consumer_inst"), (observer, "observer_inst")] {
        daemon
            .wait_for_node_log(
                instance,
                "[set-watch] members arms=[fixed_arm_inst:none] leaders=[]\n",
            )
            .await;
        // The member the boot config seeded is subscribed like any other.
        daemon
            .wait_for_node_log(instance, "received from fixed_arm_inst\n")
            .await;
    }

    require_delivered(
        federation
            .robot
            .peppy(&["stack", "join", "member", "-i", "alpha"])
            .await,
        "join alpha on the coordinator",
    );
    require_delivered(
        federation
            .robot
            .peppy(&["stack", "join", "member", "-i", "bravo", "--place", &cloud])
            .await,
        "join bravo on the peer",
    );
    // Each member names the copy its instance belongs to, on both machines;
    // the launcher-bound arm belongs to none.
    consumer
        .wait_for_node_log(
            "consumer_inst",
            "members arms=[fixed_arm_inst:none,alpha_arm_inst:alpha/arm_inst,bravo_arm_inst:bravo/arm_inst] \
              leaders=[alpha_leader_inst,bravo_leader_inst]\n",
        )
        .await;
    observer
        .wait_for_node_log(
            "observer_inst",
            "members arms=[fixed_arm_inst:none,alpha_arm_inst:alpha/arm_inst,bravo_arm_inst:bravo/arm_inst] \
              leaders=[alpha_leader_inst,bravo_leader_inst]\n",
        )
        .await;
    for name in ["alpha", "bravo"] {
        for (daemon, instance) in [(consumer, "consumer_inst"), (observer, "observer_inst")] {
            for member in [format!("{name}_arm_inst"), format!("{name}_leader_inst")] {
                daemon
                    .wait_for_node_log(instance, &format!("received from {member}\n"))
                    .await;
            }
        }
    }

    require_delivered(
        federation.robot.peppy(&["stack", "remove", "alpha"]).await,
        "remove alpha",
    );
    consumer
        .wait_for_node_log(
            "consumer_inst",
            "members arms=[fixed_arm_inst:none,bravo_arm_inst:bravo/arm_inst] leaders=[bravo_leader_inst]\n",
        )
        .await;
    observer
        .wait_for_node_log(
            "observer_inst",
            "members arms=[fixed_arm_inst:none,bravo_arm_inst:bravo/arm_inst] leaders=[bravo_leader_inst]\n",
        )
        .await;

    require_delivered(
        federation
            .robot
            .peppy(&["stack", "join", "member", "-i", "alpha"])
            .await,
        "rejoin alpha on the coordinator",
    );
    consumer
        .wait_for_node_log(
            "consumer_inst",
            "members arms=[fixed_arm_inst:none,bravo_arm_inst:bravo/arm_inst,alpha_arm_inst:alpha/arm_inst] \
              leaders=[bravo_leader_inst,alpha_leader_inst]\n",
        )
        .await;
    observer
        .wait_for_node_log(
            "observer_inst",
            "members arms=[fixed_arm_inst:none,bravo_arm_inst:bravo/arm_inst,alpha_arm_inst:alpha/arm_inst] \
              leaders=[bravo_leader_inst,alpha_leader_inst]\n",
        )
        .await;
    wait_for_log_count(
        consumer,
        "consumer_inst",
        "received from alpha_arm_inst\n",
        2,
    )
    .await;
    wait_for_log_count(
        observer,
        "observer_inst",
        "received from alpha_leader_inst\n",
        2,
    )
    .await;

    require_success(
        federation
            .robot
            .peppy(&["stack", "reset", "--federated"])
            .await,
        "reset the federation",
    );
    for (daemon, instance) in [(consumer, "consumer_inst"), (observer, "observer_inst")] {
        daemon
            .wait_for_stack(|text| !holds_instance(text, instance))
            .await;
    }
}

/// A set whose instance is gone from the machine that ran it: the join that
/// would grow it is refused at plan time, and the coordinator keeps the stack
/// it had, with no copy and no instance of one.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_join_growing_a_set_whose_instance_is_gone_is_refused_and_changes_nothing() {
    let federation = start_federation("peppy-missing-set-holder").await;
    let robot = federation.robot_core_node.as_str();
    let cloud = federation.cloud_core_node.as_str();
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                &format!("{PEER_LABEL}@{cloud}"),
                &container_launcher(GROWING_GATES_LAUNCHER_FILE),
            ])
            .await,
        "launch the gates",
    );
    // The peer keeps running, but the gate whose set every station grows is
    // gone from it.
    require_success(
        federation.cloud.peppy(&["stack", "reset"]).await,
        "clear the peer",
    );

    let refused = federation
        .robot
        .peppy(&["stack", "join", "station", "-i", "alpha"])
        .await;
    assert!(!refused.success(), "{}", refused.text);
    assert!(
        refused.text.contains(&format!(
            "grows `{PEER_GATE}.links.commander`, but `{PEER_GATE}` is not running on `{cloud}`"
        )),
        "{}",
        refused.text
    );

    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the coordinator after the refused join",
    );
    let coordinator = coordinator_section(&listed, robot);
    assert!(copy_names(&coordinator).is_empty(), "{listed}");
    assert!(
        recorded_bindings(&coordinator, COORDINATOR_GATE, "commander").is_empty(),
        "{listed}"
    );
    assert!(
        !listed.contains("alpha_commander_inst"),
        "a refused join starts nothing:\n{listed}"
    );
}

/// A set on a machine that is not live: a join that would grow it is refused
/// at plan time, naming the slot, the machine and the fix, and a removal
/// delivers the coordinator's shrunken set and says the peer's set still
/// lists the copy's members.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_set_on_an_offline_machine_refuses_a_join_and_stays_on_removal() {
    let federation = start_federation("peppy-offline-gates").await;
    let robot = federation.robot_core_node.as_str();
    let cloud = federation.cloud_core_node.as_str();
    require_success(
        federation
            .robot
            .peppy(&[
                "stack",
                "launch",
                "--place",
                &format!("{PEER_LABEL}@{cloud}"),
                &container_launcher(GROWING_GATES_LAUNCHER_FILE),
            ])
            .await,
        "launch the gates",
    );
    require_success(
        federation
            .robot
            .peppy(&["stack", "join", "station", "-i", "alpha"])
            .await,
        "join alpha while both machines are live",
    );
    federation.cloud.stop().await;

    let refused = federation
        .robot
        .peppy(&["stack", "join", "station", "-i", "bravo"])
        .await;
    assert!(!refused.success(), "{}", refused.text);
    assert!(
        refused.text.contains(&format!(
            "`{PEER_GATE}.links.commander` on `{cloud}`, which is not live"
        )) && refused
            .text
            .contains(&format!("Bring `{cloud}`'s daemon back")),
        "{}",
        refused.text
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the coordinator after the refused join",
    );
    let coordinator = coordinator_section(&listed, robot);
    assert_eq!(copy_names(&coordinator), ["alpha"], "{listed}");
    assert_eq!(
        recorded_bindings(&coordinator, COORDINATOR_GATE, "commander"),
        [format!(
            "alpha_commander_inst@{robot} [copy alpha/commander_inst]"
        )],
        "{listed}"
    );

    let removed = federation.robot.peppy(&["stack", "remove", "alpha"]).await;
    assert!(removed.success(), "{}", removed.text);
    assert!(
        removed.text.contains(&format!(
            "`{cloud}` is not live on the federation, so removing copy `alpha` did not update \
             these sets there: `{PEER_GATE}.links.commander`"
        )),
        "{}",
        removed.text
    );
    let listed = require_success(
        federation.robot.peppy(&["stack", "list", "--json"]).await,
        "the coordinator after the removal",
    );
    let coordinator = coordinator_section(&listed, robot);
    assert!(copy_names(&coordinator).is_empty(), "{listed}");
    assert!(
        recorded_bindings(&coordinator, COORDINATOR_GATE, "commander").is_empty(),
        "{listed}"
    );
}
