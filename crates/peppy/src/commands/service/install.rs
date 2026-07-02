use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceLevel, ServiceManager,
    ServiceManagerKind, ServiceStartCtx, ServiceStopCtx, ServiceUninstallCtx, TypedServiceManager,
};

use super::Command;
use crate::context::AppContext;
use crate::error::{Error, Result};

fn service_label(kind: ServiceManagerKind) -> &'static str {
    match kind {
        ServiceManagerKind::Launchd => "bot.peppy",
        _ => "peppy",
    }
}

pub struct InstallCommand {}

impl Command for InstallCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        install_peppy_daemon(None).map(|_| ())
    }
}

pub struct StopCommand {}

impl Command for StopCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        stop_peppy_daemon()
    }
}

pub struct UninstallCommand {}

impl Command for UninstallCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        uninstall_peppy_daemon()
    }
}

/// Maps a service-manager error into the CLI's `ExecutionFailed` error.
fn execution_failed(e: impl std::fmt::Display) -> Error {
    Error::ExecutionFailed(e.to_string())
}

pub fn stop_peppy_daemon() -> Result<()> {
    let (manager, kind) = create_service_manager()?;
    let label: ServiceLabel = service_label(kind).parse()?;
    manager
        .stop(ServiceStopCtx { label })
        .map_err(execution_failed)
}

pub fn uninstall_peppy_daemon() -> Result<()> {
    let (manager, kind) = create_service_manager()?;
    let label: ServiceLabel = service_label(kind).parse()?;
    manager
        .uninstall(ServiceUninstallCtx { label })
        .map_err(execution_failed)
}

pub fn install_peppy_daemon(service_dir_override: Option<PathBuf>) -> Result<PathBuf> {
    let kind = ServiceManagerKind::native()?;
    let label: ServiceLabel = service_label(kind).parse()?;
    let program_path = std::env::current_exe()?;
    let working_dir = program_path
        .parent()
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"));
    let ctx = make_install_context(
        label.clone(),
        program_path,
        working_dir,
        service_dir_override.is_none(),
    )?;

    if let Some(dir) = service_dir_override {
        return write_service_definition(kind, &dir, &ctx);
    }

    let (manager, kind) = create_service_manager()?;
    let manager_level = preferred_service_level(kind);

    manager
        .install(ctx)
        .map_err(execution_failed)?;

    manager
        .start(ServiceStartCtx {
            label: label.clone(),
        })
        .map_err(execution_failed)?;

    default_service_path(kind, manager_level, &label)
}

fn make_install_context(
    label: ServiceLabel,
    program: PathBuf,
    working_dir: PathBuf,
    autostart: bool,
) -> Result<ServiceInstallCtx> {
    Ok(ServiceInstallCtx {
        label,
        program,
        args: vec![OsString::from("service"), OsString::from("serve")],
        contents: None,
        username: None,
        working_directory: Some(working_dir),
        environment: Some(service_environment(
            std::env::var("PATH").ok(),
            std::env::var_os(config::consts::PEPPY_HOME_ENV),
        )),
        autostart,
        // Crash-only supervision: the daemon supervises its own NORMAL restarts
        // in-process (a namespace change rebuilds the generation under the same
        // PID), so the OS supervisor must recover ONLY a genuine crash or the
        // `exit(RESTART_EXIT_CODE)` fallback (port-stuck / flap-cap), never a
        // clean `service stop` (exit 0). `RestartPolicy::OnFailure` gives exactly
        // that on both platforms via the `service_manager` crate (verified against
        // 0.11): systemd `Restart=on-failure`, launchd `KeepAlive { SuccessfulExit
        // = false }`. A clean stop therefore stays stopped.
        //
        // FOLLOW-UP: systemd's default `StartLimitBurst` could give up after a few
        // rapid `exit(RESTART_EXIT_CODE)` fallbacks; the in-process flap backstop
        // already caps that, but `StartLimitIntervalSec=0` would also disable the
        // unit-side limit. The `service_manager` 0.11 generic API does not expose
        // it (its own TODO maps `reset_after_secs` -> `StartLimitIntervalSec`), so
        // it would require overriding `ServiceInstallCtx.contents` with a fully
        // rendered unit; deferred until flapping is actually observed.
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(5),
            max_retries: None,
            reset_after_secs: None,
        },
    })
}

