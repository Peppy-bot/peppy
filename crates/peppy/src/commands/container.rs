use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::io::IsTerminal;

use clap::Subcommand;

use super::Command;
use crate::context::AppContext;
#[cfg(target_os = "linux")]
use crate::error::Error;
use crate::error::Result;

#[derive(Subcommand)]
pub enum ContainerCommands {
    /// Check container prerequisites and show what needs fixing
    Status,
    /// Interactively fix container prerequisites (may prompt for sudo)
    Setup,
}

pub struct ContainerCommand {
    pub command: ContainerCommands,
}

impl Command for ContainerCommand {
    fn execute(self, _ctx: &Arc<AppContext>) -> Result<()> {
        match self.command {
            ContainerCommands::Status => status(),
            ContainerCommands::Setup => setup(),
        }
    }
}

#[cfg(target_os = "linux")]
fn status() -> Result<()> {
    let apptainer_dir = containers::Apptainer::resolve_apptainer_dir().map_err(|e| {
        Error::ExecutionFailed(format!("Could not locate Apptainer installation: {e}"))
    })?;

    let status = containers::check_setup_status(&apptainer_dir);

    println!("Container prerequisites");
    println!("----------------------");
    if !status.apparmor_manageable {
        println!("  AppArmor               : not manageable (security filesystem not mounted)");
    }
    println!(
        "  newuidmap              : {}",
        if status.newuidmap_ok {
            "OK"
        } else {
            "FAILED (install uidmap package)"
        }
    );
    if !status.apparmor_manageable {
        println!("  AppArmor profile       : skipped (AppArmor not manageable)");
    } else if status.apparmor_restricted {
        println!(
            "  AppArmor profile       : {}",
            if status.apparmor_ok { "OK" } else { "FAILED" }
        );
        println!(
            "  AppArmor profile loaded: {}",
            if status.apparmor_loaded {
                "OK"
            } else {
                "FAILED"
            }
        );
    } else {
        println!("  AppArmor profile       : not required");
    }
    println!();

    if status.is_ok() {
        println!("All checks passed. Container support is ready.");
        Ok(())
    } else {
        println!("Some checks failed. Run `peppy container setup` to fix.");
        std::process::exit(1);
    }
}

#[cfg(not(target_os = "linux"))]
fn status() -> Result<()> {
    println!("Container prerequisites");
    println!("----------------------");
    println!("  Apptainer user namespace checks are only required on Linux.");
    println!("  On macOS, containers run inside a Lima VM (no setup needed).");
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup() -> Result<()> {
    let apptainer_dir = containers::Apptainer::resolve_apptainer_dir().map_err(|e| {
        Error::ExecutionFailed(format!("Could not locate Apptainer installation: {e}"))
    })?;

    let status = containers::check_setup_status(&apptainer_dir);

    if status.is_ok() {
        if !status.apparmor_manageable {
            println!(
                "AppArmor is not manageable on this system (security filesystem not mounted)."
            );
            println!("Skipping AppArmor profile checks.");
        }
        println!("All container prerequisites are already met. Nothing to do.");
        return Ok(());
    }

    println!("The following fixes are needed:");
    if !status.newuidmap_ok {
        println!("  - Install uidmap package (provides newuidmap for fakeroot)");
    }
    if status.apparmor_restricted && !status.apparmor_ok {
        println!("  - Install/update AppArmor profile for Apptainer starter");
    } else if status.apparmor_restricted && !status.apparmor_loaded {
        println!("  - Load AppArmor profile for Apptainer starter into the kernel");
    }
    println!();

    let script = status
        .fix_script
        .as_deref()
        .expect("fix_script is Some when is_ok() is false");

    // Check if we're in an interactive terminal
    if std::io::stdin().is_terminal() {
        print!("Apply these fixes now? (requires sudo) [Y/n] ");
        use std::io::Write;
        std::io::stdout().flush().ok();

        let mut reply = String::new();
        std::io::stdin().read_line(&mut reply).ok();
        let reply = reply.trim();
        if reply.eq_ignore_ascii_case("n") || reply.eq_ignore_ascii_case("no") {
            println!();
            println!("Skipped. You can apply the fixes manually:");
            println!();
            println!("{script}");
            return Ok(());
        }
    }

    let exit_status = std::process::Command::new("sh")
        .arg("-c")
        .arg(script)
        .status()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to run fix script: {e}")))?;

    if !exit_status.success() {
        return Err(Error::ExecutionFailed(
            "Fix script failed. Check the output above for details.".to_string(),
        ));
    }

    // Re-check to confirm
    let recheck = containers::check_setup_status(&apptainer_dir);

    if recheck.is_ok() {
        println!();
        println!("Container setup completed successfully.");
        Ok(())
    } else {
        Err(Error::ExecutionFailed(
            "Some checks still failing after running fixes. Check permissions manually."
                .to_string(),
        ))
    }
}

#[cfg(not(target_os = "linux"))]
fn setup() -> Result<()> {
    println!("No setup needed. On macOS, containers run inside a Lima VM (no setup required).");
    Ok(())
}
