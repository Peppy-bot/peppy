use std::sync::Arc;

#[cfg(target_os = "linux")]
use std::io::IsTerminal;

use clap::Subcommand;

#[cfg(target_os = "linux")]
use containers::{
    FailureSeverity, ImageFiles, ProgramCheck, ProgramState, SetupVerdict, SystemPrograms,
};

use super::Command;
use crate::context::AppContext;
#[cfg(target_os = "linux")]
use crate::error::Error;
use crate::error::Result;

#[derive(Debug, Subcommand)]
pub enum ContainerCommands {
    /// Check container prerequisites and show what needs fixing
    Status {
        /// Check only the packages of the Linux distribution that container nodes need
        #[arg(long)]
        packages: bool,
    },
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
            ContainerCommands::Status { packages: false } => status(),
            ContainerCommands::Status { packages: true } => packages_status(),
            ContainerCommands::Setup => setup(),
        }
    }
}

/// The section of the Containers guide that explains the unpacked images.
#[cfg(target_os = "linux")]
const UNPACKED_IMAGES_GUIDE: &str =
    "https://docs.peppy.bot/advanced_guides/containers/#node-images-on-overlayfs";

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
    print_program_lines(&status.programs);
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
    println!(
        "  Node images            : {}",
        image_files_state(&status.image_files)
    );
    println!();

    if let Some(note) = note_on_unpacked_images(&status.image_files) {
        println!("{note}");
        println!();
    }
    let Some(advice) = advice_for_failing_checks(&status.verdict) else {
        println!("{}", summary_of_passing_checks(&status.verdict));
        return Ok(());
    };
    Err(Error::ExecutionFailed(advice))
}

/// The status of the packages alone: the programs of the Linux distribution
/// that Apptainer runs. It fails when each container build stops without one
/// of them, and gives the command that installs each missing package.
#[cfg(target_os = "linux")]
fn packages_status() -> Result<()> {
    let programs = containers::check_system_programs();

    println!("Container packages");
    println!("------------------");
    print_program_lines(&programs);
    println!();

    match advice_for_missing_packages(&programs) {
        None => {
            println!("Each package that container nodes need is installed.");
            Ok(())
        }
        Some((severity, advice)) if severity.is_a_failure() => Err(Error::ExecutionFailed(advice)),
        Some((_, advice)) => {
            eprintln!("Warning: {advice}");
            Ok(())
        }
    }
}

/// Writes one line for the check of each program of the distribution.
#[cfg(target_os = "linux")]
fn print_program_lines(programs: &SystemPrograms) {
    for check in programs.checks {
        println!("  {:<23}: {}", check.program.name(), program_state(check));
    }
}

/// The state that the line of a program shows: what its check found, and for
/// a missing program the package to install.
#[cfg(target_os = "linux")]
fn program_state(check: ProgramCheck) -> String {
    let package = check.program.package();
    match check.state {
        ProgramState::Found => "OK".to_string(),
        ProgramState::NotRequired => "not required (the builds of root do not run it)".to_string(),
        ProgramState::Missing(severity) if severity.is_a_failure() => {
            format!("FAILED (install the {package} package)")
        }
        ProgramState::Missing(_) => format!(
            "WARNING (install the {package} package for {})",
            check.program.purpose()
        ),
    }
}

/// What the user does about packages that are missing, with what the most
/// severe absence does to the daemon, or `None` when no package is missing:
/// the packages to install and the command that installs them.
#[cfg(target_os = "linux")]
fn advice_for_missing_packages(programs: &SystemPrograms) -> Option<(FailureSeverity, String)> {
    let severity = programs.severity()?;
    let install_command = programs.install_command()?;
    let packages = programs
        .missing()
        .filter(|(_, of_program)| of_program.is_a_failure() == severity.is_a_failure())
        .map(|(program, _)| program.package())
        .collect::<Vec<_>>()
        .join(", ");
    let cause = if severity.is_a_failure() {
        format!(
            "Each container build stops without packages that this machine does not have: \
             {packages}."
        )
    } else {
        format!(
            "This machine does not have packages that container nodes build and run without: \
             {packages}."
        )
    };
    Some((
        severity,
        format!(
            "{cause}\nTo install each missing package, run the following command:\n\n\
             {install_command}"
        ),
    ))
}

