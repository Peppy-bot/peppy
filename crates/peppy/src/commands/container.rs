use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::io::IsTerminal;

use clap::Subcommand;

#[cfg(target_os = "linux")]
use containers::SetupVerdict;

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

/// The installation directory of Apptainer that the checks are for.
#[cfg(target_os = "linux")]
fn apptainer_dir() -> Result<std::path::PathBuf> {
    containers::Apptainer::resolve_apptainer_dir().map_err(|e| {
        Error::ExecutionFailed(format!("Could not locate Apptainer installation: {e}"))
    })
}

#[cfg(target_os = "linux")]
fn status() -> Result<()> {
    let status = containers::check_setup_status(&apptainer_dir()?);

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

    let Some(advice) = advice_for_failing_checks(&status.verdict) else {
        println!("All checks passed. Container support is ready.");
        return Ok(());
    };
    Err(Error::ExecutionFailed(advice))
}

/// What the user does about checks that fail, or `None` when each check
/// passes: run the setup when it has a fix to make, and read the cause of a
/// failure that has none.
#[cfg(target_os = "linux")]
fn advice_for_failing_checks(verdict: &SetupVerdict) -> Option<String> {
    const RUN_THE_SETUP: &str = "Some checks failed. Run `peppy container setup` to fix.";

    match verdict {
        SetupVerdict::Ready => None,
        SetupVerdict::Fixable { .. } => Some(RUN_THE_SETUP.to_string()),
        SetupVerdict::Unfixable {
            refusal,
            fix_script: Some(_),
        } => Some(format!("{RUN_THE_SETUP}\n{refusal}")),
        SetupVerdict::Unfixable {
            refusal,
            fix_script: None,
        } => Some(refusal.clone()),
    }
}

/// The state that the `User namespace` line of `peppy container status`
/// shows: what the kernel answered to the request of a user namespace for a
/// container build.
#[cfg(target_os = "linux")]
fn user_namespace_state(user_namespace: &containers::UnprivilegedUserNamespace) -> String {
    use containers::UnprivilegedUserNamespace::{Granted, NotRequired, Refused, Untested};

    match user_namespace {
        NotRequired => "not required (root builds with no user namespace)".to_string(),
        Granted => "OK".to_string(),
        Refused { answer } => format!("FAILED ({answer})"),
        Untested { reason } => format!("not tested ({reason})"),
    }
}

/// The warning of a setup on a machine whose kernel refuses the user
/// namespace of a container build: `refusal` explains the cause, for which
/// the setup has no fix.
#[cfg(target_os = "linux")]
fn refusal_warning(refusal: &str) -> String {
    format!(
        "Warning: {refusal}\n\
         Each container build fails on this machine. The daemon runs the nodes that are not \
         containers."
    )
}

