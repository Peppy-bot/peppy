//! `peppy update`: check for and install Peppy releases in place.

mod archive;
mod http;
mod version;

use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::process::{Command as ProcessCommand, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use daemon::state::DaemonState;
use daemon_config::consts::{PEPPY_GIT_TAG, PeppyDirs};

use super::Command;
use super::confirm::confirm_prompt;
use super::platform::{daemon_has_user_nodes, read_daemon_state};
use super::service::install::{
    InstalledService, detect_installed_service, install_peppy_daemon, stop_peppy_daemon,
    uninstall_peppy_daemon,
};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::while_spinning;

use version::{CurrentVersion, UpdateDecision, decide_update};
pub use version::{InvalidReleaseVersion, ReleaseVersion};

const DEFAULT_REPO_URL: &str = "https://peppy.bot";
const REPO_URL_ENV: &str = "PEPPY_REPOURL";
const NO_SERVICE_INSTALL_ENV: &str = "PEPPY_NO_SERVICE_INSTALL";
const NO_CONTAINER_SETUP_ENV: &str = "PEPPY_NO_CONTAINER_SETUP";
const NO_ROOT_INSTALL_ENV: &str = "PEPPY_NO_ROOT_INSTALL";

/// Overall deadline and poll cadence when waiting for the newly started daemon
/// to answer `repo list` after `service install`.
const DAEMON_READY_DEADLINE: Duration = Duration::from_secs(120);
const DAEMON_READY_POLL_INTERVAL: Duration = Duration::from_millis(500);

/// Clap value parser for `--version <VERSION>`.
pub fn parse_release_version(value: &str) -> std::result::Result<ReleaseVersion, String> {
    ReleaseVersion::parse(value).map_err(|e| e.to_string())
}

#[derive(Debug)]
pub struct UpdateCommand {
    pub check: bool,
    pub version: Option<ReleaseVersion>,
    pub force: bool,
    pub yes: bool,
}

impl Command for UpdateCommand {
    fn execute(self, ctx: &Arc<AppContext>) -> Result<()> {
        if let Some(core_node) = ctx.core_node_override() {
            return Err(Error::ExecutionFailed(format!(
                "`--core-node {core_node}` is not valid for `peppy update`: it updates the \
                 local peppy installation on this machine, never another core node."
            )));
        }
        let env = UpdateEnvironment::from_host()?;
        let mut ops = HostServiceOps;
        execute_update(&self, ctx, &env, &mut ops, None)
    }
}

/// Policy for running `peppy container setup` after installing binaries on Linux.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerSetupPolicy {
    /// Run `container status`, then `container setup` if checks fail.
    RunIfNeeded,
    /// Run `container status` and warn without invoking sudo (`PEPPY_NO_ROOT_INSTALL`).
    WarnOnly,
    /// Skip container checks completely (`PEPPY_NO_CONTAINER_SETUP`).
    Skip,
}

impl ContainerSetupPolicy {
    const fn from_flags(skip: bool, no_root: bool) -> Self {
        if skip {
            Self::Skip
        } else if no_root {
            Self::WarnOnly
        } else {
            Self::RunIfNeeded
        }
    }
}

/// Whether `peppy update` manages the OS background service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServicePolicy {
    ManageIfInstalled,
    Skip,
}

/// Whether stdin is attached to an interactive terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TerminalMode {
    Interactive,
    NonInteractive,
}

/// Host environment inputs resolved once at the start of `peppy update`.
/// Separated from execution so tests can inject an isolated directory, version,
/// and mock server without mutating process-global state.
struct UpdateEnvironment {
    current_version: CurrentVersion,
    peppy_dirs: PeppyDirs,
    /// When set, passed explicitly as `PEPPY_HOME` to spawned child commands.
    /// `None` in production so child processes inherit the caller's environment
    /// unchanged (preserving whether `service install` writes `PEPPY_HOME` into
    /// the unit file).
    child_peppy_home: Option<PathBuf>,
    install_bin_dir: PathBuf,
    repo_url: String,
    archive_name: String,
    service_policy: ServicePolicy,
    container_setup: ContainerSetupPolicy,
    terminal_mode: TerminalMode,
    installed_service: InstalledService,
    daemon_running: bool,
    /// Override for `daemon_has_user_nodes` in tests; `None` probes the live daemon.
    daemon_has_user_nodes_override: Option<bool>,
    daemon_ready_deadline: Duration,
    daemon_ready_poll_interval: Duration,
}

