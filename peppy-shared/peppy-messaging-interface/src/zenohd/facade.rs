use super::super::error::{Error, Result};
use super::{ZenohEndpoint, ZenohNetProtocol};
use crate::router_id::RouterId;
use std::env;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use zenoh::config::Config;

/// Bound on one local TCP dial of the router endpoint (the port-in-use
/// pre-flight and each readiness poll attempt): loopback answers within
/// microseconds, and the bound keeps a filtered or non-local host from pinning
/// startup on a single dial.
const ROUTER_PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

enum RouterOwnership {
    /// A router spawned and supervised by peppy from a rendered (or
    /// operator-pinned) config file.
    Managed {
        zenohd_path: Option<String>,
        zenohd_config_path: PathBuf,
        pinned: bool,
        zenohd_log_path: PathBuf,
        /// The transport identity this router's config pins. Held here rather
        /// than re-read from the config file so a re-render
        /// ([`crate::ZenohAdapter::refederate`]) reuses the identity the router
        /// was built with: a federation change must not look, to anything
        /// watching the router's sessions, like a different machine arriving.
        ///
        /// Living in the `Managed` variant makes that structural. An external
        /// router's config is the operator's and carries whatever `id` they
        /// chose, so there is no id for peppy to hold, and no code path that
        /// has to remember not to look for one.
        router_id: RouterId,
    },
    /// An already-running router that peppy only probes and uses. No binary or
    /// router config belongs to peppy in this mode.
    External,
}

/// Checks whether a child process has exited prematurely.
/// Ok if it is still alive; otherwise an error carrying the tail of the router
/// log. zenohd's stdout/stderr are redirected to `log_path`, so the diagnostic
/// comes from the file rather than from a pipe.
fn check_process_alive(child: &mut Child, log_path: &Path) -> std::result::Result<(), Error> {
    match child.try_wait() {
        Ok(Some(status)) => Err(Error::BackendError(format!(
            "zenohd exited unexpectedly with status: {}{}",
            status,
            zenohd_log_excerpt(log_path),
        ))),
        Ok(None) => Ok(()),
        Err(e) => Err(Error::BackendError(format!(
            "Failed to check zenohd status: {}",
            e
        ))),
    }
}

/// Builds a short suffix describing why zenohd exited by reading the tail of
/// its redirected log file. Used only on the error path; falls back to the log
/// path when the file is empty or unreadable.
fn zenohd_log_excerpt(log_path: &Path) -> String {
    match std::fs::read_to_string(log_path) {
        Ok(contents) if !contents.trim().is_empty() => {
            let tail = contents
                .lines()
                .rev()
                .take(20)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect::<Vec<_>>()
                .join("\n");
            format!(" – last zenohd log lines:\n{tail}")
        }
        _ => format!(" (see zenohd log at {})", log_path.display()),
    }
}

/// Environment variable naming an explicit `zenohd` binary to run, taking
/// precedence over every packaged/built candidate. The escape hatch for
/// installed-wheel and installed-crate consumers whose build baked a
/// build-machine `ZENOHD_BINARY_PATH` that does not exist on this host.
pub const ZENOHD_PATH_VAR: &str = "PEPPY_ZENOHD_PATH";

/// Resolve only release-authorized router artifacts: a binary packaged beside
/// the current executable, or the exact content-tagged artifact embedded by
/// pmi's build script. In particular this helper has no PATH input, which keeps
/// the release selection policy easy to test without mutating process globals.
fn packaged_or_built_zenohd(
    current_executable: Option<&Path>,
    built_artifact: Option<&Path>,
) -> Option<String> {
    if let Some(candidate) = current_executable
        .and_then(Path::parent)
        .map(|directory| directory.join("zenohd"))
        .filter(|candidate| candidate.is_file())
    {
        return Some(candidate.to_string_lossy().into_owned());
    }

    built_artifact
        .filter(|candidate| candidate.is_file())
        .map(|candidate| candidate.to_string_lossy().into_owned())
}