/// Builds the environment the installed service runs under.
///
/// Kept pure (inputs passed in rather than read from the process env) so the
/// `PEPPY_HOME` precedence can be unit-tested without mutating global state,
/// mirroring `daemon_config::consts::resolve_root`.
fn service_environment(
    path: Option<String>,
    peppy_home: Option<OsString>,
) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = Vec::new();

    // Include the user's PATH so that build_cmd/run_cmd can find tools like cargo, python, etc.
    if let Some(path) = path {
        environment.push(("PATH".to_string(), path));
    }

    // Propagate PEPPY_HOME so the daemon resolves the same data root as the CLI
    // that installed it. systemd/launchd start the service with a clean
    // environment, so without this the daemon falls back to the default
    // `~/.peppy` while a caller that set PEPPY_HOME (CI per-run isolation or a
    // custom install prefix) reads daemon state from the override path — the two
    // never find each other and the install's readiness probe times out. Empty
    // is treated as unset, matching `daemon_config::consts::peppy_root_dir`.
    if let Some(home) = peppy_home.filter(|value| !value.is_empty()) {
        environment.push((
            config::consts::PEPPY_HOME_ENV.to_string(),
            home.to_string_lossy().into_owned(),
        ));
    }

    environment
}

fn write_service_definition(
    kind: ServiceManagerKind,
    target_dir: &Path,
    ctx: &ServiceInstallCtx,
) -> Result<PathBuf> {
    match kind {
        ServiceManagerKind::Systemd => write_systemd_service(target_dir, ctx),
        ServiceManagerKind::Launchd => write_launchd_service(target_dir, ctx),
        other => Err(Error::ExecutionFailed(format!(
            "Unsupported service manager: {other:?}"
        ))),
    }
}

fn write_systemd_service(target_dir: &Path, ctx: &ServiceInstallCtx) -> Result<PathBuf> {
    let service_name = format!("{}.service", ctx.label.to_qualified_name());
    let service_path = target_dir.join(service_name);
    if let Some(parent) = service_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&service_path, render_systemd_service(ctx))?;
    Ok(service_path)
}

fn write_launchd_service(target_dir: &Path, ctx: &ServiceInstallCtx) -> Result<PathBuf> {
    let plist_name = format!("{}.plist", ctx.label.to_qualified_name());
    let plist_path = target_dir.join(plist_name);
    if let Some(parent) = plist_path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&plist_path, render_launchd_plist(ctx))?;
    Ok(plist_path)
}

fn default_service_path(
    kind: ServiceManagerKind,
    level: ServiceLevel,
    label: &ServiceLabel,
) -> Result<PathBuf> {
    default_service_path_with_level(kind, level, label)
}

fn default_service_path_with_level(
    kind: ServiceManagerKind,
    level: ServiceLevel,
    label: &ServiceLabel,
) -> Result<PathBuf> {
    match kind {
        ServiceManagerKind::Systemd => {
            let service_name = format!("{}.service", label.to_qualified_name());
            match level {
                ServiceLevel::System => Ok(systemd_unit_dir().join(service_name)),
                ServiceLevel::User => Ok(systemd_user_unit_dir()?.join(service_name)),
            }
        }
        ServiceManagerKind::Launchd => {
            let plist_name = format!("{}.plist", label.to_qualified_name());
            match level {
                ServiceLevel::System => {
                    Ok(PathBuf::from("/Library/LaunchDaemons").join(plist_name))
                }
                ServiceLevel::User => Ok(launchd_user_agent_dir()?.join(plist_name)),
            }
        }
        other => Err(Error::ExecutionFailed(format!(
            "Unsupported service manager: {other:?}"
        ))),
    }
}

fn create_service_manager() -> Result<(TypedServiceManager, ServiceManagerKind)> {
    let kind = ServiceManagerKind::native()?;
    let manager_level = preferred_service_level(kind);

    #[cfg(target_os = "linux")]
    if matches!(kind, ServiceManagerKind::Systemd) && matches!(manager_level, ServiceLevel::User) {
        ensure_systemd_user_env()?;
    }

    let mut manager = TypedServiceManager::target(kind);
    if manager.level() != manager_level {
        manager
            .set_level(manager_level)
            .map_err(execution_failed)?;
    }

    Ok((manager, kind))
}

fn preferred_service_level(kind: ServiceManagerKind) -> ServiceLevel {
    match kind {
        ServiceManagerKind::Systemd | ServiceManagerKind::Launchd => {
            if is_root() {
                ServiceLevel::System
            } else {
                ServiceLevel::User
            }
        }
        _ => ServiceLevel::System,
    }
}

