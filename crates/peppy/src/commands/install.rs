use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use service_manager::{
    RestartPolicy, ServiceInstallCtx, ServiceLabel, ServiceManager, ServiceManagerKind,
    TypedServiceManager,
};

use super::Command;
use crate::{AppContext, Error, Result};

const PEPPY_SERVICE_LABEL: &str = "bot.peppy.daemon";

pub struct InstallCommand {}

impl Command for InstallCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        install_peppy_daemon(None).map(|_| ())
    }
}

pub fn install_peppy_daemon(service_dir_override: Option<PathBuf>) -> Result<PathBuf> {
    let label: ServiceLabel = PEPPY_SERVICE_LABEL.parse()?;
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
    let kind = ServiceManagerKind::native()?;

    if let Some(dir) = service_dir_override {
        return write_service_definition(kind, &dir, &ctx);
    }

    let manager = TypedServiceManager::target(kind);
    manager
        .install(ctx)
        .map_err(|e| Error::ExecutionFailed(e.to_string()))?;

    default_service_path(kind, &label)
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
        environment: Some(vec![("PEPPY_ENV".to_string(), "PROD".to_string())]),
        autostart,
        restart_policy: RestartPolicy::OnFailure {
            delay_secs: Some(5),
        },
    })
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
    let service_name = format!("{}.service", ctx.label.to_script_name());
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

fn default_service_path(kind: ServiceManagerKind, label: &ServiceLabel) -> Result<PathBuf> {
    match kind {
        ServiceManagerKind::Systemd => {
            let service_name = format!("{}.service", label.to_script_name());
            Ok(systemd_unit_dir().join(service_name))
        }
        ServiceManagerKind::Launchd => {
            let plist_name = format!("{}.plist", label.to_qualified_name());
            Ok(PathBuf::from("/Library/LaunchDaemons").join(plist_name))
        }
        other => Err(Error::ExecutionFailed(format!(
            "Unsupported service manager: {other:?}"
        ))),
    }
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
        RestartPolicy::OnFailure { delay_secs } => {
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

    for arg in command_parts(ctx) {
        lines.push(format!("      <string>{}</string>", arg));
    }

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
    command_parts(ctx).join(" ")
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