/// A `zenohd` packaged beside the `peppy` binary found on `path_var`. Unlike a
/// bare `zenohd` on PATH, this candidate has provenance: an installed peppy
/// distribution ships its router next to the CLI, so the peppy install anchors
/// which zenohd belongs with this workspace even when the *current* executable
/// is not peppy itself (a node's test binary, an installed peppylib wheel).
/// Release-authorized for that reason.
fn zenohd_beside_peppy_on_path(path_var: Option<&std::ffi::OsStr>) -> Option<String> {
    let path_var = path_var?;
    for dir in env::split_paths(path_var) {
        if dir.join("peppy").is_file() {
            let candidate = dir.join("zenohd");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }
    None
}

/// Full resolution policy, ordered by provenance strength. Pure in its inputs
/// so the order is testable without mutating process globals:
/// 1. an explicit [`ZENOHD_PATH_VAR`] override (used verbatim — a missing file
///    surfaces as the spawn error naming the operator's path, never a silent
///    fallback to a different binary),
/// 2. a `zenohd` packaged beside the current executable,
/// 3. the exact artifact embedded by pmi's `build_zenoh` build script,
/// 4. a `zenohd` beside the `peppy` binary found on PATH,
/// 5. (debug builds only) a bare `zenohd` anywhere on PATH.
fn resolve_zenohd_binary(
    env_override: Option<std::ffi::OsString>,
    current_executable: Option<&Path>,
    built_artifact: Option<&Path>,
    path_var: Option<&std::ffi::OsStr>,
) -> Option<String> {
    if let Some(explicit) = env_override.filter(|value| !value.is_empty()) {
        return Some(explicit.to_string_lossy().into_owned());
    }

    if let Some(path) = packaged_or_built_zenohd(current_executable, built_artifact) {
        return Some(path);
    }

    if let Some(path) = zenohd_beside_peppy_on_path(path_var) {
        return Some(path);
    }

    // Developer builds may use an explicitly installed zenohd for fast
    // iteration. Release builds deliberately compile this branch out: a
    // bare `zenohd` picked up from PATH has no provenance and no guaranteed
    // version relationship to the `zenoh` library this binary links, while
    // every candidate above is packaged or built alongside it (or an explicit
    // operator choice). Note that this follows the Cargo profile's
    // `debug-assertions` setting rather than the profile name, so a release
    // profile that turns debug assertions back on opts back into it.
    #[cfg(debug_assertions)]
    if let Some(path_var) = path_var {
        for dir in env::split_paths(path_var) {
            let candidate = dir.join("zenohd");
            if candidate.is_file() {
                return Some(candidate.to_string_lossy().into_owned());
            }
        }
    }

    None
}

/// The Zenoh daemon binary facade. Zenohd is not accessible via the Rust API (or in a very limited fashion).
/// This facade allows calling the binary in the background.
pub struct ZenohdFacade {
    ownership: RouterOwnership,
    adopted: bool,
    /// When set, the spawned zenohd is SIGKILLed by the kernel if the spawning
    /// thread dies (`PR_SET_PDEATHSIG`, Linux only). Opted into only by the
    /// ephemeral-router path — where the spawning runtime lives exactly as long
    /// as the test that owns the router — so a SIGKILLed test process cannot
    /// orphan its zenohd. Deliberately never set for daemon-managed routers:
    /// the pdeathsig contract is per-*thread*, and tying a production router's
    /// life to whichever runtime thread happened to spawn it would be fragile.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    kill_on_parent_death: bool,
    pub router_process: Option<Child>,
    pub zenoh_endpoint: ZenohEndpoint,
}

impl ZenohdFacade {
    /// Creates a facade for a peppy-managed zenohd process. The router endpoint
    /// is extracted from the exact config that will be passed to zenohd.
    ///
    /// `router_id` is the identity that config pins (or, on the operator-pinned
    /// `ZENOH_CONFIG` path, the identity peppy *would* have pinned; nothing
    /// re-renders that config, so it is never used there).
    pub fn managed(zenohd_config_path: impl AsRef<Path>, router_id: RouterId) -> Result<Self> {
        let zenoh_endpoint = ZenohdFacade::get_endpoint_from_config(&zenohd_config_path)?;
        let zenohd_config_path = zenohd_config_path.as_ref().to_path_buf();
        // The router is operator-pinned when it runs the `ZENOH_CONFIG` file
        // verbatim (`router_config_path` returns that path unchanged). Capture it
        // once, here, when the router is built — refederate consults this instead
        // of re-reading the process-global env on every call.
        let pinned =
            crate::zenohd::config_override().as_deref() == Some(zenohd_config_path.as_path());
        // Keep the log next to the generated config (same directory), one file
        // per router port.
        let zenohd_log_path = zenohd_config_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("zenohd_{}.log", zenoh_endpoint.port()));
        Ok(Self {
            ownership: RouterOwnership::Managed {
                zenohd_path: Self::get_zenohd_binary(),
                zenohd_config_path,
                pinned,
                zenohd_log_path,
                router_id,
            },
            adopted: false,
            kill_on_parent_death: false,
            router_process: None,
            zenoh_endpoint,
        })
    }

    /// Creates a facade for a router whose process and configuration are owned
    /// by the operator. The endpoint has already been validated as a dialable
    /// TCP locator by [`ZenohAdapter::with_external_router`](crate::ZenohAdapter::with_external_router).
    pub fn external(zenoh_endpoint: ZenohEndpoint) -> Self {
        Self {
            ownership: RouterOwnership::External,
            adopted: false,
            kill_on_parent_death: false,
            router_process: None,
            zenoh_endpoint,
        }
    }

    /// Opt this managed router into kernel-enforced reaping: the spawned zenohd
    /// receives SIGKILL when the spawning thread dies (`PR_SET_PDEATHSIG`;
    /// no-op off Linux). See the field doc for why only the ephemeral-router
    /// path sets this.
    pub(crate) fn set_kill_on_parent_death(&mut self) {
        self.kill_on_parent_death = true;
    }

    fn get_zenohd_binary() -> Option<String> {
        let current_executable = env::current_exe().ok();
        let built_artifact = option_env!("ZENOHD_BINARY_PATH").map(Path::new);
        let path_var = env::var_os("PATH");
        resolve_zenohd_binary(
            env::var_os(ZENOHD_PATH_VAR),
            current_executable.as_deref(),
            built_artifact,
            path_var.as_deref(),
        )
    }

    /// The zenohd binary the current resolution rules select, if any — for
    /// test tooling that must forward it to a child process via
    /// [`ZENOHD_PATH_VAR`] (e.g. running a node's `cargo test` from an
    /// environment that has no `peppy` on PATH: the node's vendored pmi has
    /// no built artifact of its own to fall back on).
    pub fn resolved_zenohd_binary() -> Option<String> {
        Self::get_zenohd_binary()
    }

    fn connect_addr(&self) -> String {
        let connect_host = if self.zenoh_endpoint.host() == "0.0.0.0" {
            "127.0.0.1"
        } else {
            self.zenoh_endpoint.host()
        };
        format!("{connect_host}:{}", self.zenoh_endpoint.port())
    }

    fn tcp_based(&self) -> bool {
        matches!(
            self.zenoh_endpoint.protocol(),
            ZenohNetProtocol::Tcp | ZenohNetProtocol::Tls
        )
    }

    /// Asynchronously checks whether the configured TCP endpoint accepts a
    /// connection within `timeout`.
    ///
    /// External endpoints may require DNS resolution or route to a remote
    /// network. Using Tokio's connect future keeps that work off the async
    /// startup thread, while the outer timeout prevents an unreachable endpoint
    /// from stalling adoption indefinitely. This is intentionally only a socket
    /// reachability check; the caller performs a separate Zenoh handshake so it
    /// can distinguish an absent endpoint from a non-Zenoh service.
    pub(crate) async fn router_endpoint_reachable(&self, timeout: std::time::Duration) -> bool {
        if !self.tcp_based() {
            return false;
        }

        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(self.connect_addr()))
            .await
        {
            Ok(Ok(stream)) => {
                // Linux can occasionally satisfy a loopback connect to an
                // unbound ephemeral port with a TCP self-connection: the
                // kernel selects that same port for the client side, making
                // local and peer addresses identical. That is not a listening
                // router and must not advance external-router adoption to the
                // Zenoh handshake path.
                match (stream.local_addr(), stream.peer_addr()) {
                    (Ok(local), Ok(peer)) => local != peer,
                    _ => false,
                }
            }
            _ => false,
        }
    }

    pub fn is_adopted(&self) -> bool {
        self.adopted
    }

    pub(crate) fn is_external(&self) -> bool {
        matches!(&self.ownership, RouterOwnership::External)
    }

    pub(crate) fn is_pinned(&self) -> bool {
        matches!(
            &self.ownership,
            RouterOwnership::Managed { pinned: true, .. }
        )
    }

    pub(crate) fn managed_config_path(&self) -> Option<&Path> {
        match &self.ownership {
            RouterOwnership::Managed {
                zenohd_config_path, ..
            } => Some(zenohd_config_path),
            RouterOwnership::External => None,
        }
    }

    pub(crate) fn managed_log_path(&self) -> Option<&Path> {
        match &self.ownership {
            RouterOwnership::Managed {
                zenohd_log_path, ..
            } => Some(zenohd_log_path),
            RouterOwnership::External => None,
        }
    }

    /// The transport identity a managed router's config pins, so a re-render
    /// keeps it. `None` for an external router, whose config peppy neither
    /// wrote nor rewrites.
    pub(crate) fn managed_router_id(&self) -> Option<&RouterId> {
        match &self.ownership {
            RouterOwnership::Managed { router_id, .. } => Some(router_id),
            RouterOwnership::External => None,
        }
    }

    /// The `connect.endpoints` of the active managed router config (rendered or
    /// operator-pinned) — the links the router dials on startup, which
    /// [`super::RouterLinksProbe`] waits on before the boot presence check.
    /// Empty for an external router (its config is the operator's, unknown to
    /// peppy) and for a standalone managed router; also empty — with a warning —
    /// when the config cannot be read, so a broken read degrades to "nothing to
    /// wait for" rather than an error on the startup path (zenohd itself already
    /// parsed the same file to boot).
    ///
    /// Endpoints are accepted in both config shapes: a plain array (what peppy
    /// renders) and the per-mode `{ router: [...] }` map an operator may pin.
    pub(crate) fn configured_connect_endpoints(&self) -> Vec<String> {
        let Some(config_path) = self.managed_config_path() else {
            return Vec::new();
        };
        let connect = Config::from_file(config_path)
            .map_err(|e| e.to_string())
            .and_then(|config| config.get_json("connect").map_err(|e| e.to_string()))
            .and_then(|json| {
                serde_json::from_str::<serde_json::Value>(&json).map_err(|e| e.to_string())
            });
        let connect = match connect {
            Ok(connect) => connect,
            Err(error) => {
                tracing::warn!(
                    config = %config_path.display(),
                    error,
                    "failed to read the router config's connect endpoints"
                );
                return Vec::new();
            }
        };
        let endpoints = &connect["endpoints"];
        let list = endpoints
            .as_array()
            .or_else(|| endpoints["router"].as_array());
        list.into_iter()
            .flatten()
            .filter_map(|endpoint| endpoint.as_str())
            .map(str::to_string)
            .collect()
    }

    pub(crate) fn adopt_external_router(&mut self) {
        debug_assert!(self.is_external());
        self.adopted = true;
        tracing::info!(
            "Adopted external zenoh router at {}; peppy will not manage its lifecycle",
            self.zenoh_endpoint
        );
    }

    fn get_endpoint_from_config(zenohd_config_path: impl AsRef<Path>) -> Result<ZenohEndpoint> {
        let config = Config::from_file(zenohd_config_path).map_err(|e| {
            Error::ConfigurationError(format!("Failed to load zenoh config file: {}", e))
        })?;
        let listen_json = config.get_json("listen").map_err(|e| {
            Error::ConfigurationError(format!("Failed to get listen config: {}", e))
        })?;

        let listen: serde_json::Value = serde_json::from_str(&listen_json).map_err(|e| {
            Error::ConfigurationError(format!("Failed to parse listen config: {}", e))
        })?;

        let endpoint_str = listen["endpoints"]["router"][0].as_str().ok_or_else(|| {
            Error::ConfigurationError("No router endpoint found in config".to_string())
        })?;

        endpoint_str.parse()
    }

    /// Starts a managed zenohd process without blocking the async runtime during
    /// readiness. The child is stored before the first await, so cancellation
    /// leaves it supervised by the facade.
    ///
    /// `Tls` is TLS-over-TCP, so the plain TCP probes here apply to it too: the
    /// listening socket accepts TCP before the TLS handshake, which is all a
    /// "port already bound / accepting yet?" check needs.
    pub(crate) async fn start_router(&mut self) -> Result<()> {
        let (zenohd_config_path, zenohd_log_path) =
            match (&self.ownership, self.router_process.is_some()) {
                // A managed child already exists (an earlier, e.g. timed-out,
                // start spawned it): resume waiting on that child instead of
                // spawning a replacement.
                (
                    RouterOwnership::Managed {
                        zenohd_config_path,
                        zenohd_log_path,
                        ..
                    },
                    true,
                ) => (zenohd_config_path.clone(), zenohd_log_path.clone()),
                _ => {
                    // Pre-flight (managed routers only, since the external case
                    // falls through to `spawn_router_process`'s ownership
                    // error): refuse to spawn onto a port something is already
                    // listening on.
                    if !self.is_external()
                        && self.router_endpoint_reachable(ROUTER_PROBE_TIMEOUT).await
                    {
                        return Err(Error::BackendError(format!(
                            "Zenoh router port already in use: {}",
                            self.connect_addr()
                        )));
                    }
                    self.spawn_router_process()?
                }
            };

        if self.tcp_based() {
            tracing::info!(
                "Waiting for Zenoh router to accept connections at {}://{}",
                self.zenoh_endpoint.protocol(),
                self.connect_addr()
            );
            let start = tokio::time::Instant::now();
            let timeout = std::time::Duration::from_secs(30);
            let mut backoff = std::time::Duration::from_millis(10);
            let max_backoff = std::time::Duration::from_millis(500);

            loop {
                if let Err(error) = self.check_spawned_process(&zenohd_log_path) {
                    self.discard_failed_router_process().await;
                    return Err(error);
                }

                if self.router_endpoint_reachable(ROUTER_PROBE_TIMEOUT).await {
                    break;
                }
                if start.elapsed() >= timeout {
                    if let Err(error) = self.stop_router_async().await {
                        tracing::warn!(
                            "Failed to stop zenohd after its readiness timeout: {}",
                            error
                        );
                    }
                    return Err(Error::BackendError(format!(
                        "zenohd readiness timeout after {}s (TCP {})",
                        timeout.as_secs(),
                        self.connect_addr()
                    )));
                }
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(max_backoff);
            }
        } else if let Err(error) = self.check_spawned_process(&zenohd_log_path) {
            self.discard_failed_router_process().await;
            return Err(error);
        }

        tracing::info!(
            "Zenoh router started (config {}, logs {})",
            zenohd_config_path.display(),
            zenohd_log_path.display()
        );

        Ok(())
    }

    /// Spawns the managed zenohd child and records it on the facade, returning
    /// the config and log paths the readiness wait needs.
    fn spawn_router_process(&mut self) -> Result<(PathBuf, PathBuf)> {
        if self.router_process.is_some() {
            return Err(Error::BackendError(
                "refusing to replace an existing managed zenohd process".to_string(),
            ));
        }

        let RouterOwnership::Managed {
            zenohd_path,
            zenohd_config_path,
            zenohd_log_path,
            ..
        } = &self.ownership
        else {
            return Err(Error::BackendError(
                "cannot spawn an operator-managed external Zenoh router".to_string(),
            ));
        };
        let zenohd_path = zenohd_path.clone().ok_or_else(|| {
            Error::ZenohdError(format!(
                "Zenohd binary not found. Checked, in order: the `{ZENOHD_PATH_VAR}` \
                 environment variable (unset), a `zenohd` packaged beside the current \
                 executable, the exact artifact produced by pmi's `build_zenoh` feature, \
                 and a `zenohd` beside the `peppy` binary on PATH. Fix: set \
                 `{ZENOHD_PATH_VAR}` to an explicit zenohd binary, or install a peppy \
                 distribution (which ships zenohd next to `peppy`). A bare `zenohd` on \
                 PATH is honored only in debug builds, because it has no guaranteed \
                 version relationship to the linked zenoh library."
            ))
        })?;
        let zenohd_config_path = zenohd_config_path.clone();
        let zenohd_log_path = zenohd_log_path.clone();

        // Redirect stdout+stderr to a log file instead of unread pipes: a full
        // pipe buffer blocks a zenohd thread in `write` and deadlocks the whole
        // router. Pin the log level too, so a verbose inherited `RUST_LOG`
        // can't flood the file (override with `PEPPY_ZENOHD_LOG`).
        let log_file = File::create(&zenohd_log_path).map_err(|e| {
            Error::BackendError(format!(
                "Failed to create zenohd log file {}: {}",
                zenohd_log_path.display(),
                e
            ))
        })?;
        let stderr_file = log_file
            .try_clone()
            .map_err(|e| Error::BackendError(format!("Failed to set up zenohd log file: {}", e)))?;
        let zenohd_log_level =
            env::var("PEPPY_ZENOHD_LOG").unwrap_or_else(|_| "zenoh=warn".to_string());

        let mut command = Command::new(&zenohd_path);
        command
            .env("ZENOH_CONFIG", zenohd_config_path.as_os_str())
            .env("RUST_LOG", zenohd_log_level)
            .arg("-c")
            .arg(&zenohd_config_path)
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(stderr_file));
        #[cfg(target_os = "linux")]
        if self.kill_on_parent_death {
            use std::os::unix::process::CommandExt;
            // The one crate-wide unsafe opt-out (see lib.rs): `pre_exec` is
            // unsafe by signature, so no safe equivalent exists.
            // SAFETY: the pre_exec closure runs in the forked child before
            // exec; prctl on the child's own process is async-signal-safe and
            // touches no parent state.
            #[allow(unsafe_code)]
            unsafe {
                command.pre_exec(|| {
                    if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let child = command.spawn().map_err(|e| {
            Error::BackendError(format!("Failed to start zenohd `{zenohd_path}`: {e}"))
        })?;

        // Store the child before any async readiness wait. If the caller's
        // timeout cancels that wait, Drop or the next stop/start still owns it.
        self.router_process = Some(child);
        Ok((zenohd_config_path, zenohd_log_path))
    }

    /// Reports whether the child recorded by [`Self::spawn_router_process`] is
    /// still running. A missing handle means an invariant broke between the
    /// spawn and this check, which the caller gets as an error rather than a
    /// panic.
    fn check_spawned_process(&mut self, zenohd_log_path: &Path) -> Result<()> {
        let child = self.router_process.as_mut().ok_or_else(|| {
            Error::BackendError("zenohd process handle disappeared during startup".to_string())
        })?;
        check_process_alive(child, zenohd_log_path)
    }

    /// Drops a managed child whose startup liveness check failed, so the stored
    /// handle never outlives the process it describes: leaving it set would make
    /// a later `start_router` resume waiting on a dead child instead of spawning
    /// a fresh one.
    async fn discard_failed_router_process(&mut self) {
        if let Err(error) = self.stop_router_async().await {
            tracing::warn!(
                "Failed to clear zenohd after its startup check failed: {}",
                error
            );
        }
        self.router_process = None;
    }

    pub fn stop_router(&mut self) -> Result<()> {
        let Some(child) = self.take_router_process_for_stop() else {
            return Ok(());
        };
        Self::terminate_router_process(child);
        Ok(())
    }

    /// Stops a managed router without running `Child::wait` on an async worker.
    ///
    /// The child moves into the blocking task before the first await. If a
    /// caller bounds or cancels this future, that task keeps ownership and
    /// finishes reaping the process in the background.
    pub(crate) async fn stop_router_async(&mut self) -> Result<()> {
        let Some(child) = self.take_router_process_for_stop() else {
            return Ok(());
        };
        tokio::task::spawn_blocking(move || Self::terminate_router_process(child))
            .await
            .map_err(|e| {
                Error::BackendError(format!("zenohd process-reaper task failed: {}", e))
            })?;
        Ok(())
    }

    fn take_router_process_for_stop(&mut self) -> Option<Child> {
        if self.is_external() {
            tracing::info!("leaving operator-managed external zenoh router running");
            return None;
        }

        self.router_process.take()
    }

    fn terminate_router_process(mut child: Child) {
        // Try to kill the process gracefully.
        if let Err(e) = child.kill() {
            tracing::warn!("Failed to terminate zenohd router process: {}", e);
        } else {
            tracing::info!("Zenohd router process terminated");
        }

        // Reap it so a managed router never leaves a zombie process behind.
        if let Err(e) = child.wait() {
            tracing::warn!("Error waiting for zenohd process to exit: {}", e);
        }
    }
}

impl Drop for ZenohdFacade {
    fn drop(&mut self) {
        if let Err(e) = self.stop_router() {
            tracing::warn!("Failed to stop router during drop: {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn release_router_resolution_accepts_only_adjacent_or_exact_built_artifact() {
        let dir = tempfile::tempdir().expect("temp dir");
        let package_dir = dir.path().join("package");
        std::fs::create_dir(&package_dir).expect("create package dir");
        let current_executable = package_dir.join("peppy");
        let adjacent = package_dir.join("zenohd");
        let built = dir.path().join("target/policy-tagged/zenohd");
        std::fs::create_dir_all(built.parent().unwrap()).expect("create build dir");
        std::fs::write(&built, b"built").expect("write built artifact");

        assert_eq!(
            packaged_or_built_zenohd(Some(&current_executable), Some(&built)),
            Some(built.to_string_lossy().into_owned()),
            "the exact build artifact is used when no adjacent package exists"
        );

        std::fs::write(&adjacent, b"packaged").expect("write adjacent artifact");
        assert_eq!(
            packaged_or_built_zenohd(Some(&current_executable), Some(&built)),
            Some(adjacent.to_string_lossy().into_owned()),
            "a packaged adjacent router takes precedence"
        );

        std::fs::remove_file(&adjacent).expect("remove adjacent fixture");
        std::fs::remove_file(&built).expect("remove built fixture");
        assert_eq!(
            packaged_or_built_zenohd(Some(&current_executable), Some(&built)),
            None,
            "missing authorized artifacts do not become a command-name/PATH fallback"
        );
    }

    #[test]
    fn explicit_env_override_wins_and_is_used_verbatim() {
        let dir = tempfile::tempdir().expect("temp dir");
        let package_dir = dir.path().join("package");
        std::fs::create_dir(&package_dir).expect("create package dir");
        let current_executable = package_dir.join("peppy");
        let adjacent = package_dir.join("zenohd");
        std::fs::write(&adjacent, b"packaged").expect("write adjacent artifact");
        let operator_choice = dir.path().join("operator/zenohd");

        // The override wins over an existing packaged candidate, and is used
        // verbatim even though the file does not exist: a missing operator
        // path must surface as a spawn error naming that path, not silently
        // fall back to a different binary.
        assert_eq!(
            resolve_zenohd_binary(
                Some(operator_choice.clone().into_os_string()),
                Some(&current_executable),
                None,
                None,
            ),
            Some(operator_choice.to_string_lossy().into_owned()),
        );

        // An empty override is treated as unset (the common `VAR= cmd` shell
        // idiom for clearing), falling through to the packaged candidate.
        assert_eq!(
            resolve_zenohd_binary(
                Some(std::ffi::OsString::new()),
                Some(&current_executable),
                None,
                None,
            ),
            Some(adjacent.to_string_lossy().into_owned()),
        );
    }

    #[test]
    fn peppy_adjacent_path_probe_requires_the_peppy_anchor() {
        let dir = tempfile::tempdir().expect("temp dir");
        let with_peppy = dir.path().join("with_peppy");
        let without_peppy = dir.path().join("without_peppy");
        std::fs::create_dir(&with_peppy).expect("create dir");
        std::fs::create_dir(&without_peppy).expect("create dir");
        std::fs::write(without_peppy.join("zenohd"), b"unanchored").expect("write");

        let path_var = env::join_paths([&without_peppy, &with_peppy]).expect("join paths");

        // A `zenohd` in a PATH directory with no `peppy` beside it is not a
        // release-authorized candidate for this probe.
        assert_eq!(
            zenohd_beside_peppy_on_path(Some(path_var.as_os_str())),
            None
        );

        // Once a peppy install anchors the directory, its zenohd is used.
        std::fs::write(with_peppy.join("peppy"), b"cli").expect("write");
        std::fs::write(with_peppy.join("zenohd"), b"anchored").expect("write");
        assert_eq!(
            zenohd_beside_peppy_on_path(Some(path_var.as_os_str())),
            Some(with_peppy.join("zenohd").to_string_lossy().into_owned()),
        );

        // A peppy install without a packaged zenohd contributes nothing.
        std::fs::remove_file(with_peppy.join("zenohd")).expect("remove");
        assert_eq!(
            zenohd_beside_peppy_on_path(Some(path_var.as_os_str())),
            None
        );
    }

    #[test]
    fn resolution_order_prefers_packaged_over_peppy_adjacent() {
        let dir = tempfile::tempdir().expect("temp dir");
        let package_dir = dir.path().join("package");
        let path_dir = dir.path().join("on_path");
        std::fs::create_dir(&package_dir).expect("create dir");
        std::fs::create_dir(&path_dir).expect("create dir");
        let current_executable = package_dir.join("node_test_binary");
        std::fs::write(path_dir.join("peppy"), b"cli").expect("write");
        std::fs::write(path_dir.join("zenohd"), b"peppy-adjacent").expect("write");
        let path_var = env::join_paths([&path_dir]).expect("join paths");

        // With no packaged/built candidate, the peppy-adjacent probe answers.
        assert_eq!(
            resolve_zenohd_binary(
                None,
                Some(&current_executable),
                None,
                Some(path_var.as_os_str()),
            ),
            Some(path_dir.join("zenohd").to_string_lossy().into_owned()),
        );

        // A zenohd packaged beside the current executable outranks it.
        std::fs::write(package_dir.join("zenohd"), b"packaged").expect("write");
        assert_eq!(
            resolve_zenohd_binary(
                None,
                Some(&current_executable),
                None,
                Some(path_var.as_os_str()),
            ),
            Some(package_dir.join("zenohd").to_string_lossy().into_owned()),
        );
    }

    #[test]
    fn test_zenohd_facade_creation_with_config() {
        use std::io::Write;
        use tempfile::Builder;

        let expected_host = "127.0.0.1";
        let expected_port = 7447u16;

        // Create a minimal zenoh config file with .json5 extension
        let mut config_file = Builder::new()
            .suffix(".json5")
            .tempfile()
            .expect("Failed to create temp file");
        writeln!(
            config_file,
            r#"{{
                "listen": {{
                    "endpoints": {{
                        "router": ["tcp/{expected_host}:{expected_port}"]
                    }}
                }}
            }}"#
        )
        .expect("Failed to write config");

        let facade = ZenohdFacade::managed(config_file.path(), RouterId::generate());
        assert!(facade.is_ok(), "Error creating facade: {:?}", facade.err());

        let facade = facade.unwrap();
        assert_eq!(facade.zenoh_endpoint.host(), expected_host);
        assert_eq!(facade.zenoh_endpoint.port(), expected_port);
        assert_eq!(facade.zenoh_endpoint.protocol(), ZenohNetProtocol::Tcp);

        // The log file sits next to the config, named per router port.
        let RouterOwnership::Managed {
            zenohd_log_path, ..
        } = &facade.ownership
        else {
            panic!("expected a managed router facade");
        };
        let expected_log_path = config_file
            .path()
            .parent()
            .unwrap()
            .join(format!("zenohd_{expected_port}.log"));
        assert_eq!(zenohd_log_path, &expected_log_path);
    }

    #[test]
    fn test_stop_router_multiple_times() {
        use std::io::Write;
        use tempfile::Builder;

        // Create a config file with .json5 extension
        let mut config_file = Builder::new()
            .suffix(".json5")
            .tempfile()
            .expect("Failed to create temp file");
        writeln!(
            config_file,
            r#"{{
                "listen": {{
                    "endpoints": {{
                        "router": ["tcp/127.0.0.1:7447"]
                    }}
                }}
            }}"#
        )
        .expect("Failed to write config");

        let mut facade = ZenohdFacade::managed(config_file.path(), RouterId::generate())
            .expect("Failed to create facade");

        // First stop should succeed (no process to stop)
        assert!(facade.stop_router().is_ok());

        // Second stop should also succeed (idempotent)
        assert!(facade.stop_router().is_ok());

        // Process should remain None
        assert!(facade.router_process.is_none());
    }

    #[test]
    fn external_router_has_no_binary_or_config_path() {
        let endpoint = "tcp/zenoh-router.internal:17447"
            .parse()
            .expect("parse external endpoint");
        let facade = ZenohdFacade::external(endpoint);

        assert!(facade.is_external());
        assert_eq!(
            facade.zenoh_endpoint.to_string(),
            "tcp/zenoh-router.internal:17447"
        );
        assert!(facade.managed_config_path().is_none());
        assert!(!facade.is_adopted());
        assert!(facade.router_process.is_none());
    }

    #[test]
    fn stop_router_is_an_idempotent_no_op_for_an_adopted_router() {
        let endpoint = "tcp/127.0.0.1:7447"
            .parse()
            .expect("parse external endpoint");
        let mut facade = ZenohdFacade::external(endpoint);
        facade.adopt_external_router();

        assert!(facade.is_adopted());
        assert!(facade.stop_router().is_ok());
        assert!(facade.stop_router().is_ok());
        assert!(facade.router_process.is_none());
    }

    #[tokio::test]
    async fn external_reachability_probe_distinguishes_listening_and_closed_endpoints() {
        let listener =
            std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind external endpoint");
        let port = listener.local_addr().expect("read listener address").port();
        let endpoint = format!("tcp/127.0.0.1:{port}")
            .parse()
            .expect("parse external endpoint");
        let facade = ZenohdFacade::external(endpoint);

        assert!(
            facade
                .router_endpoint_reachable(std::time::Duration::from_secs(1))
                .await,
            "a listening TCP endpoint must be reachable"
        );

        drop(listener);
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(1);
        while facade
            .router_endpoint_reachable(std::time::Duration::from_millis(50))
            .await
            && tokio::time::Instant::now() < deadline
        {
            // A just-closed listener can remain connectable briefly while the
            // kernel drains its backlog. The probe must converge to closed;
            // requiring that state in the very next scheduler tick is flaky.
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        assert!(
            !facade
                .router_endpoint_reachable(std::time::Duration::from_millis(50))
                .await,
            "a closed TCP endpoint must not be reachable"
        );
    }

    #[tokio::test]
    async fn repeated_start_keeps_an_existing_unready_managed_child() {
        use std::io::Write;

        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("reserve port");
        let port = listener.local_addr().expect("reserved addr").port();
        drop(listener);
        let mut config_file = tempfile::Builder::new()
            .suffix(".json5")
            .tempfile()
            .expect("create config");
        writeln!(
            config_file,
            r#"{{
                "listen": {{
                    "endpoints": {{
                        "router": ["tcp/127.0.0.1:{port}"]
                    }}
                }}
            }}"#
        )
        .expect("write config");

        let mut facade =
            ZenohdFacade::managed(config_file.path(), RouterId::generate()).expect("create facade");
        let child = Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn placeholder child");
        let child_id = child.id();
        facade.router_process = Some(child);

        let result =
            tokio::time::timeout(std::time::Duration::from_millis(50), facade.start_router()).await;

        assert!(
            result.is_err(),
            "start should keep waiting for the existing child"
        );
        assert_eq!(
            facade.router_process.as_ref().map(Child::id),
            Some(child_id),
            "a repeated start must not replace the original child handle"
        );
    }

    #[test]
    fn test_zenohd_log_excerpt() {
        use std::io::Write;
        use tempfile::Builder;

        // Missing file: fall back to pointing at the path. Build it inside a
        // temp dir (without creating the file) so the test is portable.
        let dir = tempfile::tempdir().expect("Failed to create temp dir");
        let missing = dir.path().join("zenohd_0.log");
        assert!(zenohd_log_excerpt(&missing).contains("see zenohd log"));

        // Present file: include the tail of its contents.
        let mut log = Builder::new()
            .suffix(".log")
            .tempfile()
            .expect("Failed to create temp log");
        writeln!(log, "starting up\nerror: address already in use").expect("write");
        let excerpt = zenohd_log_excerpt(log.path());
        assert!(excerpt.contains("address already in use"));
    }
}