impl UpdateEnvironment {
    fn from_host() -> Result<Self> {
        let exe = std::env::current_exe().map_err(|e| {
            Error::ExecutionFailed(format!("Could not locate the current peppy binary: {e}"))
        })?;
        let canonical = fs::canonicalize(&exe).unwrap_or(exe);
        let install_bin_dir = canonical
            .parent()
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "Could not determine the installation directory of `{}`.",
                    canonical.display()
                ))
            })?
            .to_path_buf();

        let peppy_dirs = PeppyDirs::default();
        let daemon_running =
            read_daemon_state(&peppy_dirs).is_some_and(|s| DaemonState::is_running(&s));
        let service_policy = if env_flag_set(NO_SERVICE_INSTALL_ENV) {
            ServicePolicy::Skip
        } else {
            ServicePolicy::ManageIfInstalled
        };
        let container_setup = ContainerSetupPolicy::from_flags(
            env_flag_set(NO_CONTAINER_SETUP_ENV),
            env_flag_set(NO_ROOT_INSTALL_ENV),
        );
        let terminal_mode = if io::stdin().is_terminal() {
            TerminalMode::Interactive
        } else {
            TerminalMode::NonInteractive
        };

        Ok(Self {
            current_version: CurrentVersion::from_git_tag(PEPPY_GIT_TAG),
            peppy_dirs,
            child_peppy_home: None,
            install_bin_dir,
            repo_url: resolve_repo_url(std::env::var(REPO_URL_ENV).ok()),
            archive_name: format!("peppy-{}.tgz", release_target_triple()?),
            service_policy,
            container_setup,
            terminal_mode,
            installed_service: detect_installed_service(),
            daemon_running,
            daemon_has_user_nodes_override: None,
            daemon_ready_deadline: DAEMON_READY_DEADLINE,
            daemon_ready_poll_interval: DAEMON_READY_POLL_INTERVAL,
        })
    }
}

/// Service-manager side effects isolated behind a trait so tests can verify
/// stop/uninstall/rollback ordering without touching systemd or launchd.
trait ServiceOps {
    fn stop_and_uninstall(&mut self);
    fn restore_after_rollback(&mut self);
}

struct HostServiceOps;

impl ServiceOps for HostServiceOps {
    fn stop_and_uninstall(&mut self) {
        let _ = stop_peppy_daemon();
        let _ = uninstall_peppy_daemon();
    }

    fn restore_after_rollback(&mut self) {
        let _ = install_peppy_daemon(None);
    }
}

fn env_flag_set(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn resolve_repo_url(override_value: Option<String>) -> String {
    override_value
        .map(|v| v.trim().trim_end_matches('/').to_owned())
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_REPO_URL.to_owned())
}

/// The release target triple for this host, matching `scripts/install.sh` and
/// `scripts/functions/build.py`.
#[expect(
    clippy::unnecessary_wraps,
    reason = "unsupported targets return Err while supported targets compile to Ok"
)]
fn release_target_triple() -> Result<&'static str> {
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        Ok("x86_64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        Ok("aarch64-unknown-linux-gnu")
    }
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        Ok("aarch64-apple-darwin")
    }
    #[cfg(not(any(
        all(target_os = "linux", target_arch = "x86_64"),
        all(target_os = "linux", target_arch = "aarch64"),
        all(target_os = "macos", target_arch = "aarch64"),
    )))]
    {
        Err(Error::ExecutionFailed(format!(
            "Unsupported platform `{}-{}` for `peppy update`. Peppy releases are published for \
             Linux (x86_64, aarch64) and macOS (Apple Silicon).",
            std::env::consts::OS,
            std::env::consts::ARCH
        )))
    }
}