fn is_root() -> bool {
    #[cfg(unix)]
    {
        rustix::process::geteuid().is_root()
    }

    #[cfg(not(unix))]
    {
        false
    }
}

/// Ensures the environment variables needed by `systemctl --user` are present.
/// On SSH sessions and minimal installs, `XDG_RUNTIME_DIR` and
/// `DBUS_SESSION_BUS_ADDRESS` may be missing, causing
/// "Failed to connect to bus: No medium found".
// This is the one function in the crate that keeps `unsafe`: `env::set_var` has
// no safe equivalent in edition 2024, and `service-manager` shells out to
// `systemctl --user` with the inherited environment, giving no hook to pass
// these values to the child explicitly. The mutation is sound here because it
// runs during synchronous, single-threaded CLI startup, before any tokio
// runtime or other thread exists (see the SAFETY notes on each call). The rest
// of the crate is `#![deny(unsafe_code)]`; this function carries the only
// allowance, scoped and documented.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
fn ensure_systemd_user_env() -> Result<()> {
    use std::env;

    let uid = rustix::process::getuid().as_raw();
    let runtime_dir = format!("/run/user/{uid}");

    if env::var("XDG_RUNTIME_DIR").is_err() {
        if !Path::new(&runtime_dir).exists() {
            return Err(Error::ExecutionFailed(format!(
                "User runtime directory {runtime_dir} does not exist.\n\
                 The systemd user session is not active.\n\
                 Try: loginctl enable-linger $USER\n\
                 Or ensure 'dbus-user-session' is installed: sudo apt install dbus-user-session"
            )));
        }
        // SAFETY: called during single-threaded CLI startup, before tokio runtime.
        unsafe {
            env::set_var("XDG_RUNTIME_DIR", &runtime_dir);
        }
    }

    let xdg = env::var("XDG_RUNTIME_DIR").unwrap_or(runtime_dir);
    let bus_path = format!("{xdg}/bus");
    if env::var("DBUS_SESSION_BUS_ADDRESS").is_err() {
        if !Path::new(&bus_path).exists() {
            return Err(Error::ExecutionFailed(format!(
                "D-Bus session bus socket not found at {bus_path}.\n\
                 Try: sudo apt install dbus-user-session && loginctl enable-linger $USER\n\
                 Then log out and back in, or reboot."
            )));
        }
        // SAFETY: called during single-threaded CLI startup, before tokio runtime.
        unsafe {
            env::set_var("DBUS_SESSION_BUS_ADDRESS", format!("unix:path={bus_path}"));
        }
    }

    Ok(())
}

fn systemd_user_unit_dir() -> Result<PathBuf> {
    dirs::config_dir()
        .map(|p| p.join("systemd").join("user"))
        .ok_or_else(|| Error::ExecutionFailed("Unable to locate config directory".to_string()))
}

fn launchd_user_agent_dir() -> Result<PathBuf> {
    dirs::home_dir()
        .map(|p| p.join("Library").join("LaunchAgents"))
        .ok_or_else(|| Error::ExecutionFailed("Unable to locate home directory".to_string()))
}

fn render_systemd_service(ctx: &ServiceInstallCtx) -> String {
    let mut lines = vec![
        "[Unit]".to_string(),
        "Description=Peppy daemon service".to_string(),
        String::new(),
        "[Service]".to_string(),
    ];

    if let Some(dir) = &ctx.working_directory {
        lines.push(format!("WorkingDirectory={}", dir.display()));
    }

    if let Some(envs) = &ctx.environment {
        for (key, value) in envs {
            lines.push(format!("Environment=\"{key}={value}\""));
        }
    }

    lines.push(format!("ExecStart={}", build_exec_command(ctx)));

    match &ctx.restart_policy {
        RestartPolicy::Never => {
            lines.push("Restart=no".to_string());
        }
        RestartPolicy::Always { delay_secs } => {
            lines.push("Restart=always".to_string());
            if let Some(secs) = delay_secs {
                lines.push(format!("RestartSec={secs}"));
            }
        }
        RestartPolicy::OnFailure { delay_secs, .. } => {
            lines.push("Restart=on-failure".to_string());
            if let Some(secs) = delay_secs {
                lines.push(format!("RestartSec={secs}"));
            }
        }
        RestartPolicy::OnSuccess { delay_secs } => {
            lines.push("Restart=on-success".to_string());
            if let Some(secs) = delay_secs {
                lines.push(format!("RestartSec={secs}"));
            }
        }
    }

    lines.push(String::new());
    lines.push("[Install]".to_string());
    if ctx.autostart {
        lines.push("WantedBy=multi-user.target".to_string());
    } else {
        lines.push("WantedBy=default.target".to_string());
    }

    lines.join("\n")
}

