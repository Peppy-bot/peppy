//! `peppy update`: check for and install Peppy releases in place.

mod archive;
mod http;
mod runner;
mod version;

use std::fmt::Write as _;
use std::fs;
use std::io::{self, BufRead, IsTerminal};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use daemon::state::DaemonState;
use daemon_config::consts::{PEPPY_GIT_TAG, PeppyDirs};

use super::Command;
use super::confirm::confirm_prompt;
use super::platform::{daemon_has_user_nodes, read_daemon_state};
use super::service::install::{InstalledService, detect_installed_service};
use crate::context::AppContext;
use crate::error::{Error, Result};
use crate::terminal::while_spinning;

use runner::{ChildIo, FailedCommand, HostPeppyRunner, PeppyRunner, run_peppy};
use version::{CurrentVersion, UpdateDecision, decide_update};
pub use version::{InvalidReleaseVersion, ReleaseVersion};

const DEFAULT_REPO_URL: &str = "https://peppy.bot";
const REPO_URL_ENV: &str = "PEPPY_REPOURL";
const NO_SERVICE_INSTALL_ENV: &str = "PEPPY_NO_SERVICE_INSTALL";
const NO_CONTAINER_SETUP_ENV: &str = "PEPPY_NO_CONTAINER_SETUP";
const NO_ROOT_INSTALL_ENV: &str = "PEPPY_NO_ROOT_INSTALL";

/// The command that installs a release of peppy without a peppy that runs.
const INSTALL_SCRIPT_COMMAND: &str = "curl -fsSL https://peppy.bot/install.sh | sh";

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
        execute_update(&self, ctx, &env, &mut HostPeppyRunner, None)
    }
}

/// What `peppy update` does about the container prerequisites after it
/// installs the binaries, as `install.sh` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ContainerSetupPolicy {
    /// Run `peppy container setup`, which checks each prerequisite and fixes
    /// what fails, with sudo when a fix needs it.
    RunSetup,
    /// Run `peppy container status --packages`, which uses no sudo and fails
    /// when a package that each container build needs is missing
    /// (`PEPPY_NO_ROOT_INSTALL`).
    CheckPackagesOnly,
    /// Do nothing: `PEPPY_NO_CONTAINER_SETUP` is set, or the host is not
    /// Linux (on macOS, containers run inside a Lima VM).
    Skip,
}

impl ContainerSetupPolicy {
    const fn for_host(is_linux: bool, skip: bool, no_root: bool) -> Self {
        if !is_linux || skip {
            Self::Skip
        } else if no_root {
            Self::CheckPackagesOnly
        } else {
            Self::RunSetup
        }
    }

    /// The arguments of the peppy command of this policy, or `None` when the
    /// policy runs no command.
    const fn command(self) -> Option<&'static [&'static str]> {
        match self {
            Self::RunSetup => Some(&["container", "setup"]),
            Self::CheckPackagesOnly => Some(&["container", "status", "--packages"]),
            Self::Skip => None,
        }
    }
}