fn execute_update(
    cmd: &UpdateCommand,
    ctx: &Arc<AppContext>,
    env: &UpdateEnvironment,
    service_ops: &mut dyn ServiceOps,
    confirm_reader: Option<&mut dyn BufRead>,
) -> Result<()> {
    let resolved = while_spinning("Checking for updates", || {
        http::probe_release(&env.repo_url, &env.archive_name, cmd.version)
    })?;

    if cmd.check {
        print_check_result(&env.current_version, resolved.version);
        return Ok(());
    }

    if matches!(env.current_version, CurrentVersion::Development) {
        return Err(Error::ExecutionFailed(format!(
            "This peppy binary is a development build (version: unknown) and cannot be updated in \
             place. The latest release is {}. Install a release with:\n  curl -fsSL \
             https://peppy.bot/install.sh | sh",
            resolved.version
        )));
    }

    let decision = decide_update(
        &env.current_version,
        resolved.version,
        cmd.version.is_some(),
        cmd.force,
    );
    match decision {
        UpdateDecision::AlreadyUpToDate { current } => {
            println!("peppy is already up to date ({current}).");
            return Ok(());
        }
        UpdateDecision::NewerThanLatest { current, latest } => {
            println!(
                "Current peppy ({current}) is newer than the latest release ({latest}). \
                 Pass --version {latest} or --force to replace it."
            );
            return Ok(());
        }
        UpdateDecision::Install { .. } => {}
    }

    // Fail fast on directory permissions before downloading a large archive.
    archive::check_dir_writable(&env.install_bin_dir)?;

    let manage_service = env.service_policy == ServicePolicy::ManageIfInstalled
        && matches!(env.installed_service, InstalledService::CurrentLevel(_));

    if manage_service
        && env.daemon_running
        && !cmd.yes
        && !confirm_stopping_active_stack(ctx, env, confirm_reader)?
    {
        println!("Update cancelled.");
        return Ok(());
    }

    println!(
        "Updating peppy from {} to {}...",
        env.current_version, resolved.version
    );

    // Stage inside `install_bin_dir` so all renames stay on the same filesystem
    // and never spill into a size-limited `/tmp` tmpfs.
    let staging_dir = tempfile::Builder::new()
        .prefix(".peppy-update-stage-")
        .tempdir_in(&env.install_bin_dir)
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "Failed to create staging directory in `{}`: {e}",
                env.install_bin_dir.display()
            ))
        })?;

    http::download_and_extract(
        &resolved.download_url,
        &env.archive_name,
        resolved.content_length,
        staging_dir.path(),
    )?;

    let staged_bin_dir = staging_dir.path().join("bin");
    archive::verify_staged_binary(&staged_bin_dir.join("peppy"))?;

    if manage_service {
        service_ops.stop_and_uninstall();
    }

    if let Err(err) = archive::swap_installation(&staged_bin_dir, &env.install_bin_dir) {
        if manage_service {
            service_ops.restore_after_rollback();
        }
        return Err(err);
    }

    // Release staging dir before post-update steps so temp space is freed now.
    drop(staging_dir);

    if env.install_bin_dir.join("lima").is_dir() {
        let lima_data = env.peppy_dirs.root().join("lima-data");
        if !lima_data.exists() {
            fs::create_dir_all(&lima_data)?;
        }
    }

    let new_peppy = env.install_bin_dir.join("peppy");
    run_post_update(&new_peppy, resolved.version, manage_service, env)?;

    println!(
        "Updated peppy from {} to {}.",
        env.current_version, resolved.version
    );
    print_post_update_notes(resolved.version, manage_service, env);

    Ok(())
}

fn print_check_result(current: &CurrentVersion, latest: ReleaseVersion) {
    println!("Current version: {current}");
    println!("Latest release:  {latest}");
    match current {
        CurrentVersion::Release(installed) if *installed == latest => {
            println!("peppy is up to date.");
        }
        CurrentVersion::Release(installed) if *installed > latest => {
            println!("Current version is newer than the latest published release.");
        }
        CurrentVersion::Release(_) | CurrentVersion::TaggedOther(_) => {
            println!("Update available. Run `peppy update` to install {latest}.");
        }
        CurrentVersion::Development => {
            println!(
                "This is a development build. Install {latest} with:\n  curl -fsSL \
                 https://peppy.bot/install.sh | sh"
            );
        }
    }
}

/// Confirms stopping the background daemon when it currently holds active user
/// nodes. An idle daemon (holding only its synthetic core-node root) restarts
/// without prompting, matching `peppy platform enroll`.
fn confirm_stopping_active_stack(
    ctx: &Arc<AppContext>,
    env: &UpdateEnvironment,
    confirm_reader: Option<&mut dyn BufRead>,
) -> Result<bool> {
    let has_nodes = env
        .daemon_has_user_nodes_override
        .unwrap_or_else(|| daemon_has_user_nodes(ctx));
    if !has_nodes {
        return Ok(true);
    }
    if env.terminal_mode == TerminalMode::NonInteractive && confirm_reader.is_none() {
        return Err(Error::ExecutionFailed(
            "The peppy daemon has active nodes in its stack and stdin is not a terminal. \
             Stop the stack first or pass --yes to update non-interactively."
                .to_owned(),
        ));
    }
    confirm_prompt(
        "The peppy daemon has active nodes in its stack. Updating will stop the daemon and its \
         running nodes.\nContinue? [y/N] ",
        confirm_reader,
    )
}