/// What the user does about checks that fail, or `None` when no check that
/// the daemon or a container build needs fails: run the setup when it has a
/// fix for a failure, and read the cause of a failure that has none.
#[cfg(target_os = "linux")]
fn advice_for_failing_checks(verdict: &SetupVerdict) -> Option<String> {
    const RUN_THE_SETUP: &str = "Some checks failed. Run `peppy container setup` to fix.";

    match verdict {
        SetupVerdict::Ready => None,
        SetupVerdict::Fixable { .. } => verdict
            .fix_repairs_a_failure()
            .then(|| RUN_THE_SETUP.to_string()),
        SetupVerdict::Unfixable { refusal, .. } if verdict.fix_repairs_a_failure() => {
            Some(format!("{RUN_THE_SETUP}\n{refusal}"))
        }
        SetupVerdict::Unfixable { refusal, .. } => Some(refusal.clone()),
    }
}

/// The last line of a status with no advice to give: each check passes, or
/// the one failure is a warning that the setup has a fix for.
#[cfg(target_os = "linux")]
const fn summary_of_passing_checks(verdict: &SetupVerdict) -> &'static str {
    match verdict.fix() {
        None => "All checks passed. Container support is ready.",
        Some(_) => "Container support is ready. Run `peppy container setup` to fix the warnings.",
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

/// The state that the `Node images` line of `peppy container status` shows:
/// how each node gets the files of its image.
#[cfg(target_os = "linux")]
fn image_files_state(image_files: &ImageFiles) -> String {
    match image_files.cause_of_unpacking() {
        None => "mounted".to_string(),
        Some(cause) => format!("unpacked at each start ({cause})"),
    }
}

/// What the status tells below its lines when peppy unpacks the image of
/// each node: what this costs, and how to get mounted images. It is not a
/// failure of the setup. `None` for images that peppy mounts.
#[cfg(target_os = "linux")]
fn note_on_unpacked_images(image_files: &ImageFiles) -> Option<String> {
    image_files.cause_of_unpacking()?;
    Some(format!(
        "Note: each node starts from an unpacked copy of its image, because its user is not \
         root and it can write to a directory on overlayfs. A start takes more time, and the \
         copy uses disk space while the node runs.\n\
         For mounted images, put a volume or a tmpfs at each directory that the `Node images` \
         line names, or run the daemon as root. See {UNPACKED_IMAGES_GUIDE}"
    ))
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

/// The warning of a setup that leaves packages not installed whose absence
/// is a warning: the fix script did not install them, and the daemon and
/// its builds do not need them. `None` when `programs`, the checks made
/// again after the script, find each such package.
#[cfg(target_os = "linux")]
fn warning_for_packages_left(programs: &SystemPrograms) -> Option<String> {
    let left: Vec<String> = programs
        .missing()
        .filter(|(_, severity)| !severity.is_a_failure())
        .map(|(program, _)| format!("{} (for {})", program.package(), program.purpose()))
        .collect();
    if left.is_empty() {
        return None;
    }
    Some(format!(
        "Warning: these packages are not installed: {}. The nodes that peppy builds build and \
         run without them.",
        left.join(", ")
    ))
}

/// How the setup ends after its fix script, which `script_succeeded` or not,
/// for the checks made again (`verdict`, and `programs` for the packages).
///
/// The checks made again decide, and not the exit status of the script: the
/// setup ends with an error only when a check that is a failure and has a fix
/// fails again. It ends with success and the warnings to show when what is
/// left is a refusal that has no fix, or a package whose absence is a
/// warning.
#[cfg(target_os = "linux")]
fn end_after_fixes(
    verdict: &SetupVerdict,
    programs: &SystemPrograms,
    script_succeeded: bool,
) -> Result<Vec<String>> {
    if verdict.fix_repairs_a_failure() {
        let message = if script_succeeded {
            "Some checks still failing after running fixes. Check permissions manually."
        } else {
            "Fix script failed. Check the output above for details."
        };
        return Err(Error::ExecutionFailed(message.to_string()));
    }
    let refusal = match verdict {
        SetupVerdict::Unfixable { refusal, .. } => Some(refusal_warning(refusal)),
        SetupVerdict::Ready | SetupVerdict::Fixable { .. } => None,
    };
    Ok(refusal
        .into_iter()
        .chain(warning_for_packages_left(programs))
        .collect())
}

/// What stays wrong when the user refuses the fixes of the setup, whose most
/// severe failure has `severity`, or `None` for fixes of warnings alone.
#[cfg(target_os = "linux")]
const fn consequence_of_refused_fixes(severity: FailureSeverity) -> Option<&'static str> {
    match severity {
        FailureSeverity::Warning => None,
        FailureSeverity::StopsEachBuild => {
            Some("The fixes were not made. Each container build stops until they are made.")
        }
        FailureSeverity::StopsTheDaemon => {
            Some("The fixes were not made. The daemon does not start until they are made.")
        }
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

#[cfg(not(target_os = "linux"))]
#[expect(
    clippy::unnecessary_wraps,
    reason = "the Linux form of this function can fail, and both forms have one caller"
)]
fn packages_status() -> Result<()> {
    println!("Container packages");
    println!("------------------");
    println!("  The packages that container nodes need are only required on Linux.");
    println!("  On macOS, containers run inside a Lima VM (no setup needed).");
    Ok(())
}

#[cfg(target_os = "linux")]
fn setup() -> Result<()> {
    let apptainer_dir = apptainer_dir()?;
    let status = containers::check_setup_status(&apptainer_dir);

    let fix = match &status.verdict {
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
        SetupVerdict::Unfixable { refusal, fix: None } => {
            eprintln!("{}", refusal_warning(refusal));
            return Ok(());
        }
        // The user reads, before a change of the machine, that the fixes
        // leave the container builds failing.
        SetupVerdict::Unfixable {
            refusal,
            fix: Some(fix),
        } => {
            eprintln!("{}", refusal_warning(refusal));
            eprintln!("{}", note_on_fixes_beside_a_refusal(fix.severity));
            eprintln!();
            fix
        }
        SetupVerdict::Fixable { fix } => fix,
    };

    println!("The following fixes are needed:");
    for (program, _) in status.programs.missing() {
        println!(
            "  - Install the {} package (provides {} for {})",
            program.package(),
            program.name(),
            program.purpose()
        );
    }
    if status.apparmor_restricted && !status.apparmor_ok {
        println!("  - Install/update AppArmor profile for Apptainer");
    } else if status.apparmor_restricted && !status.apparmor_loaded {
        println!("  - Load AppArmor profile for Apptainer into the kernel");
    }
    println!();

    // Check if we're in an interactive terminal
    if std::io::stdin().is_terminal() {
        print!("{} ", question_before_fixes(fix.uses_sudo));
        use std::io::Write;
        let _ = std::io::stdout().flush();

        let mut reply = String::new();
        let _ = std::io::stdin().read_line(&mut reply);
        let reply = reply.trim();
        if reply.eq_ignore_ascii_case("n") || reply.eq_ignore_ascii_case("no") {
            println!();
            println!("Skipped. You can apply the fixes manually:");
            println!();
            println!("{}", fix.script);
            // A caller, such as the installer, must not go on as if the
            // fixes of a failure were made.
            return match consequence_of_refused_fixes(fix.severity) {
                Some(consequence) => Err(Error::ExecutionFailed(consequence.to_string())),
                None => Ok(()),
            };
        }
    }

    let script_succeeded = std::process::Command::new("sh")
        .arg("-c")
        .arg(&fix.script)
        .status()
        .map_err(|e| Error::ExecutionFailed(format!("Failed to run fix script: {e}")))?
        .success();

    // The checks made again tell what the script repaired: a script that
    // stops at the package of a warning has made each fix before it.
    let recheck = containers::check_setup_status(&apptainer_dir);

    let warnings = end_after_fixes(&recheck.verdict, &recheck.programs, script_succeeded)?;
    if warnings.is_empty() {
        println!();
        println!("Container setup completed successfully.");
        return Ok(());
    }
    for warning in warnings {
        eprintln!();
        eprintln!("{warning}");
    }
    Ok(())
}

/// What the setup tells about its fixes on a machine whose kernel refuses
/// the user namespace of a container build: they do not repair the refusal,
/// and the daemon stops at its start without those of `severity`
/// [`FailureSeverity::StopsTheDaemon`].
#[cfg(target_os = "linux")]
const fn note_on_fixes_beside_a_refusal(severity: FailureSeverity) -> &'static str {
    match severity {
        FailureSeverity::StopsTheDaemon => {
            "The fixes below do not repair this cause. The daemon needs them to start."
        }
        FailureSeverity::StopsEachBuild | FailureSeverity::Warning => {
            "The fixes below do not repair this cause."
        }
    }
}