/// Whether `peppy update` syncs the default repositories and manages the OS
/// background service, as `install.sh` does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServicePolicy {
    /// Sync the default repositories (`repo init`), and restart the
    /// background service when it is installed at the level of the update.
    ManageIfInstalled,
    /// Do neither (`PEPPY_NO_SERVICE_INSTALL`).
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
    /// The binary that runs this update. It stops and uninstalls the
    /// background service before the swap, and installs it again when the
    /// swap rolls back.
    current_peppy: PathBuf,
    peppy_dirs: PeppyDirs,
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
        let current_peppy = fs::canonicalize(&exe).unwrap_or(exe);
        let install_bin_dir = current_peppy
            .parent()
            .ok_or_else(|| {
                Error::ExecutionFailed(format!(
                    "Could not determine the installation directory of `{}`.",
                    current_peppy.display()
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
        let container_setup = ContainerSetupPolicy::for_host(
            cfg!(target_os = "linux"),
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
            current_peppy,
            peppy_dirs,
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
    runner: &mut dyn PeppyRunner,
    confirm_reader: Option<&mut dyn BufRead>,
) -> Result<()> {
    let resolved = while_spinning("Checking for updates", || {
        http::probe_release(&env.repo_url, &env.archive_name, cmd.version)
    })?;
    let decision = decide_update(
        &env.current_version,
        resolved.version,
        cmd.version.is_some(),
        cmd.force,
    );

    if cmd.check {
        print_check_result(&env.current_version, resolved.version, &decision);
        return Ok(());
    }
    match decision {
        UpdateDecision::DevelopmentBuild => {
            return Err(Error::ExecutionFailed(format!(
                "This peppy binary is a development build (version: unknown) and cannot be \
                 updated in place. The latest release is {}. Install a release with:\n  \
                 {INSTALL_SCRIPT_COMMAND}",
                resolved.version
            )));
        }
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
    // Held until the update ends, so a second `peppy update` cannot change
    // the installation or the service meanwhile.
    let install_lock = archive::lock_install_dir(&env.install_bin_dir)?;
    for problem in archive::recover_interrupted_updates(&env.install_bin_dir, &install_lock) {
        eprintln!("Warning: {problem}");
    }

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
        .prefix(archive::STAGING_DIR_PREFIX)
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
    verify_staged_binary(
        runner,
        &staged_bin_dir.join("peppy"),
        resolved.version,
        &env.repo_url,
    )?;

    if manage_service {
        stop_service_for_swap(runner, env)?;
    }

    if let Err(swap_error) = archive::swap_installation(&staged_bin_dir, &env.install_bin_dir) {
        if !manage_service {
            return Err(swap_error);
        }
        return Err(restart_previous_service(runner, env, swap_error));
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
    run_post_update(runner, &new_peppy, resolved.version, manage_service, env)?;

    println!(
        "Updated peppy from {} to {}.",
        env.current_version, resolved.version
    );
    print_post_update_notes(resolved.version, manage_service, env);

    Ok(())
}

fn print_check_result(current: &CurrentVersion, latest: ReleaseVersion, decision: &UpdateDecision) {
    println!("Current version: {current}");
    println!("Latest release:  {latest}");
    match decision {
        UpdateDecision::AlreadyUpToDate { .. } => println!("peppy is up to date."),
        UpdateDecision::NewerThanLatest { .. } => {
            println!("Current version is newer than the latest published release.");
        }
        UpdateDecision::Install { .. } => {
            println!("Update available. Run `peppy update` to install {latest}.");
        }
        UpdateDecision::DevelopmentBuild => {
            println!(
                "This is a development build. Install {latest} with:\n  {INSTALL_SCRIPT_COMMAND}"
            );
        }
    }
}

/// Runs `<staged peppy> --version` before the installation or the daemon
/// changes: the downloaded binary must run on this machine and be the
/// release that the update installs.
fn verify_staged_binary(
    runner: &mut dyn PeppyRunner,
    staged_peppy: &Path,
    expected: ReleaseVersion,
    repo_url: &str,
) -> Result<()> {
    let output =
        run_peppy(runner, staged_peppy, &["--version"], ChildIo::Captured).map_err(|failure| {
            Error::ExecutionFailed(format!(
                "The downloaded peppy binary does not run on this machine, so nothing changed. \
                 {failure}"
            ))
        })?;
    let reported = String::from_utf8_lossy(&output.stdout);
    if ReleaseVersion::from_version_output(&reported) == Some(expected) {
        return Ok(());
    }
    Err(Error::ExecutionFailed(format!(
        "The archive of release {expected} from {} holds a peppy binary that reports `{}`, so \
         nothing changed.",
        http::redact_url(repo_url),
        reported.trim()
    )))
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

/// Stops and uninstalls the background service with the binary that runs,
/// as `install.sh` does before it replaces the binaries.
///
/// When the daemon runs, a failed stop ends the update before anything
/// changes: the swap must not happen under a daemon that keeps running. Any
/// other failure is a warning, because `service install` after the swap
/// replaces the service definition.
fn stop_service_for_swap(runner: &mut dyn PeppyRunner, env: &UpdateEnvironment) -> Result<()> {
    let stopped = run_peppy(
        runner,
        &env.current_peppy,
        &["service", "stop"],
        ChildIo::Captured,
    );
    if let Err(failure) = stopped {
        if env.daemon_running {
            return Err(Error::ExecutionFailed(format!(
                "Could not stop the background service, so nothing changed. {failure}"
            )));
        }
        eprintln!("Warning: {failure}");
    }
    let uninstalled = run_peppy(
        runner,
        &env.current_peppy,
        &["service", "uninstall"],
        ChildIo::Captured,
    );
    if let Err(failure) = uninstalled {
        eprintln!("Warning: {failure}");
    }
    Ok(())
}

/// Installs and starts the background service again with the previous
/// binary, which the rollback of the swap put back, and adds to `swap_error`
/// when that fails.
fn restart_previous_service(
    runner: &mut dyn PeppyRunner,
    env: &UpdateEnvironment,
    swap_error: Error,
) -> Error {
    let restarted = run_peppy(
        runner,
        &env.current_peppy,
        &["service", "install"],
        ChildIo::Captured,
    );
    let Err(failure) = restarted else {
        return swap_error;
    };
    Error::ExecutionFailed(format!(
        "{swap_error}\nThe background service did not start again: {failure}\nStart it with: \
         `{} service install`",
        env.current_peppy.display()
    ))
}

/// A set-up step after the installation that did not complete, with the
/// commands that complete it.
struct IncompleteStep {
    failure: FailedCommand,
    /// The arguments of each peppy command that completes the step, in order.
    completed_by: Vec<String>,
}

impl IncompleteStep {
    /// A step that the run of its failed command again completes.
    fn retry(failure: FailedCommand) -> Self {
        let completed_by = vec![failure.args.clone()];
        Self {
            failure,
            completed_by,
        }
    }
}

/// Runs the set-up commands of `install.sh` with the new binary, so that the
/// container prerequisites, the default repositories, the background service,
/// and the repository indexes match the new release.
///
/// A failed step does not stop the steps after it, so the background service
/// that the update stopped always gets its `service install`. The error names
/// each step that did not complete and the commands that complete it.
fn run_post_update(
    runner: &mut dyn PeppyRunner,
    new_peppy: &Path,
    target: ReleaseVersion,
    manage_service: bool,
    env: &UpdateEnvironment,
) -> Result<()> {
    let mut incomplete = Vec::new();
    if let Some(args) = env.container_setup.command() {
        let container = run_peppy(runner, new_peppy, args, ChildIo::Terminal);
        incomplete.extend(container.err().map(IncompleteStep::retry));
    }
    if env.service_policy == ServicePolicy::ManageIfInstalled {
        let init = run_peppy(runner, new_peppy, &["repo", "init"], ChildIo::Captured);
        incomplete.extend(init.err().map(IncompleteStep::retry));
    }
    if manage_service {
        incomplete.extend(restart_service_and_refresh_indexes(runner, new_peppy, env).err());
    }
    post_update_result(target, new_peppy, &incomplete)
}

/// Installs and starts the background service with the new binary, waits
/// for the daemon, and refreshes the repository indexes.
fn restart_service_and_refresh_indexes(
    runner: &mut dyn PeppyRunner,
    new_peppy: &Path,
    env: &UpdateEnvironment,
) -> std::result::Result<(), IncompleteStep> {
    run_peppy(
        runner,
        new_peppy,
        &["service", "install"],
        ChildIo::Captured,
    )
    .map_err(|failure| IncompleteStep {
        failure,
        completed_by: vec!["service install".to_owned(), "repo update".to_owned()],
    })?;
    wait_for_daemon_ready(runner, new_peppy, env).map_err(IncompleteStep::retry)?;
    run_peppy(runner, new_peppy, &["repo", "update"], ChildIo::Terminal)
        .map_err(IncompleteStep::retry)?;
    Ok(())
}

fn post_update_result(
    target: ReleaseVersion,
    new_peppy: &Path,
    incomplete: &[IncompleteStep],
) -> Result<()> {
    if incomplete.is_empty() {
        return Ok(());
    }
    let mut message =
        format!("peppy {target} is installed, but these set-up steps did not complete:");
    for step in incomplete {
        let commands: Vec<String> = step
            .completed_by
            .iter()
            .map(|args| format!("`{} {args}`", new_peppy.display()))
            .collect();
        let _ = write!(
            message,
            "\n  {}\n    Complete it with: {}",
            step.failure,
            commands.join(", then ")
        );
    }
    Err(Error::ExecutionFailed(message))
}

/// Whether the restarted daemon answers a request.
enum Readiness {
    Ready,
    /// Not yet, with the stderr of the last probe.
    NotReady(String),
}

/// Polls `<new_peppy> repo list` until the restarted daemon accepts requests or
/// `env.daemon_ready_deadline` expires. The failure names `repo update`, the
/// command that the wait is for.
fn wait_for_daemon_ready(
    runner: &mut dyn PeppyRunner,
    new_peppy: &Path,
    env: &UpdateEnvironment,
) -> std::result::Result<(), FailedCommand> {
    let did_not_run = |reason: String| FailedCommand {
        args: "repo update".to_owned(),
        reason: format!("it did not run, because {reason}"),
    };
    let probe = || {
        let start = Instant::now();
        poll_until_ready(
            env.daemon_ready_deadline,
            || start.elapsed(),
            || thread::sleep(env.daemon_ready_poll_interval),
            || probe_daemon_once(runner, new_peppy).map_err(&did_not_run),
            |last_error| {
                let detail = if last_error.is_empty() {
                    String::new()
                } else {
                    format!(" ({last_error})")
                };
                did_not_run(format!(
                    "the peppy daemon did not become ready in time{detail}"
                ))
            },
        )
    };

    while_spinning("Waiting for the peppy daemon to start", probe)
}

/// Runs `<new_peppy> repo list` once. Fails only when the probe cannot start.
fn probe_daemon_once(
    runner: &mut dyn PeppyRunner,
    new_peppy: &Path,
) -> std::result::Result<Readiness, String> {
    let output = runner
        .run(new_peppy, &["repo", "list"], ChildIo::Captured)
        .map_err(|e| format!("the daemon probe `peppy repo list` could not start: {e}"))?;
    if output.status.success() {
        return Ok(Readiness::Ready);
    }
    Ok(Readiness::NotReady(
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

/// Deterministic poll loop parameterized over elapsed-time and sleep hooks so
/// readiness retries and timeouts can be tested without real wall-clock waits.
fn poll_until_ready<E>(
    deadline: Duration,
    mut elapsed: impl FnMut() -> Duration,
    mut sleep: impl FnMut(),
    mut probe: impl FnMut() -> std::result::Result<Readiness, E>,
    on_timeout: impl FnOnce(&str) -> E,
) -> std::result::Result<(), E> {
    loop {
        match probe()? {
            Readiness::Ready => return Ok(()),
            Readiness::NotReady(last_error) => {
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
    use httpmock::{Mock, MockServer};
    use runner::test_support::ScriptedRunner;
    use std::io::Cursor;

    const TEST_ARCHIVE: &str = "peppy-x86_64-unknown-linux-gnu.tgz";

    /// The commands of an update that installs and restarts the service.
    const FULL_UPDATE_COMMANDS: [&str; 8] = [
        "--version",
        "service stop",
        "service uninstall",
        "container setup",
        "repo init",
        "service install",
        "repo list",
        "repo update",
    ];

    /// The mocks of a release server whose latest release is `tag`.
    struct ReleaseMocks<'a> {
        head_latest: Mock<'a>,
        head_asset: Mock<'a>,
        get_asset: Mock<'a>,
    }

    fn mock_release<'a>(server: &'a MockServer, tag: &str, tgz: Vec<u8>) -> ReleaseMocks<'a> {
        let base = server.base_url();
        let length = tgz.len().to_string();
        let head_latest = server.mock(|when, then| {
            when.method(HEAD).path(format!("/latest/{TEST_ARCHIVE}"));
            then.status(302).header(
                "Location",
                format!("{base}/releases/download/{tag}/{TEST_ARCHIVE}"),
            );
        });
        let head_asset = server.mock(|when, then| {
            when.method(HEAD)
                .path(format!("/releases/download/{tag}/{TEST_ARCHIVE}"));
            then.status(200).header("Content-Length", length.clone());
        });
        server.mock(|when, then| {
            when.method(HEAD).path(format!("/{tag}/{TEST_ARCHIVE}"));
            then.status(200).header("Content-Length", length.clone());
        });
        let get_asset = server.mock(|when, then| {
            when.method(GET).path(format!("/{tag}/{TEST_ARCHIVE}"));
            then.status(200)
                .header("Content-Length", length.clone())
                .body(tgz);
        });
        ReleaseMocks {
            head_latest,
            head_asset,
            get_asset,
        }
    }

    /// The archive of a release with `bin/peppy` and `bin/zenohd`.
    fn release_tgz(peppy: &[u8]) -> Vec<u8> {
        archive::test_support::build_tgz(&[
            ("./bin/peppy", peppy, 0o755),
            ("./bin/zenohd", b"new-zenohd", 0o755),
        ])
    }

    /// An installation directory `<tmp>/bin` with an old `peppy` and
    /// `zenohd`, and an unmanaged `capnp`.
    struct Installation {
        tmp: tempfile::TempDir,
        bin_dir: PathBuf,
    }

    impl Installation {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let bin_dir = tmp.path().join("bin");
            fs::create_dir_all(&bin_dir).unwrap();
            fs::write(bin_dir.join("peppy"), "old-peppy").unwrap();
            fs::write(bin_dir.join("zenohd"), "old-zenohd").unwrap();
            fs::write(bin_dir.join("capnp"), "keep-me").unwrap();
            Self { tmp, bin_dir }
        }

        fn env(&self, server: &MockServer, current_tag: Option<&str>) -> UpdateEnvironment {
            UpdateEnvironment {
                current_version: CurrentVersion::from_git_tag(current_tag),
                current_peppy: self.bin_dir.join("peppy"),
                peppy_dirs: PeppyDirs::new(self.tmp.path()),
                install_bin_dir: self.bin_dir.clone(),
                repo_url: server.base_url(),
                archive_name: TEST_ARCHIVE.to_owned(),
                service_policy: ServicePolicy::ManageIfInstalled,
                container_setup: ContainerSetupPolicy::RunSetup,
                terminal_mode: TerminalMode::NonInteractive,
                installed_service: InstalledService::None,
                daemon_running: false,
                daemon_has_user_nodes_override: Some(false),
                daemon_ready_deadline: Duration::from_secs(5),
                daemon_ready_poll_interval: Duration::from_millis(10),
            }
        }

        /// The environment of a host whose user-level service runs.
        fn env_with_running_service(
            &self,
            server: &MockServer,
            current_tag: Option<&str>,
        ) -> UpdateEnvironment {
            UpdateEnvironment {
                installed_service: InstalledService::CurrentLevel(
                    self.tmp.path().join("peppy.service"),
                ),
                daemon_running: true,
                ..self.env(server, current_tag)
            }
        }

        fn ctx(&self) -> Arc<AppContext> {
            Arc::new(AppContext::new(self.tmp.path()))
        }

        fn read(&self, name: &str) -> String {
            fs::read_to_string(self.bin_dir.join(name)).unwrap()
        }

        /// The names in the installation directory, in order.
        fn names(&self) -> Vec<String> {
            let mut names: Vec<String> = fs::read_dir(&self.bin_dir)
                .unwrap()
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    const fn update() -> UpdateCommand {
        UpdateCommand {
            check: false,
            version: None,
            force: false,
            yes: false,
        }
    }

    fn run(
        installation: &Installation,
        cmd: &UpdateCommand,
        env: &UpdateEnvironment,
        runner: &mut ScriptedRunner,
    ) -> Result<()> {
        execute_update(cmd, &installation.ctx(), env, runner, None)
    }

    #[test]
    fn update_rejects_core_node_override() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = Arc::new(AppContext::new(tmp.path()).with_core_node_override(Some(
            config::runtime::CoreNodeName::parse("robot-1").unwrap(),
        )));
        let cmd = UpdateCommand {
            check: true,
            ..update()
        };
        let err = cmd.execute(&ctx).expect_err("--core-node must be refused");
        assert!(err.to_string().contains("--core-node robot-1"), "{err}");
    }

    #[test]
    fn check_mode_probes_latest_without_downloading_or_modifying_files() {
        let installation = Installation::new();
        let server = MockServer::start();
        let mocks = mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let cmd = UpdateCommand {
            check: true,
            ..update()
        };
        run(&installation, &cmd, &env, &mut runner).unwrap();

        mocks.head_latest.assert_calls(1);
        mocks.head_asset.assert_calls(1);
        mocks.get_asset.assert_calls(0);
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);
        assert_eq!(installation.read("peppy"), "old-peppy");
    }

    #[test]
    fn already_up_to_date_skips_download_and_service_restart() {
        let installation = Installation::new();
        let server = MockServer::start();
        let mocks = mock_release(&server, "v0.39.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.39.0");

        run(&installation, &update(), &env, &mut runner).unwrap();

        mocks.get_asset.assert_calls(0);
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);
    }

    #[test]
    fn development_build_refuses_in_place_update_but_allows_check() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env(&server, None);
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let check_cmd = UpdateCommand {
            check: true,
            ..update()
        };
        run(&installation, &check_cmd, &env, &mut runner)
            .expect("--check works on development builds");

        let force_cmd = UpdateCommand {
            force: true,
            ..update()
        };
        let err = run(&installation, &force_cmd, &env, &mut runner)
            .expect_err("dev build must refuse in-place update, even with --force");
        assert!(err.to_string().contains("development build"), "{err}");
        assert!(err.to_string().contains(INSTALL_SCRIPT_COMMAND), "{err}");
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);
    }

    #[test]
    fn full_update_downloads_verifies_swaps_and_restarts_service() {
        let installation = Installation::new();
        // What an update that a Ctrl-C stopped left behind.
        let leftover = installation
            .bin_dir
            .join(format!("{}old", archive::STAGING_DIR_PREFIX));
        fs::create_dir_all(leftover.join("bin")).unwrap();
        let tgz = archive::test_support::build_tgz(&[
            ("./bin/peppy", b"new-peppy", 0o755),
            ("./bin/zenohd", b"new-zenohd", 0o755),
            ("./bin/lima/bin/limactl", b"limactl", 0o755),
        ]);
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", tgz);
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0\n");

        run(&installation, &update(), &env, &mut runner).expect("full update should succeed");

        assert_eq!(runner.commands(), FULL_UPDATE_COMMANDS);
        let staged_peppy = &runner.calls[0].0;
        assert!(
            staged_peppy.starts_with(&installation.bin_dir)
                && staged_peppy.ends_with("bin/peppy")
                && *staged_peppy != env.current_peppy,
            "the staged binary is verified: {staged_peppy:?}"
        );
        for (program, args) in &runner.calls[1..] {
            assert_eq!(program, &installation.bin_dir.join("peppy"), "{args}");
        }
        assert_eq!(installation.read("peppy"), "new-peppy");
        assert_eq!(installation.read("zenohd"), "new-zenohd");
        assert_eq!(installation.read("capnp"), "keep-me");
        assert_eq!(
            installation.names(),
            ["capnp", "lima", "peppy", "zenohd"],
            "no staging, backup, or leftover directory stays"
        );
        assert!(
            installation.tmp.path().join("lima-data").is_dir(),
            "lima-data should be created when lima is shipped"
        );
    }

    /// A step after the swap that fails does not leave the service
    /// uninstalled: the service still restarts, and the error tells each
    /// step to complete.
    #[test]
    fn failed_container_setup_and_repo_init_still_restart_the_service() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0")
            .failing("container setup", 1)
            .failing("repo init", 2);

        let err = run(&installation, &update(), &env, &mut runner)
            .expect_err("the failed steps are reported");

        assert_eq!(runner.commands(), FULL_UPDATE_COMMANDS);
        let message = err.to_string();
        let new_peppy = installation.bin_dir.join("peppy").display().to_string();
        assert!(message.contains("peppy v0.40.0 is installed"), "{message}");
        assert!(
            message.contains("`peppy container setup`: it ended with")
                && message.contains("container setup broke"),
            "{message}"
        );
        assert!(
            message.contains(&format!("Complete it with: `{new_peppy} container setup`")),
            "{message}"
        );
        assert!(
            message.contains(&format!("Complete it with: `{new_peppy} repo init`")),
            "{message}"
        );
        assert_eq!(installation.read("peppy"), "new-peppy");
    }

    #[test]
    fn failed_service_install_tells_to_install_it_then_refresh_the_indexes() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0").failing("service install", 1);

        let err = run(&installation, &update(), &env, &mut runner)
            .expect_err("the failed service install is reported");

        assert_eq!(runner.commands(), &FULL_UPDATE_COMMANDS[..6]);
        let new_peppy = installation.bin_dir.join("peppy").display().to_string();
        assert!(
            err.to_string().contains(&format!(
                "Complete it with: `{new_peppy} service install`, then `{new_peppy} repo update`"
            )),
            "{err}"
        );
    }

    #[test]
    fn daemon_that_does_not_become_ready_skips_repo_update_and_says_so() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            daemon_ready_deadline: Duration::ZERO,
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0").failing("repo list", 1);

        let err =
            run(&installation, &update(), &env, &mut runner).expect_err("the daemon is not ready");

        assert_eq!(runner.commands(), &FULL_UPDATE_COMMANDS[..7]);
        let message = err.to_string();
        assert!(
            message.contains(
                "`peppy repo update`: it did not run, because the peppy daemon did \
                              not become ready in time (repo list broke)"
            ),
            "{message}"
        );
    }

    /// `PEPPY_NO_ROOT_INSTALL` checks the packages with no sudo, and
    /// `PEPPY_NO_SERVICE_INSTALL` leaves the repositories and the service
    /// alone, as in `install.sh`.
    #[test]
    fn no_root_and_no_service_policies_check_packages_only() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            container_setup: ContainerSetupPolicy::CheckPackagesOnly,
            service_policy: ServicePolicy::Skip,
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        run(&installation, &update(), &env, &mut runner).unwrap();

        assert_eq!(
            runner.commands(),
            ["--version", "container status --packages"]
        );
        assert_eq!(installation.read("peppy"), "new-peppy");
    }

    #[test]
    fn missing_packages_under_no_root_policy_fail_the_update() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            container_setup: ContainerSetupPolicy::CheckPackagesOnly,
            ..installation.env(&server, Some("v0.39.0"))
        };
        let mut runner =
            ScriptedRunner::reporting("peppy v0.40.0").failing("container status --packages", 1);

        let err = run(&installation, &update(), &env, &mut runner)
            .expect_err("missing packages fail the update");

        assert!(
            err.to_string()
                .contains("`peppy container status --packages`: it ended with"),
            "{err}"
        );
        assert_eq!(
            runner.commands(),
            ["--version", "container status --packages", "repo init"]
        );
    }

    #[test]
    fn container_setup_policy_follows_the_host_and_the_installer_variables() {
        for (is_linux, skip, no_root, expected) in [
            (true, false, false, ContainerSetupPolicy::RunSetup),
            (true, false, true, ContainerSetupPolicy::CheckPackagesOnly),
            (true, true, true, ContainerSetupPolicy::Skip),
            (true, true, false, ContainerSetupPolicy::Skip),
            (false, false, false, ContainerSetupPolicy::Skip),
            (false, false, true, ContainerSetupPolicy::Skip),
        ] {
            assert_eq!(
                ContainerSetupPolicy::for_host(is_linux, skip, no_root),
                expected,
                "linux {is_linux}, skip {skip}, no root {no_root}"
            );
        }
    }

    #[test]
    fn active_user_nodes_prompt_and_abort_when_declined() {
        let installation = Installation::new();
        let server = MockServer::start();
        let mocks = mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            daemon_has_user_nodes_override: Some(true),
            terminal_mode: TerminalMode::Interactive,
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let mut decline = Cursor::new(b"n\n".to_vec());
        execute_update(
            &update(),
            &installation.ctx(),
            &env,
            &mut runner,
            Some(&mut decline),
        )
        .unwrap();

        mocks.get_asset.assert_calls(0);
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);
        assert_eq!(installation.read("peppy"), "old-peppy");
    }

    #[test]
    fn active_user_nodes_proceed_when_confirmed_interactively() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            daemon_has_user_nodes_override: Some(true),
            terminal_mode: TerminalMode::Interactive,
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let mut accept = Cursor::new(b"y\n".to_vec());
        execute_update(
            &update(),
            &installation.ctx(),
            &env,
            &mut runner,
            Some(&mut accept),
        )
        .unwrap();

        assert_eq!(runner.commands(), FULL_UPDATE_COMMANDS);
        assert_eq!(installation.read("peppy"), "new-peppy");
    }

    #[test]
    fn active_user_nodes_non_interactive_without_yes_fails_fast() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            daemon_has_user_nodes_override: Some(true),
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let err = run(&installation, &update(), &env, &mut runner)
            .expect_err("non-interactive update with active nodes must require --yes");

        assert!(err.to_string().contains("--yes"), "{err}");
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);
    }

    #[test]
    fn broken_downloaded_binary_aborts_before_stopping_service_or_swapping() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("").failing("--version", 1);
        let cmd = UpdateCommand {
            yes: true,
            ..update()
        };

        let err = run(&installation, &cmd, &env, &mut runner)
            .expect_err("broken downloaded binary must abort");

        assert!(
            err.to_string().contains("does not run on this machine"),
            "{err}"
        );
        assert!(err.to_string().contains("--version broke"), "{err}");
        assert_eq!(runner.commands(), ["--version"]);
        assert_eq!(installation.read("peppy"), "old-peppy");
        assert_eq!(installation.names(), ["capnp", "peppy", "zenohd"]);
    }

    #[test]
    fn downloaded_binary_of_another_release_is_refused() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        // A mirror serves the archive of v0.39.0 under v0.40.0.
        let mut runner = ScriptedRunner::reporting("peppy v0.39.0\n");

        let err = run(&installation, &update(), &env, &mut runner)
            .expect_err("a binary of another release must abort");

        assert!(
            err.to_string()
                .contains("holds a peppy binary that reports `peppy v0.39.0`"),
            "{err}"
        );
        assert_eq!(runner.commands(), ["--version"]);
        assert_eq!(installation.read("peppy"), "old-peppy");
    }

    #[test]
    fn failed_stop_of_a_running_daemon_ends_the_update_before_any_change() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0").failing("service stop", 1);

        let err =
            run(&installation, &update(), &env, &mut runner).expect_err("the daemon still runs");

        assert!(
            err.to_string()
                .contains("Could not stop the background service"),
            "{err}"
        );
        assert!(err.to_string().contains("service stop broke"), "{err}");
        assert_eq!(runner.commands(), ["--version", "service stop"]);
        assert_eq!(installation.read("peppy"), "old-peppy");
    }

    /// With no daemon that runs, the stop has nothing to stop: its failure
    /// (`launchctl stop` of a service that is not loaded) is a warning.
    #[test]
    fn failed_stop_without_a_running_daemon_continues_the_update() {
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = UpdateEnvironment {
            daemon_running: false,
            ..installation.env_with_running_service(&server, Some("v0.39.0"))
        };
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0")
            .failing("service stop", 1)
            .failing("service uninstall", 1);

        run(&installation, &update(), &env, &mut runner).expect("the update continues");

        assert_eq!(runner.commands(), FULL_UPDATE_COMMANDS);
        assert_eq!(installation.read("peppy"), "new-peppy");
    }

    /// The installation holds a directory where the release holds the file
    /// `zenohd`: the swap fails after it replaced `peppy`, the rollback puts
    /// the old `peppy` back, and the previous binary starts the service again.
    fn installation_whose_swap_fails() -> Installation {
        let installation = Installation::new();
        fs::remove_file(installation.bin_dir.join("zenohd")).unwrap();
        fs::create_dir_all(installation.bin_dir.join("zenohd")).unwrap();
        fs::write(installation.bin_dir.join("zenohd/keep"), "a directory").unwrap();
        installation
    }

    #[test]
    fn failed_swap_restores_the_previous_binaries_and_restarts_the_service() {
        let installation = installation_whose_swap_fails();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0");

        let err = run(&installation, &update(), &env, &mut runner).expect_err("the swap fails");

        assert!(err.to_string().contains("is a directory"), "{err}");
        assert!(err.to_string().contains("back in place"), "{err}");
        assert_eq!(
            runner.commands(),
            [
                "--version",
                "service stop",
                "service uninstall",
                "service install"
            ]
        );
        assert_eq!(runner.calls[3].0, env.current_peppy);
        assert_eq!(installation.read("peppy"), "old-peppy");
        assert_eq!(installation.names(), ["capnp", "peppy", "zenohd"]);
    }

    #[test]
    fn failed_swap_with_a_failed_restart_tells_how_to_start_the_service() {
        let installation = installation_whose_swap_fails();
        let server = MockServer::start();
        mock_release(&server, "v0.40.0", release_tgz(b"new-peppy"));
        let env = installation.env_with_running_service(&server, Some("v0.39.0"));
        let mut runner = ScriptedRunner::reporting("peppy v0.40.0").failing("service install", 1);

        let err = run(&installation, &update(), &env, &mut runner).expect_err("the swap fails");

        let message = err.to_string();
        assert!(message.contains("is a directory"), "{message}");
        assert!(
            message.contains("The background service did not start again")
                && message.contains("service install broke"),
            "{message}"
        );
        assert!(
            message.contains(&format!(
                "Start it with: `{} service install`",
                env.current_peppy.display()
            )),
            "{message}"
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
        let installation = Installation::new();
        let server = MockServer::start();
        mock_release(&server, "v0.38.0", release_tgz(b"v38-peppy"));
        let env = installation.env(&server, Some("v0.39.0"));

        // Without --version or --force, newer-than-latest leaves installation untouched.
        let mut runner = ScriptedRunner::reporting("peppy v0.38.0");
        run(&installation, &update(), &env, &mut runner).unwrap();
        assert_eq!(installation.read("peppy"), "old-peppy");
        assert!(runner.calls.is_empty(), "{:?}", runner.calls);

        // With explicit --version v0.38.0, downgrade proceeds.
        let downgrade_cmd = UpdateCommand {
            version: Some(ReleaseVersion::parse("v0.38.0").unwrap()),
            ..update()
        };
        run(&installation, &downgrade_cmd, &env, &mut runner).unwrap();
        assert_eq!(
            installation.read("peppy"),
            "v38-peppy",
            "pinned version should replace installed binary"
        );

        // With --force when already at the target version, reinstall proceeds.
        fs::write(installation.bin_dir.join("peppy"), "modified-v38").unwrap();
        let at_v38_env = installation.env(&server, Some("v0.38.0"));
        let force_cmd = UpdateCommand {
            force: true,
            ..update()
        };
        run(&installation, &force_cmd, &at_v38_env, &mut runner).unwrap();
        assert_eq!(
            installation.read("peppy"),
            "v38-peppy",
            "--force should reinstall even when already at latest"
        );
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
                    Ok::<_, String>(Readiness::NotReady("not ready".to_owned()))
                } else {
                    Ok(Readiness::Ready)
                }
            },
            |msg| format!("timed out: {msg}"),
        )
        .expect("third attempt should succeed");
        assert_eq!(attempts, 3);
        assert_eq!(sleeps.get(), 2);

        let ticks = Cell::new(0u64);
        let err = poll_until_ready(
            Duration::from_secs(3),
            || Duration::from_secs(ticks.get()),
            || ticks.set(ticks.get() + 1),
            || Ok(Readiness::NotReady("connection refused".to_owned())),
            |msg| format!("timed out: {msg}"),
        )
        .expect_err("reaching deadline must fail");
        assert_eq!(err, "timed out: connection refused");
        assert_eq!(ticks.get(), 3);

        let err = poll_until_ready(
            Duration::from_secs(3),
            || Duration::ZERO,
            || {},
            || Err::<Readiness, _>("could not start".to_owned()),
            |msg| format!("timed out: {msg}"),
        )
        .expect_err("a probe that cannot start fails at once");
        assert_eq!(err, "could not start");
    }
}
