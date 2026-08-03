#![cfg(feature = "multi_daemon_e2e")]

use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use config::consts::{PEPPY_CONFIG_ENV, PEPPY_HOME_ENV};
use daemon_config::peppy_config::{
    ExternalZenohConfig, ManagedZenohConfig, PeppyConfig, ZenohConfig,
};
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

/// Name of the image this test builds for its daemon containers.
const E2E_IMAGE_NAME: &str = "peppy-multi-daemon-e2e";

/// Pinned `uv` release copied into the image. A moving `latest` would make a
/// green run depend on what Astral published that morning.
const UV_VERSION: &str = "0.11.33";

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
            // The build tags the image `E2E_IMAGE_NAME:release` in the local
            // daemon, which is the part every container needs; the returned
            // handle is just another way to name what is now on disk, and
            // `start_daemon` builds its own request per container anyway.
            let _tagged = GenericBuildableImage::new(E2E_IMAGE_NAME, &release)
                .with_dockerfile_string(e2e_dockerfile(&release))
                .build_image()
                .await
                .expect("building the e2e daemon image must succeed");
            (String::from(E2E_IMAGE_NAME), release)
        })
        .await
        .clone()
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

    async fn stop(&self) {
        self.container
            .stop_with_timeout(Some(10))
            .await
            .unwrap_or_else(|error| panic!("stopping {}: {error}", self.name));
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
        self.wait_for_exit().await;
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
    require_docker().await;
    let (image_name, image_tag) = e2e_image().await;

    let _router = ZenohAdapter::start_router_ephemeral_in_mode(
        "0.0.0.0",
        None,
        false,
        pmi::SubscriberBufferSizes::default(),
        None,
    )
    .await
    .expect("host zenohd should start");
    let router_port = _router.port;

    let peppy_binary = Path::new(env!("CARGO_BIN_EXE_peppy"));
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

    let daemon_a = start_daemon(
        &launch,
        &format!("peppy-md-a-{suffix}"),
        "robo-a",
        &external_daemon_config("daemon-a", router_port),
        None,
        None,
    )
    .await;
    let daemon_b = start_daemon(
        &launch,
        &format!("peppy-md-b-{suffix}"),
        "robo-b",
        &external_daemon_config("daemon-b", router_port),
        None,
        None,
    )
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

    let collision = start_daemon(
        &launch,
        &format!("peppy-md-c-{suffix}"),
        "robo-c",
        &external_daemon_config("daemon-a", router_port),
        None,
        None,
    )
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

/// Instances the launcher places on `robot_onboard`, i.e. everything the
/// control loop touches.
const ROBOT_INSTANCES: [&str; 3] = ["wrist_cam_inst", "arm_inst", "reflex_inst"];

/// Instances the launcher places on `cloud_inference`.
const CLOUD_INSTANCES: [&str; 2] = ["planner_inst", "recorder_inst"];

impl Daemon {
    /// Waits for one of this daemon's node instances to log `marker`.
    ///
    /// Reads the per-instance run log rather than the container's stdout: node
    /// output goes to `$PEPPY_HOME/logs/run/<instance>.log`, and it is the only
    /// evidence that a node is not merely Running but actually carrying
    /// messages over its slots. Polls rather than sleeping a fixed time, so it
    /// is bounded by the same `TIMEOUT` as every other wait here and does not
    /// depend on how fast the host is.
    async fn wait_for_node_log(&self, instance_id: &str, marker: &str) -> String {
        let path = format!("{CONTAINER_PEPPY_HOME}/logs/run/{instance_id}.log");
        let started = Instant::now();
        let mut last = String::new();
        while started.elapsed() < TIMEOUT {
            let mut result = self
                .container
                .exec(
                    ExecCommand::new(["cat", path.as_str()])
                        .with_cmd_ready_condition(CmdWaitFor::exit()),
                )
                .await
                .unwrap_or_else(|error| panic!("failed to read {path} in {}: {error}", self.name));
            last = String::from_utf8_lossy(
                &result
                    .stdout_to_vec()
                    .await
                    .unwrap_or_else(|error| panic!("reading {path} in {}: {error}", self.name)),
            )
            .into_owned();
            if last.contains(marker) {
                return last;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
        panic!(
            "timed out waiting for `{marker}` in {}'s `{instance_id}` log; last contents:\n{last}",
            self.name
        );
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

/// Two daemons on a shared router in one namespace, plus the launcher mounted
/// into the coordinator. The substrate every federated test needs.
struct Federation {
    robot: Daemon,
    cloud: Daemon,
    robot_core_node: String,
    cloud_core_node: String,
    _router: pmi::ZenohdInstance,
    _launcher_dir: tempfile::TempDir,
    /// Held for the federation's lifetime: both containers read this
    /// repository off the host while they run.
    _fixture: FixtureRepository,
}

async fn start_federation(prefix: &str) -> Federation {
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
    let router_port = router.port;

    let peppy_binary = Path::new(env!("CARGO_BIN_EXE_peppy"));
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

    // Every launcher these tests drive, in one mountable directory: the file
    // the guide documents, plus the probe launcher beside it.
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
    std::fs::write(
        launcher_dir.path().join(NODE_PROBE_LAUNCHER_FILE),
        NODE_PROBE_LAUNCHER,
    )
    .expect("writing the probe launcher into the launcher mount");
    std::fs::write(
        launcher_dir.path().join(CALLER_ENV_PROBE_LAUNCHER_FILE),
        CALLER_ENV_PROBE_LAUNCHER,
    )
    .expect("writing the caller-env probe launcher into the launcher mount");
    std::fs::write(
        launcher_dir.path().join(PEER_RUN_FAILURE_LAUNCHER_FILE),
        PEER_RUN_FAILURE_LAUNCHER,
    )
    .expect("writing the peer-run-failure launcher into the launcher mount");

    let robot = start_daemon(
        &launch,
        &format!("{prefix}-robot-{suffix}"),
        "robo-robot",
        &external_daemon_config("cn-robot", router_port),
        None,
        Some((launcher_dir.path(), CONTAINER_LAUNCHER_DIR)),
    )
    .await;
    let cloud = start_daemon(
        &launch,
        &format!("{prefix}-cloud-{suffix}"),
        "robo-cloud",
        &external_daemon_config("cn-cloud", router_port),
        None,
        Some((launcher_dir.path(), CONTAINER_LAUNCHER_DIR)),
    )
    .await;

    // Only the coordinator's cache decides what a launch runs: it resolves
    // every deployment once and ships pinned content to the peer, which
    // fetches whatever it does not already hold. The robot submits every
    // launch in this suite, so the cloud daemon deliberately gets NO
    // refresh: its empty cache is what proves the pins carry the launch.
    robot.wait_until_serving().await;
    robot.refresh_repos().await;
    cloud.wait_until_serving().await;

    Federation {
        robot,
        cloud,
        robot_core_node: "cn-robot".to_owned(),
        cloud_core_node: "cn-cloud".to_owned(),
        _router: router,
        _launcher_dir: launcher_dir,
        _fixture: fixture,
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
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
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

    // Make the peer unable to materialize the instance it will be asked to run.
    // A plain file is the shape that survives the daemon's own cleanup: the
    // start first clears any leftover working directory of that name, and that
    // clear refuses on a non-directory, so the name stays occupied and the
    // directory can never be created. A symlink would not do — clearing a
    // leftover unlinks it, and the start would sail on. It is planted on the
    // working directory rather than on a bind source because bind sources are
    // prepared when the peer takes over its slice, before the phases this test
    // is about; nothing prepares this one ahead of the start. Planted on the
    // peer only, so the coordinator's own instance gets through every phase.
    let instance_dir = format!("{CONTAINER_PEPPY_HOME}/instances/{PEER_RUN_FAILURE_INSTANCE}");
    require_success(
        federation
            .cloud
            .exec(vec![
                "sh",
                "-c",
                &format!(
                    "mkdir -p {CONTAINER_PEPPY_HOME}/instances \
                     && printf '' > {instance_dir}"
                ),
            ])
            .await,
        &format!("planting an uncreatable {instance_dir} on the peer"),
    );

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