fn render_launchd_plist(ctx: &ServiceInstallCtx) -> String {
    let mut lines = vec![
        r#"<?xml version="1.0" encoding="UTF-8"?>"#.to_string(),
        r#"<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">"#.to_string(),
        "<plist version=\"1.0\">".to_string(),
        "  <dict>".to_string(),
        "    <key>Label</key>".to_string(),
        format!(
            "    <string>{}</string>",
            ctx.label.to_qualified_name()
        ),
        "    <key>ProgramArguments</key>".to_string(),
        "    <array>".to_string(),
    ];

    // Wrap in a login shell to source the user's profile and get full environment
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let inner_cmd = command_parts(ctx).join(" ");
    lines.push(format!("      <string>{}</string>", shell));
    lines.push("      <string>-l</string>".to_string());
    lines.push("      <string>-c</string>".to_string());
    lines.push(format!("      <string>{}</string>", inner_cmd));

    lines.push("    </array>".to_string());

    if let Some(envs) = &ctx.environment {
        lines.push("    <key>EnvironmentVariables</key>".to_string());
        lines.push("    <dict>".to_string());
        for (key, value) in envs {
            lines.push(format!("      <key>{key}</key>"));
            lines.push(format!("      <string>{value}</string>"));
        }
        lines.push("    </dict>".to_string());
    }

    if let Some(dir) = &ctx.working_directory {
        lines.push("    <key>WorkingDirectory</key>".to_string());
        lines.push(format!("    <string>{}</string>", dir.display()));
    }

    lines.push("    <key>RunAtLoad</key>".to_string());
    if ctx.autostart {
        lines.push("    <true/>".to_string());
    } else {
        lines.push("    <false/>".to_string());
    }

    if !matches!(ctx.restart_policy, RestartPolicy::Never) {
        lines.push("    <key>KeepAlive</key>".to_string());
        lines.push("    <true/>".to_string());
    }

    lines.push("  </dict>".to_string());
    lines.push("</plist>".to_string());

    lines.join("\n")
}

fn build_exec_command(ctx: &ServiceInstallCtx) -> String {
    let inner_cmd = command_parts(ctx).join(" ");
    // Wrap in a login shell to source the user's profile and get full environment
    // (PATH, custom env vars, etc.) even when started by systemd
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    format!("{} -l -c '{}'", shell, inner_cmd)
}

fn command_parts(ctx: &ServiceInstallCtx) -> Vec<String> {
    let mut parts = Vec::with_capacity(ctx.args.len() + 1);
    parts.push(ctx.program.to_string_lossy().into_owned());
    parts.extend(
        ctx.args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned()),
    );
    parts
}

fn systemd_unit_dir() -> PathBuf {
    PathBuf::from("/etc/systemd/system")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peppy_home_value(env: &[(String, String)]) -> Option<&str> {
        env.iter()
            .find(|(key, _)| key == config::consts::PEPPY_HOME_ENV)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn service_environment_propagates_peppy_home_override() {
        let env = service_environment(
            Some("/usr/bin:/bin".to_string()),
            Some(OsString::from("/var/tmp/run-home")),
        );
        assert_eq!(
            peppy_home_value(&env),
            Some("/var/tmp/run-home"),
            "service env must carry the PEPPY_HOME override so the daemon shares \
             the CLI's data root: {env:?}"
        );
    }

    #[test]
    fn service_environment_omits_unset_peppy_home() {
        // No override: daemon and CLI both fall back to the default root.
        let env = service_environment(Some("/usr/bin".to_string()), None);
        assert_eq!(peppy_home_value(&env), None, "{env:?}");
    }

    #[test]
    fn service_environment_treats_empty_peppy_home_as_unset() {
        // Matches `peppy_root_dir`'s empty-string guard: `PEPPY_HOME=` must not
        // root the daemon at the empty path.
        let env = service_environment(None, Some(OsString::new()));
        assert_eq!(peppy_home_value(&env), None, "{env:?}");
    }
}
