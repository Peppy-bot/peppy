use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::io::IsTerminal;

use clap::Subcommand;

use super::Command;
use crate::context::AppContext;
#[cfg(target_os = "linux")]
use crate::error::Error;
use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum ContainerCommands {
    /// Check container prerequisites and show what needs fixing
    Status,
    /// Interactively fix container prerequisites (may prompt for sudo)
    Setup,
}

#[derive(Debug)]
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
    println!(
        "  User namespace         : {}",
        user_namespace_state(&status.user_namespace)
    );
    println!();

    if status.is_ok() {
        println!("All checks passed. Container support is ready.");
        return Ok(());
    }
    Err(Error::ExecutionFailed(advice_for_failing_status(&status)))
}

/// What the user does about a status that fails: run the setup when it has a
/// fix to make, and read the cause of a failure that has none.
#[cfg(target_os = "linux")]
fn advice_for_failing_status(status: &containers::SetupStatus) -> String {
    let run_the_setup = status
        .fix_script
        .as_ref()
        .map(|_| "Some checks failed. Run `peppy container setup` to fix.".to_string());
    let cause_with_no_fix = status.user_namespace.refusal_without_repair();
    [run_the_setup, cause_with_no_fix]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n")
}

/// The state that the `User namespace` line of `peppy container status`
/// shows: what the kernel answered to the request of a user namespace that
/// Apptainer makes before a build.
#[cfg(target_os = "linux")]
fn user_namespace_state(user_namespace: &containers::UnprivilegedUserNamespace) -> String {
    use containers::UnprivilegedUserNamespace::{
        Granted, NotRequired, Refused, RefusedWithoutProfile,
    };

    match user_namespace {
        NotRequired => "not required (root builds with no user namespace)".to_string(),
        Granted => "OK".to_string(),
        RefusedWithoutProfile { answer } | Refused { answer } => format!("FAILED ({answer})"),
    }
}

/// The error of a setup whose checks fail with no fix left to make. A user
/// namespace that the kernel refuses for a cause with no fix explains itself.
#[cfg(target_os = "linux")]
fn failure_with_no_fix_left(status: &containers::SetupStatus) -> Error {
    Error::ExecutionFailed(
        status
            .user_namespace
            .refusal_without_repair()
            .unwrap_or_else(|| {
                "Some checks still failing after running fixes. Check permissions manually."
                    .to_string()
            }),
    )
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Linux form of this function can fail, and both forms have one caller"
)]
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

    // The one failing check with no fix is a user namespace that the kernel
    // refuses for a cause other than the AppArmor profile.
    let Some(script) = status.fix_script.as_deref() else {
        return Err(failure_with_no_fix_left(&status));
    };

    println!("The following fixes are needed:");
    if !status.newuidmap_ok {
        println!("  - Install uidmap package (provides newuidmap for fakeroot)");
    }
    if status.apparmor_restricted && !status.apparmor_ok {
        println!("  - Install/update AppArmor profile for Apptainer starter");
    } else if status.apparmor_restricted && !status.apparmor_loaded {
        println!("  - Load AppArmor profile for Apptainer starter into the kernel");
    }
    if status.user_namespace.needs_profile() {
        println!(
            "  - Load AppArmor profile `unprivileged_userns` into the kernel \
             (gives Apptainer the user namespace of a build)"
        );
    }
    println!();

    // Check if we're in an interactive terminal
    if std::io::stdin().is_terminal() {
        print!("Apply these fixes now? (requires sudo) [Y/n] ");
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let mut reply = String::new();
        let _ = std::io::stdin().read_line(&mut reply);
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

    if !recheck.is_ok() {
        return Err(failure_with_no_fix_left(&recheck));
    }
    println!();
    println!("Container setup completed successfully.");
    Ok(())
}

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Linux form of this function can fail, and both forms have one caller"
)]
fn setup() -> Result<()> {
    println!("No setup needed. On macOS, containers run inside a Lima VM (no setup required).");
    Ok(())
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use containers::{SetupStatus, UnprivilegedUserNamespace};

    const REFUSAL: &str = "unshare: unshare failed: Permission denied";

    /// A status whose every check passes but the user namespace, which is
    /// `user_namespace`.
    fn status_with(user_namespace: UnprivilegedUserNamespace) -> SetupStatus {
        SetupStatus {
            newuidmap_ok: true,
            apparmor_restricted: true,
            apparmor_ok: true,
            apparmor_loaded: true,
            apparmor_manageable: true,
            user_namespace,
            fix_script: None,
        }
    }

    fn message_of(error: Error) -> String {
        match error {
            Error::ExecutionFailed(message) => message,
            other => panic!("expected an execution failure, got {other:?}"),
        }
    }

    #[test]
    fn the_user_namespace_line_shows_the_answer_of_the_kernel() {
        let refused = || REFUSAL.to_string();
        let cases = [
            (UnprivilegedUserNamespace::Granted, "OK".to_string()),
            (
                UnprivilegedUserNamespace::NotRequired,
                "not required (root builds with no user namespace)".to_string(),
            ),
            (
                UnprivilegedUserNamespace::RefusedWithoutProfile { answer: refused() },
                format!("FAILED ({REFUSAL})"),
            ),
            (
                UnprivilegedUserNamespace::Refused { answer: refused() },
                format!("FAILED ({REFUSAL})"),
            ),
        ];
        for (user_namespace, state) in cases {
            assert_eq!(
                user_namespace_state(&user_namespace),
                state,
                "{user_namespace:?}"
            );
        }
    }

    #[test]
    fn a_failing_status_tells_to_run_the_setup_that_has_a_fix() {
        let status = SetupStatus {
            fix_script: Some("sudo apparmor_parser -r /etc/apparmor.d/unprivileged_userns".into()),
            ..status_with(UnprivilegedUserNamespace::RefusedWithoutProfile {
                answer: REFUSAL.to_string(),
            })
        };

        assert_eq!(
            advice_for_failing_status(&status),
            "Some checks failed. Run `peppy container setup` to fix."
        );
    }

    #[test]
    fn a_failing_status_explains_the_refusal_that_has_no_fix() {
        let refused = UnprivilegedUserNamespace::Refused {
            answer: REFUSAL.to_string(),
        };
        let explanation = refused
            .refusal_without_repair()
            .expect("a refusal with no repair explains itself");

        assert_eq!(
            advice_for_failing_status(&status_with(refused.clone())),
            explanation
        );

        // A second check that fails, with a fix: the advice has the two.
        let with_a_fix_also = SetupStatus {
            newuidmap_ok: false,
            fix_script: Some("sudo apt-get install -y uidmap".into()),
            ..status_with(refused)
        };
        assert_eq!(
            advice_for_failing_status(&with_a_fix_also),
            format!("Some checks failed. Run `peppy container setup` to fix.\n{explanation}")
        );
    }

    #[test]
    fn a_refusal_with_no_fix_is_the_error_of_the_setup() {
        let status = status_with(UnprivilegedUserNamespace::Refused {
            answer: REFUSAL.to_string(),
        });

        let message = message_of(failure_with_no_fix_left(&status));

        assert_eq!(
            Some(message),
            status.user_namespace.refusal_without_repair()
        );
    }

    #[test]
    fn a_check_that_fails_after_its_fix_is_the_error_of_the_setup() {
        let status = status_with(UnprivilegedUserNamespace::RefusedWithoutProfile {
            answer: REFUSAL.to_string(),
        });

        assert_eq!(
            message_of(failure_with_no_fix_left(&status)),
            "Some checks still failing after running fixes. Check permissions manually."
        );
    }
}