fn new_peppy_command(new_peppy: &Path, env: &UpdateEnvironment) -> ProcessCommand {
    let mut cmd = ProcessCommand::new(new_peppy);
    if let Some(home) = &env.child_peppy_home {
        cmd.env(config::consts::PEPPY_HOME_ENV, home);
    }
    cmd
}

/// Runs post-install steps through the newly installed `peppy` binary so that
/// container prerequisites, default repositories, the background service, and
/// repository indexes match the new release.
fn run_post_update(
    new_peppy: &Path,
    target: ReleaseVersion,
    manage_service: bool,
    env: &UpdateEnvironment,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    if env.container_setup != ContainerSetupPolicy::Skip {
        run_container_setup_if_needed(new_peppy, target, manage_service, env)?;
    }

    let restart_hint = if manage_service {
        format!(
            " Then run `{} service install` to restart the background service.",
            new_peppy.display()
        )
    } else {
        String::new()
    };

    let init_output = new_peppy_command(new_peppy, env)
        .args(["repo", "init"])
        .output()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but running `peppy repo init` failed: {e}.{restart_hint}"
            ))
        })?;
    if !init_output.status.success() {
        let stderr = String::from_utf8_lossy(&init_output.stderr);
        return Err(Error::ExecutionFailed(format!(
            "peppy {target} was installed, but `peppy repo init` failed: {}. Retry manually: \
             `{} repo init`.{restart_hint}",
            stderr.trim(),
            new_peppy.display()
        )));
    }

    if !manage_service {
        return Ok(());
    }

    let install_output = new_peppy_command(new_peppy, env)
        .args(["service", "install"])
        .output()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but starting the background service failed: {e}. \
                 Retry manually: `{} service install`",
                new_peppy.display()
            ))
        })?;
    if !install_output.status.success() {
        let stderr = String::from_utf8_lossy(&install_output.stderr);
        return Err(Error::ExecutionFailed(format!(
            "peppy {target} was installed, but `peppy service install` failed: {}. Retry \
             manually: `{} service install`",
            stderr.trim(),
            new_peppy.display()
        )));
    }

    wait_for_daemon_ready(new_peppy, target, env)?;

    let refresh_status = new_peppy_command(new_peppy, env)
        .args(["repo", "update"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but running `peppy repo update` failed: {e}. \
                 Retry manually: `{} repo update`",
                new_peppy.display()
            ))
        })?;
    if !refresh_status.success() {
        return Err(Error::ExecutionFailed(format!(
            "peppy {target} was installed, but `peppy repo update` exited with {refresh_status}. \
             Retry manually: `{} repo update`",
            new_peppy.display()
        )));
    }

    Ok(())
}

#[cfg(target_os = "linux")]
fn run_container_setup_if_needed(
    new_peppy: &Path,
    target: ReleaseVersion,
    manage_service: bool,
    env: &UpdateEnvironment,
) -> Result<()> {
    let restart_hint = if manage_service {
        format!(
            " Then run `{} service install` to restart the background service.",
            new_peppy.display()
        )
    } else {
        String::new()
    };
    let status = new_peppy_command(new_peppy, env)
        .args(["container", "status"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but checking container prerequisites failed: \
                 {e}.{restart_hint}"
            ))
        })?;
    if status.success() {
        return Ok(());
    }

    if env.container_setup == ContainerSetupPolicy::WarnOnly {
        eprintln!(
            "Warning: container prerequisites need attention. Run `peppy container status` to \
             inspect."
        );
        return Ok(());
    }

    let setup_status = new_peppy_command(new_peppy, env)
        .args(["container", "setup"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but running `peppy container setup` failed: {e}. \
                 Retry manually: `{} container setup`.{restart_hint}",
                new_peppy.display()
            ))
        })?;
    if !setup_status.success() {
        return Err(Error::ExecutionFailed(format!(
            "peppy {target} was installed, but `peppy container setup` failed. Retry manually: \
             `{} container setup`.{restart_hint}",
            new_peppy.display()
        )));
    }
    Ok(())
}

/// Polls `<new_peppy> repo list` until the restarted daemon accepts requests or
/// `env.daemon_ready_deadline` expires.
fn wait_for_daemon_ready(
    new_peppy: &Path,
    target: ReleaseVersion,
    env: &UpdateEnvironment,
) -> Result<()> {
    let probe = || -> Result<()> {
        let start = Instant::now();
        poll_until_ready(
            env.daemon_ready_deadline,
            || start.elapsed(),
            || thread::sleep(env.daemon_ready_poll_interval),
            || probe_daemon_once(new_peppy, target, env),
            |last_error| {
                let detail = if last_error.is_empty() {
                    String::new()
                } else {
                    format!(" ({last_error})")
                };
                Error::ExecutionFailed(format!(
                    "peppy {target} was installed, but the peppy daemon did not become ready in \
                     time{detail}. Run `{} repo update` once it is running.",
                    new_peppy.display()
                ))
            },
        )
    };

    while_spinning("Waiting for the peppy daemon to start", probe)
}