/// How the setup ends after its fixes, for the verdict of the checks made
/// again: with success, with success and a warning to show when the one
/// failure left is a refusal that has no fix, or with an error when a check
/// that has a fix fails again.
#[cfg(target_os = "linux")]
fn end_after_fixes(verdict: &SetupVerdict) -> Result<Option<String>> {
    match verdict {
        SetupVerdict::Ready => Ok(None),
        SetupVerdict::Unfixable {
            refusal,
            fix_script: None,
        } => Ok(Some(refusal_warning(refusal))),
        SetupVerdict::Fixable { .. }
        | SetupVerdict::Unfixable {
            fix_script: Some(_),
            ..
        } => Err(Error::ExecutionFailed(
            "Some checks still failing after running fixes. Check permissions manually."
                .to_string(),
        )),
    }
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
    let apptainer_dir = apptainer_dir()?;
    let status = containers::check_setup_status(&apptainer_dir);

    let fix_script = match &status.verdict {
        SetupVerdict::Ready => {
            if !status.apparmor_manageable {
                println!(
                    "AppArmor is not manageable on this system (security filesystem not mounted)."
                );
                println!("Skipping AppArmor profile checks.");
            }
            println!("All container prerequisites are already met. Nothing to do.");
            return Ok(());
        }
        // The setup has no fix to make, so it changes nothing and ends with
        // success: a second run has no other result.
        SetupVerdict::Unfixable {
            refusal,
            fix_script: None,
        } => {
            eprintln!("{}", refusal_warning(refusal));
            return Ok(());
        }
        // The user reads, before a change of the machine, that the fixes
        // leave the container builds failing.
        SetupVerdict::Unfixable {
            refusal,
            fix_script: Some(fix_script),
        } => {
            eprintln!("{}", refusal_warning(refusal));
            eprintln!("The fixes below do not repair this cause. The daemon needs them to start.");
            eprintln!();
            fix_script
        }
        SetupVerdict::Fixable { fix_script } => fix_script,
    };

    println!("The following fixes are needed:");
    if !status.newuidmap_ok {
        println!("  - Install uidmap package (provides newuidmap for fakeroot)");
    }
    if status.apparmor_restricted && !status.apparmor_ok {
        println!("  - Install/update AppArmor profile for Apptainer");
    } else if status.apparmor_restricted && !status.apparmor_loaded {
        println!("  - Load AppArmor profile for Apptainer into the kernel");
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
            println!("{fix_script}");
            return Ok(());
        }
    }

    let exit_status = std::process::Command::new("sh")
        .arg("-c")
        .arg(fix_script)
        .status()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to run fix script: {e}")))?;

    if !exit_status.success() {
        return Err(Error::ExecutionFailed(
            "Fix script failed. Check the output above for details.".to_string(),
        ));
    }

    // Re-check to confirm
    let recheck = containers::check_setup_status(&apptainer_dir);

    let Some(warning) = end_after_fixes(&recheck.verdict)? else {
        println!();
        println!("Container setup completed successfully.");
        return Ok(());
    };
    eprintln!();
    eprintln!("{warning}");
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
    use containers::UnprivilegedUserNamespace;

    const ANSWER: &str = "unshare: unshare failed: Operation not permitted";
    const REFUSAL: &str = "The kernel refuses a user namespace.";
    const FIX_SCRIPT: &str = "sudo apt-get install -y uidmap";

    fn fixable() -> SetupVerdict {
        SetupVerdict::Fixable {
            fix_script: FIX_SCRIPT.to_string(),
        }
    }

    /// The verdict of a refusal that has no fix, with or without another
    /// check that fails and has one.
    fn unfixable(fix_script: Option<&str>) -> SetupVerdict {
        SetupVerdict::Unfixable {
            refusal: REFUSAL.to_string(),
            fix_script: fix_script.map(str::to_owned),
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
        let cases = [
            (UnprivilegedUserNamespace::Granted, "OK".to_string()),
            (
                UnprivilegedUserNamespace::NotRequired,
                "not required (root builds with no user namespace)".to_string(),
            ),
            (
                UnprivilegedUserNamespace::Refused {
                    answer: ANSWER.to_string(),
                },
                format!("FAILED ({ANSWER})"),
            ),
            (
                UnprivilegedUserNamespace::Untested {
                    reason: "cannot run `unshare`: no such program".to_string(),
                },
                "not tested (cannot run `unshare`: no such program)".to_string(),
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
    fn a_status_that_passes_has_no_advice() {
        assert_eq!(advice_for_failing_checks(&SetupVerdict::Ready), None);
    }

    #[test]
    fn a_failing_status_tells_to_run_the_setup_that_has_a_fix() {
        assert_eq!(
            advice_for_failing_checks(&fixable()).as_deref(),
            Some("Some checks failed. Run `peppy container setup` to fix.")
        );
    }

    #[test]
    fn a_failing_status_explains_the_refusal_that_has_no_fix() {
        assert_eq!(
            advice_for_failing_checks(&unfixable(None)).as_deref(),
            Some(REFUSAL)
        );

        // A second check that fails, with a fix: the advice has the two.
        assert_eq!(
            advice_for_failing_checks(&unfixable(Some(FIX_SCRIPT))),
            Some(format!(
                "Some checks failed. Run `peppy container setup` to fix.\n{REFUSAL}"
            ))
        );
    }

    #[test]
    fn the_warning_of_a_refusal_tells_what_runs_and_what_fails() {
        assert_eq!(
            refusal_warning(REFUSAL),
            "Warning: The kernel refuses a user namespace.\n\
             Each container build fails on this machine. The daemon runs the nodes that are \
             not containers."
        );
    }

    #[test]
    fn a_setup_whose_fixes_leave_each_check_passing_ends_with_success() {
        assert_eq!(end_after_fixes(&SetupVerdict::Ready).unwrap(), None);
    }

    /// A refusal has no fix, so a setup that made each of its fixes ends
    /// with success and the warning.
    #[test]
    fn a_setup_whose_fixes_leave_a_refusal_ends_with_success_and_a_warning() {
        assert_eq!(
            end_after_fixes(&unfixable(None)).unwrap(),
            Some(refusal_warning(REFUSAL))
        );
    }

    #[test]
    fn a_check_that_fails_after_its_fix_is_the_error_of_the_setup() {
        for verdict in [fixable(), unfixable(Some(FIX_SCRIPT))] {
            assert_eq!(
                message_of(end_after_fixes(&verdict).unwrap_err()),
                "Some checks still failing after running fixes. Check permissions manually.",
                "{verdict:?}"
            );
        }
    }
}