/// The question of the setup before it makes its fixes. A fix script that
/// `uses_sudo` can ask for the password of the user.
#[cfg(target_os = "linux")]
const fn question_before_fixes(uses_sudo: bool) -> &'static str {
    if uses_sudo {
        return "Apply these fixes now? (requires sudo) [Y/n]";
    }
    "Apply these fixes now? [Y/n]"
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

/// What the tests of the commands that read the state of the container setup
/// share.
#[cfg(all(test, target_os = "linux"))]
pub(crate) mod test_fixtures {
    use containers::{FailureSeverity, SetupFix};

    /// The fix of checks that fail with `severity`.
    pub(crate) fn fix_of(severity: FailureSeverity) -> SetupFix {
        SetupFix {
            script: "sudo apt-get install -y uidmap".to_string(),
            severity,
            uses_sudo: true,
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::test_fixtures::fix_of;
    use super::*;
    use containers::{SystemProgram, UnprivilegedUserNamespace};

    const ANSWER: &str = "unshare: unshare failed: Operation not permitted";
    const REFUSAL: &str = "The kernel refuses a user namespace.";
    const RUN_THE_SETUP: &str = "Some checks failed. Run `peppy container setup` to fix.";

    fn fixable(severity: FailureSeverity) -> SetupVerdict {
        SetupVerdict::Fixable {
            fix: fix_of(severity),
        }
    }

    /// The verdict of a refusal that has no fix, with or without another
    /// check that fails with `severity` and has one.
    fn unfixable(severity: Option<FailureSeverity>) -> SetupVerdict {
        SetupVerdict::Unfixable {
            refusal: REFUSAL.to_string(),
            fix: severity.map(fix_of),
        }
    }

    fn check(program: SystemProgram, state: ProgramState) -> ProgramCheck {
        ProgramCheck { program, state }
    }

    /// The checks of a machine on which the programs of `missing` are
    /// missing, each with what its absence does to the daemon.
    fn programs_without(missing: &[(SystemProgram, FailureSeverity)]) -> SystemPrograms {
        SystemPrograms {
            checks: SystemProgram::ALL.map(|program| {
                let severity = missing
                    .iter()
                    .find(|(candidate, _)| *candidate == program)
                    .map(|(_, severity)| *severity);
                check(
                    program,
                    severity.map_or(ProgramState::Found, ProgramState::Missing),
                )
            }),
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
    fn the_node_images_line_shows_how_a_node_gets_its_image() {
        let unpacked = ImageFiles::Unpacked {
            directories_on_overlayfs: vec!["/tmp".into(), "/home/operator".into()],
        };

        assert_eq!(image_files_state(&ImageFiles::Mounted), "mounted");
        assert_eq!(
            image_files_state(&unpacked),
            "unpacked at each start (`/tmp` and `/home/operator` are on overlayfs)"
        );
    }

    /// The note tells the cost and the two ways to mounted images, and
    /// mounted images get none.
    #[test]
    fn the_note_on_unpacked_images_tells_how_to_get_mounted_images() {
        let unpacked = ImageFiles::Unpacked {
            directories_on_overlayfs: vec!["/tmp".into()],
        };

        let note = note_on_unpacked_images(&unpacked).expect("unpacked images have a note");

        assert!(note.starts_with("Note: each node starts from an unpacked copy"));
        assert!(note.contains("a volume or a tmpfs"));
        assert!(note.contains("run the daemon as root"));
        assert!(note.ends_with(UNPACKED_IMAGES_GUIDE));
        assert_eq!(note_on_unpacked_images(&ImageFiles::Mounted), None);
    }

    /// The line of a missing program names the package to install. A
    /// program that no build of peppy runs is a warning.
    #[test]
    fn the_line_of_a_program_shows_what_its_check_found() {
        let cases = [
            (check(SystemProgram::Newuidmap, ProgramState::Found), "OK"),
            (
                check(SystemProgram::Newuidmap, ProgramState::NotRequired),
                "not required (the builds of root do not run it)",
            ),
            (
                check(
                    SystemProgram::Newuidmap,
                    ProgramState::Missing(FailureSeverity::StopsEachBuild),
                ),
                "FAILED (install the uidmap package)",
            ),
            (
                check(
                    SystemProgram::Mksquashfs,
                    ProgramState::Missing(FailureSeverity::StopsEachBuild),
                ),
                "FAILED (install the squashfs-tools package)",
            ),
            (
                check(
                    SystemProgram::Fuse2fs,
                    ProgramState::Missing(FailureSeverity::Warning),
                ),
                "WARNING (install the fuse2fs package for the mount of an EXT3 image)",
            ),
        ];
        for (check, state) in cases {
            assert_eq!(program_state(check), state, "{check:?}");
        }
    }

    #[test]
    fn a_status_that_passes_has_no_advice() {
        assert_eq!(advice_for_failing_checks(&SetupVerdict::Ready), None);
        assert_eq!(
            summary_of_passing_checks(&SetupVerdict::Ready),
            "All checks passed. Container support is ready."
        );
    }

    /// The nodes that peppy builds run without `fuse2fs`: the status passes,
    /// and its last line names the command that installs the package.
    #[test]
    fn a_status_whose_one_failure_is_a_warning_passes_and_names_the_setup() {
        let verdict = fixable(FailureSeverity::Warning);

        assert_eq!(advice_for_failing_checks(&verdict), None);
        assert_eq!(
            summary_of_passing_checks(&verdict),
            "Container support is ready. Run `peppy container setup` to fix the warnings."
        );
    }

    #[test]
    fn a_failing_status_tells_to_run_the_setup_that_has_a_fix() {
        for severity in [
            FailureSeverity::StopsEachBuild,
            FailureSeverity::StopsTheDaemon,
        ] {
            assert_eq!(
                advice_for_failing_checks(&fixable(severity)).as_deref(),
                Some(RUN_THE_SETUP),
                "{severity:?}"
            );
        }
    }

    #[test]
    fn a_failing_status_explains_the_refusal_that_has_no_fix() {
        for no_fix_of_a_failure in [None, Some(FailureSeverity::Warning)] {
            assert_eq!(
                advice_for_failing_checks(&unfixable(no_fix_of_a_failure)).as_deref(),
                Some(REFUSAL),
                "{no_fix_of_a_failure:?}"
            );
        }

        // A second check that fails, with a fix for a failure: the advice
        // has the two.
        for severity in [
            FailureSeverity::StopsEachBuild,
            FailureSeverity::StopsTheDaemon,
        ] {
            assert_eq!(
                advice_for_failing_checks(&unfixable(Some(severity))),
                Some(format!("{RUN_THE_SETUP}\n{REFUSAL}")),
                "{severity:?}"
            );
        }
    }

    #[test]
    fn a_machine_that_has_each_package_gets_no_advice() {
        assert_eq!(advice_for_missing_packages(&programs_without(&[])), None);
    }

    /// The advice names the packages that a container build does not run
    /// without, and its command installs each missing package.
    #[test]
    fn the_advice_names_each_missing_package_that_a_build_needs() {
        let programs = programs_without(&[
            (SystemProgram::Newuidmap, FailureSeverity::StopsEachBuild),
            (SystemProgram::Mksquashfs, FailureSeverity::StopsEachBuild),
            (SystemProgram::Fuse2fs, FailureSeverity::Warning),
        ]);
        let install_command = programs
            .install_command()
            .expect("missing programs have packages to install");

        let (severity, advice) =
            advice_for_missing_packages(&programs).expect("packages are missing");

        assert_eq!(severity, FailureSeverity::StopsEachBuild);
        let (cause, command) = advice
            .split_once("\nTo install each missing package, run the following command:\n\n")
            .expect("the advice gives the command after the cause");
        assert_eq!(
            cause,
            "Each container build stops without packages that this machine does not have: \
             uidmap, squashfs-tools."
        );
        assert_eq!(command, install_command);
        assert!(
            command.contains("install -y uidmap squashfs-tools")
                && command.ends_with("install -y fuse2fs"),
            "{command}"
        );
    }

    #[test]
    fn the_advice_for_a_package_that_no_build_needs_is_a_warning() {
        let programs = programs_without(&[(SystemProgram::Fuse2fs, FailureSeverity::Warning)]);

        let (severity, advice) =
            advice_for_missing_packages(&programs).expect("a package is missing");

        assert_eq!(severity, FailureSeverity::Warning);
        assert!(
            advice.starts_with(
                "This machine does not have packages that container nodes build and run \
                 without: fuse2fs.\n"
            ),
            "{advice}"
        );
        assert!(advice.ends_with("install -y fuse2fs"), "{advice}");
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

    /// Beside a refusal, the setup tells that the daemon needs its fixes
    /// only when one of them repairs a failure that stops the daemon.
    #[test]
    fn the_note_on_fixes_beside_a_refusal_tells_whether_the_daemon_needs_them() {
        assert_eq!(
            note_on_fixes_beside_a_refusal(FailureSeverity::StopsTheDaemon),
            "The fixes below do not repair this cause. The daemon needs them to start."
        );
        for severity in [FailureSeverity::StopsEachBuild, FailureSeverity::Warning] {
            assert_eq!(
                note_on_fixes_beside_a_refusal(severity),
                "The fixes below do not repair this cause.",
                "{severity:?}"
            );
        }
    }

    /// The setup ends with an error when the user refuses the fixes of a
    /// failure, and tells what stays wrong. Fixes of warnings alone can be
    /// refused.
    #[test]
    fn refused_fixes_of_a_failure_have_a_consequence() {
        assert_eq!(
            consequence_of_refused_fixes(FailureSeverity::StopsTheDaemon),
            Some("The fixes were not made. The daemon does not start until they are made.")
        );
        assert_eq!(
            consequence_of_refused_fixes(FailureSeverity::StopsEachBuild),
            Some("The fixes were not made. Each container build stops until they are made.")
        );
        assert_eq!(consequence_of_refused_fixes(FailureSeverity::Warning), None);
    }

    /// Root runs the fix script with no `sudo`, so the question names
    /// `sudo` for a user that is not root only.
    #[test]
    fn the_question_before_the_fixes_names_sudo_for_a_script_that_uses_it() {
        assert_eq!(
            question_before_fixes(true),
            "Apply these fixes now? (requires sudo) [Y/n]"
        );
        assert_eq!(question_before_fixes(false), "Apply these fixes now? [Y/n]");
    }

    /// The checks of a machine that has each package.
    fn each_package() -> SystemPrograms {
        programs_without(&[])
    }

    /// The checks of a machine without `fuse2fs`, whose absence is a
    /// warning.
    fn no_fuse2fs() -> SystemPrograms {
        programs_without(&[(SystemProgram::Fuse2fs, FailureSeverity::Warning)])
    }

    #[test]
    fn a_setup_whose_fixes_leave_each_check_passing_ends_with_success() {
        for script_succeeded in [true, false] {
            assert_eq!(
                end_after_fixes(&SetupVerdict::Ready, &each_package(), script_succeeded).unwrap(),
                Vec::<String>::new(),
                "script_succeeded={script_succeeded}"
            );
        }
    }

    /// A refusal has no fix, so a setup that made each of its fixes ends
    /// with success and the warning.
    #[test]
    fn a_setup_whose_fixes_leave_a_refusal_ends_with_success_and_a_warning() {
        assert_eq!(
            end_after_fixes(&unfixable(None), &each_package(), true).unwrap(),
            [refusal_warning(REFUSAL)]
        );
    }

    /// The package of a warning is the last step of the fix script. A script
    /// that stops there (a distribution without the package, no network) has
    /// made each fix that the daemon and its builds need: the setup ends
    /// with success and a warning, and the installer goes on.
    #[test]
    fn a_setup_that_leaves_the_package_of_a_warning_ends_with_success_and_a_warning() {
        const PACKAGE_LEFT: &str = "Warning: these packages are not installed: fuse2fs (for the \
             mount of an EXT3 image). The nodes that peppy builds build and run without them.";

        for script_succeeded in [true, false] {
            assert_eq!(
                end_after_fixes(
                    &fixable(FailureSeverity::Warning),
                    &no_fuse2fs(),
                    script_succeeded
                )
                .unwrap(),
                [PACKAGE_LEFT],
                "script_succeeded={script_succeeded}"
            );
        }
        assert_eq!(
            end_after_fixes(
                &unfixable(Some(FailureSeverity::Warning)),
                &no_fuse2fs(),
                false
            )
            .unwrap(),
            [refusal_warning(REFUSAL), PACKAGE_LEFT.to_string()]
        );
    }

    /// A failure that has a fix and fails again is the error of the setup.
    /// The error tells whether the script itself failed.
    #[test]
    fn a_failure_that_stays_after_its_fix_is_the_error_of_the_setup() {
        for verdict in [
            fixable(FailureSeverity::StopsTheDaemon),
            fixable(FailureSeverity::StopsEachBuild),
            unfixable(Some(FailureSeverity::StopsTheDaemon)),
            unfixable(Some(FailureSeverity::StopsEachBuild)),
        ] {
            assert_eq!(
                message_of(end_after_fixes(&verdict, &each_package(), true).unwrap_err()),
                "Some checks still failing after running fixes. Check permissions manually.",
                "{verdict:?}"
            );
            assert_eq!(
                message_of(end_after_fixes(&verdict, &each_package(), false).unwrap_err()),
                "Fix script failed. Check the output above for details.",
                "{verdict:?}"
            );
        }
    }
}