fn probe_daemon_once(
    new_peppy: &Path,
    target: ReleaseVersion,
    env: &UpdateEnvironment,
) -> Result<std::result::Result<(), String>> {
    let output = new_peppy_command(new_peppy, env)
        .args(["repo", "list"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            Error::ExecutionFailed(format!(
                "peppy {target} was installed, but probing the daemon failed: {e}"
            ))
        })?;
    if output.status.success() {
        return Ok(Ok(()));
    }
    Ok(Err(String::from_utf8_lossy(&output.stderr)
        .trim()
        .to_owned()))
}

/// Deterministic poll loop parameterized over elapsed-time and sleep hooks so
/// readiness retries and timeouts can be tested without real wall-clock waits.
fn poll_until_ready(
    deadline: Duration,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(),
    mut probe: impl FnMut() -> Result<std::result::Result<(), String>>,
    on_timeout: impl FnOnce(&str) -> Error,
) -> Result<()> {
    loop {
        match probe()? {
            Ok(()) => return Ok(()),
            Err(last_error) => {
                if elapsed() >= deadline {
                    return Err(on_timeout(&last_error));
                }
                sleep();
            }
        }
    }
}

fn print_post_update_notes(target: ReleaseVersion, manage_service: bool, env: &UpdateEnvironment) {
    if manage_service {
        return;
    }
    match &env.installed_service {
        InstalledService::SystemOnly(path) => {
            println!(
                "Note: a system-wide peppy service is installed at {}. Restart it with sudo and \
                 run `peppy repo update` to use peppy {target}.",
                path.display()
            );
        }
        InstalledService::CurrentLevel(_) | InstalledService::None => {
            if env.daemon_running {
                println!(
                    "Note: a peppy daemon is still running from the previous version. Restart it \
                     and run `peppy repo update` to use peppy {target}."
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::Method::{GET, HEAD};
    use httpmock::MockServer;
    use std::io::Cursor;
    use std::os::unix::fs::PermissionsExt;

    const TEST_ARCHIVE: &str = "peppy-x86_64-unknown-linux-gnu.tgz";

    #[derive(Default)]
    struct RecordingServiceOps {
        stops: usize,
        restores: usize,
    }

    impl ServiceOps for RecordingServiceOps {
        fn stop_and_uninstall(&mut self) {
            self.stops += 1;
        }

        fn restore_after_rollback(&mut self) {
            self.restores += 1;
        }
    }

    fn fake_peppy_script(version: &str, log_path: &Path, container_status_exit: i32) -> Vec<u8> {
        format!(
            "#!/bin/sh\n\
             echo \"$*\" >> \"{}\"\n\
             case \"$1\" in\n\
               --version) echo \"peppy {version}\"; exit 0 ;;\n\
               container)\n\
                 if [ \"$2\" = \"status\" ]; then exit {container_status_exit}; fi\n\
                 exit 0 ;;\n\
               *) exit 0 ;;\n\
             esac\n",
            log_path.display()
        )
        .into_bytes()
    }

    fn mock_latest_release(server: &MockServer, tag: &str, tgz: Vec<u8>) {
        let base = server.base_url();
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{TEST_ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{base}/releases/download/{tag}/{TEST_ARCHIVE}"),
            );
        });
        server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/{tag}/{TEST_ARCHIVE}"));
            then.status(200)
                .header("Content-Length", tgz.len().to_string());
        });
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/{tag}/{TEST_ARCHIVE}"));
            then.status(200)
                .header("Content-Length", tgz.len().to_string());
        });
        server.mock(|when, then| {
            when.method(GET).path(format!("/{tag}/{TEST_ARCHIVE}"));
            then.status(200)
                .header("Content-Length", tgz.len().to_string())
                .body(tgz);
        });
    }

    fn make_env(
        server_url: String,
        peppy_home: &Path,
        install_bin_dir: &Path,
        current_tag: Option<&str>,
    ) -> UpdateEnvironment {
        UpdateEnvironment {
            current_version: CurrentVersion::from_git_tag(current_tag),
            peppy_dirs: PeppyDirs::new(peppy_home),
            child_peppy_home: Some(peppy_home.to_path_buf()),
            install_bin_dir: install_bin_dir.to_path_buf(),
            repo_url: server_url,
            archive_name: TEST_ARCHIVE.to_owned(),
            service_policy: ServicePolicy::ManageIfInstalled,
            container_setup: ContainerSetupPolicy::RunIfNeeded,
            terminal_mode: TerminalMode::NonInteractive,
            installed_service: InstalledService::None,
            daemon_running: false,
            daemon_has_user_nodes_override: Some(false),
            daemon_ready_deadline: Duration::from_secs(5),
            daemon_ready_poll_interval: Duration::from_millis(10),
        }
    }

    fn test_ctx(dir: &Path) -> Arc<AppContext> {
        Arc::new(AppContext::new(dir))
    }

    #[test]
    fn update_rejects_core_node_override() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(tmp.path()).with_core_node_override(Some(
            config::runtime::CoreNodeName::parse("robot-1").unwrap(),
        )));
        let cmd = UpdateCommand {
            check: true,
            version: None,
            force: false,
            yes: false,
        };
        let err = cmd.execute(&ctx).expect_err("--core-node must be refused");
        assert!(err.to_string().contains("--core-node robot-1"), "{err}");
    }

    #[test]
    fn check_mode_probes_latest_without_downloading_or_modifying_files() {
        let server = MockServer::start();
        let base = server.base_url();
        let head_latest = server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{TEST_ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{base}/releases/download/v0.40.0/{TEST_ARCHIVE}"),
            );
        });
        let head_asset = server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/v0.40.0/{TEST_ARCHIVE}"));
            then.status(200);
        });
        let get_asset = server.mock(|when, then| {
            when.method(GET).path(format!("/v0.40.0/{TEST_ARCHIVE}"));
            then.status(200);
        });

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "existing").unwrap();

        let env = make_env(base, tmp.path(), &bin_dir, Some("v0.39.0"));
        let cmd = UpdateCommand {
            check: true,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None).unwrap();

        head_latest.assert_calls(1);
        head_asset.assert_calls(1);
        get_asset.assert_calls(0);
        assert_eq!(ops.stops, 0);
        assert_eq!(
            fs::read_to_string(bin_dir.join("peppy")).unwrap(),
            "existing"
        );
    }

    #[test]
    fn already_up_to_date_skips_download_and_service_restart() {
        let server = MockServer::start();
        let base = server.base_url();
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{TEST_ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{base}/releases/download/v0.39.0/{TEST_ARCHIVE}"),
            );
        });
        server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/v0.39.0/{TEST_ARCHIVE}"));
            then.status(200);
        });
        let get_asset = server.mock(|when, then| {
            when.method(GET).path(format!("/v0.39.0/{TEST_ARCHIVE}"));
            then.status(200);
        });

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let mut env = make_env(base, tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None).unwrap();

        get_asset.assert_calls(0);
        assert_eq!(ops.stops, 0);
    }

    #[test]
    fn development_build_refuses_in_place_update_but_allows_check() {
        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", Vec::new());

        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let env = make_env(server.base_url(), tmp.path(), &bin_dir, None);

        let check_cmd = UpdateCommand {
            check: true,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        execute_update(&check_cmd, &test_ctx(tmp.path()), &env, &mut ops, None)
            .expect("--check works on development builds");

        let update_cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let err = execute_update(&update_cmd, &test_ctx(tmp.path()), &env, &mut ops, None)
            .expect_err("dev build must refuse in-place update");
        assert!(err.to_string().contains("development build"), "{err}");
    }

    #[test]
    fn full_update_downloads_verifies_swaps_and_restarts_service() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "old-peppy").unwrap();
        fs::write(bin_dir.join("zenohd"), "old-zenohd").unwrap();
        fs::write(bin_dir.join("capnp"), "keep-me").unwrap();

        let log_path = tmp.path().join("invocations.log");
        let new_peppy = fake_peppy_script("v0.40.0", &log_path, 0);
        let tgz = archive::test_support::build_tgz(&[
            ("./bin/peppy", &new_peppy, 0o755),
            ("./bin/zenohd", b"new-zenohd", 0o755),
            ("./bin/lima/bin/limactl", b"limactl", 0o755),
        ]);

        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", tgz);

        let mut env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None)
            .expect("full update should succeed");

        assert_eq!(
            ops.stops, 1,
            "installed service must be stopped before swap"
        );
        assert_eq!(ops.restores, 0);
        assert_eq!(
            fs::read_to_string(bin_dir.join("zenohd")).unwrap(),
            "new-zenohd"
        );
        assert_eq!(
            fs::read_to_string(bin_dir.join("capnp")).unwrap(),
            "keep-me"
        );
        assert!(
            tmp.path().join("lima-data").is_dir(),
            "lima-data should be created when lima is shipped"
        );

        let calls: Vec<String> = fs::read_to_string(&log_path)
            .unwrap()
            .lines()
            .map(ToOwned::to_owned)
            .collect();
        assert_eq!(calls[0], "--version");
        #[cfg(target_os = "linux")]
        assert!(
            calls.iter().any(|line| line == "container status"),
            "{calls:?}"
        );
        assert!(calls.iter().any(|line| line == "repo init"), "{calls:?}");
        assert!(
            calls.iter().any(|line| line == "service install"),
            "{calls:?}"
        );
        assert!(calls.iter().any(|line| line == "repo list"), "{calls:?}");
        assert!(calls.iter().any(|line| line == "repo update"), "{calls:?}");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn failed_container_status_triggers_container_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let log_path = tmp.path().join("invocations.log");
        let new_peppy = fake_peppy_script("v0.40.0", &log_path, 1);
        let tgz = archive::test_support::build_tgz(&[("./bin/peppy", &new_peppy, 0o755)]);

        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", tgz);

        let env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None).unwrap();

        let calls = fs::read_to_string(&log_path).unwrap();
        assert!(calls.contains("container status"), "{calls}");
        assert!(calls.contains("container setup"), "{calls}");
    }

    #[test]
    fn active_user_nodes_prompt_and_abort_when_declined() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "old-peppy").unwrap();

        let server = MockServer::start();
        let base = server.base_url();
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{TEST_ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{base}/releases/download/v0.40.0/{TEST_ARCHIVE}"),
            );
        });
        server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/v0.40.0/{TEST_ARCHIVE}"));
            then.status(200);
        });
        let get_asset = server.mock(|when, then| {
            when.method(GET).path(format!("/v0.40.0/{TEST_ARCHIVE}"));
            then.status(200);
        });

        let mut env = make_env(base, tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;
        env.daemon_has_user_nodes_override = Some(true);
        env.terminal_mode = TerminalMode::Interactive;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        let mut decline = Cursor::new(b"n\n".to_vec());
        execute_update(
            &cmd,
            &test_ctx(tmp.path()),
            &env,
            &mut ops,
            Some(&mut decline),
        )
        .unwrap();

        get_asset.assert_calls(0);
        assert_eq!(ops.stops, 0);
        assert_eq!(
            fs::read_to_string(bin_dir.join("peppy")).unwrap(),
            "old-peppy"
        );
    }

    #[test]
    fn active_user_nodes_non_interactive_without_yes_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();

        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", Vec::new());

        let mut env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;
        env.daemon_has_user_nodes_override = Some(true);
        env.terminal_mode = TerminalMode::NonInteractive;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        let err = execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None)
            .expect_err("non-interactive update with active nodes must require --yes");
        assert!(err.to_string().contains("--yes"), "{err}");
        assert_eq!(ops.stops, 0);
    }

    #[test]
    fn broken_downloaded_binary_aborts_before_stopping_service_or_swapping() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "working-old-peppy").unwrap();
        fs::set_permissions(bin_dir.join("peppy"), fs::Permissions::from_mode(0o755)).unwrap();

        let tgz = archive::test_support::build_tgz(&[(
            "./bin/peppy",
            b"#!/bin/sh\necho 'glibc mismatch' >&2\nexit 1\n",
            0o755,
        )]);
        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", tgz);

        let mut env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: true,
        };
        let mut ops = RecordingServiceOps::default();
        let err = execute_update(&cmd, &test_ctx(tmp.path()), &env, &mut ops, None)
            .expect_err("broken downloaded binary must abort");

        assert!(err.to_string().contains("glibc mismatch"), "{err}");
        assert_eq!(
            ops.stops, 0,
            "daemon must never be stopped for a broken archive"
        );
        assert_eq!(
            fs::read_to_string(bin_dir.join("peppy")).unwrap(),
            "working-old-peppy"
        );
    }

    #[test]
    fn resolve_repo_url_uses_default_when_unset_or_blank() {
        assert_eq!(resolve_repo_url(None), DEFAULT_REPO_URL);
        assert_eq!(resolve_repo_url(Some(String::new())), DEFAULT_REPO_URL);
        assert_eq!(resolve_repo_url(Some("   ".to_owned())), DEFAULT_REPO_URL);
        assert_eq!(
            resolve_repo_url(Some("https://mirror.example.com/".to_owned())),
            "https://mirror.example.com"
        );
    }

    #[test]
    fn pinned_version_allows_downgrade_and_force_allows_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "v39-peppy").unwrap();

        let log_path = tmp.path().join("downgrade.log");
        let v38_script = fake_peppy_script("v0.38.0", &log_path, 0);
        let tgz = archive::test_support::build_tgz(&[("./bin/peppy", &v38_script, 0o755)]);

        let server = MockServer::start();
        mock_latest_release(&server, "v0.38.0", tgz);

        let env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        let mut ops = RecordingServiceOps::default();

        // Without --version or --force, newer-than-latest leaves installation untouched.
        let latest_cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        execute_update(&latest_cmd, &test_ctx(tmp.path()), &env, &mut ops, None).unwrap();
        assert_eq!(
            fs::read_to_string(bin_dir.join("peppy")).unwrap(),
            "v39-peppy"
        );

        // With explicit --version v0.38.0, downgrade proceeds.
        let downgrade_cmd = UpdateCommand {
            check: false,
            version: Some(ReleaseVersion::parse("v0.38.0").unwrap()),
            force: false,
            yes: false,
        };
        execute_update(&downgrade_cmd, &test_ctx(tmp.path()), &env, &mut ops, None).unwrap();
        assert_eq!(
            fs::read(bin_dir.join("peppy")).unwrap(),
            v38_script,
            "pinned version should replace installed binary"
        );

        // With --force when already at the target version, reinstall proceeds.
        fs::write(bin_dir.join("peppy"), "modified-v38").unwrap();
        let at_v38_env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.38.0"));
        let force_cmd = UpdateCommand {
            check: false,
            version: None,
            force: true,
            yes: false,
        };
        execute_update(
            &force_cmd,
            &test_ctx(tmp.path()),
            &at_v38_env,
            &mut ops,
            None,
        )
        .unwrap();
        assert_eq!(
            fs::read(bin_dir.join("peppy")).unwrap(),
            v38_script,
            "--force should reinstall even when already at latest"
        );
    }

    #[test]
    fn active_user_nodes_proceed_when_confirmed_interactively() {
        let tmp = tempfile::tempdir().unwrap();
        let bin_dir = tmp.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        fs::write(bin_dir.join("peppy"), "old-peppy").unwrap();

        let log_path = tmp.path().join("confirm.log");
        let new_peppy = fake_peppy_script("v0.40.0", &log_path, 0);
        let tgz = archive::test_support::build_tgz(&[("./bin/peppy", &new_peppy, 0o755)]);

        let server = MockServer::start();
        mock_latest_release(&server, "v0.40.0", tgz);

        let mut env = make_env(server.base_url(), tmp.path(), &bin_dir, Some("v0.39.0"));
        env.installed_service = InstalledService::CurrentLevel(tmp.path().join("peppy.service"));
        env.daemon_running = true;
        env.daemon_has_user_nodes_override = Some(true);
        env.terminal_mode = TerminalMode::Interactive;

        let cmd = UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        };
        let mut ops = RecordingServiceOps::default();
        let mut accept = Cursor::new(b"y\n".to_vec());
        execute_update(
            &cmd,
            &test_ctx(tmp.path()),
            &env,
            &mut ops,
            Some(&mut accept),
        )
        .unwrap();

        assert_eq!(ops.stops, 1);
        assert_eq!(fs::read(bin_dir.join("peppy")).unwrap(), new_peppy);
    }

    #[test]
    fn poll_until_ready_retries_until_success_and_reports_timeout_deterministically() {
        use std::cell::Cell;

        let mut attempts = 0usize;
        let sleeps = Cell::new(0u64);
        poll_until_ready(
            Duration::from_secs(10),
            || Duration::from_secs(sleeps.get()),
            || sleeps.set(sleeps.get() + 1),
            || {
                attempts += 1;
                if attempts < 3 {
                    Ok(Err("not ready".to_owned()))
                } else {
                    Ok(Ok(()))
                }
            },
            |msg| Error::ExecutionFailed(format!("timed out: {msg}")),
        )
        .expect("third attempt should succeed");
        assert_eq!(attempts, 3);
        assert_eq!(sleeps.get(), 2);

        let ticks = Cell::new(0u64);
        let err = poll_until_ready(
            Duration::from_secs(3),
            || Duration::from_secs(ticks.get()),
            || ticks.set(ticks.get() + 1),
            || Ok(Err("connection refused".to_owned())),
            |msg| Error::ExecutionFailed(format!("timed out: {msg}")),
        )
        .expect_err("reaching deadline must fail");
        assert_eq!(err.to_string(), "timed out: connection refused");
        assert_eq!(ticks.get(), 3);
    }
}
