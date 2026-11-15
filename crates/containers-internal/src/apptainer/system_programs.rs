//! The programs of the Linux distribution that the container nodes need.
//!
//! peppy bundles `squashfuse_ll` and `gocryptfs` with Apptainer. Apptainer
//! runs three more programs, and each one comes from a package of the
//! distribution:
//!
//! | Program      | Package          | What uses it                                   |
//! |--------------|------------------|------------------------------------------------|
//! | `newuidmap`  | `uidmap`         | The fakeroot build of a user that is not root. |
//! | `mksquashfs` | `squashfs-tools` | Each node build: it packs the image.           |
//! | `fuse2fs`    | `fuse2fs`        | The mount of an EXT3 image.                    |
//!
//! [`SystemProgram`] is the only list of these programs and of their
//! packages: the checks of the setup and the step of the fix script that
//! installs the packages read it.
//!
//! `newuidmap` is a setuid root program, so it comes from the distribution in
//! all cases, and the fix script has a step that installs packages. The two
//! other programs are packages of that step, and not programs that peppy
//! builds for each architecture.

use std::path::PathBuf;

/// The `apt-get` of the fix script. `apt-get` stops at once when another
/// process holds the dpkg lock, and on a machine that started a short time
/// ago cloud-init and unattended-upgrades hold it for a minute or two. With
/// this option `apt-get` waits for the lock, for 300 seconds at most.
const APT_GET: &str = "apt-get -o DPkg::Lock::Timeout=300";

/// What joins two commands of the fix script: the second one runs when the
/// first one ends with success, and starts on a line of its own.
pub(crate) const AND_THEN: &str = " \\\n  && ";

/// A program of the Linux distribution that Apptainer runs and that peppy
/// does not bundle with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SystemProgram {
    /// Maps the subordinate ids of a user into the user namespace of a
    /// fakeroot build. Root builds with no fakeroot, so it does not run the
    /// program.
    Newuidmap,
    /// Packs the image of each node build.
    Mksquashfs,
    /// Mounts an EXT3 image. The squashfs images that peppy builds run
    /// without it, and each container start then writes `fuse2fs not found,
    /// will not be able to mount EXT3 filesystems`.
    Fuse2fs,
}

impl SystemProgram {
    /// Each program, in the order of the checks.
    pub const ALL: [Self; 3] = [Self::Newuidmap, Self::Mksquashfs, Self::Fuse2fs];

    /// The name of the program, which Apptainer looks for in `PATH`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Newuidmap => "newuidmap",
            Self::Mksquashfs => "mksquashfs",
            Self::Fuse2fs => "fuse2fs",
        }
    }

    /// The package of the distribution that installs the program.
    #[must_use]
    pub const fn package(self) -> &'static str {
        match self {
            Self::Newuidmap => "uidmap",
            Self::Mksquashfs => "squashfs-tools",
            Self::Fuse2fs => "fuse2fs",
        }
    }

    /// What Apptainer does with the program.
    #[must_use]
    pub const fn purpose(self) -> &'static str {
        match self {
            Self::Newuidmap => "the fakeroot build of a user that is not root",
            Self::Mksquashfs => "each node build, which packs its image with it",
            Self::Fuse2fs => "the mount of an EXT3 image",
        }
    }

    /// What the absence of the program does to the daemon of a user that
    /// `builds_as_root` or not, or `None` when no build of that user runs the
    /// program.
    const fn severity_when_missing(self, builds_as_root: bool) -> Option<FailureSeverity> {
        match self {
            Self::Newuidmap if builds_as_root => None,
            Self::Newuidmap | Self::Mksquashfs => Some(FailureSeverity::StopsTheDaemon),
            Self::Fuse2fs => Some(FailureSeverity::Warning),
        }
    }
}

/// What a check that fails does to the daemon. The order is from the least
/// severe to the most severe.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FailureSeverity {
    /// The daemon starts, and the nodes that peppy builds run.
    Warning,
    /// The daemon stops at its start.
    StopsTheDaemon,
}

/// What the check of one [`SystemProgram`] finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramState {
    /// The program is in `PATH`.
    Found,
    /// No build of the user runs the program, so the check does not look for
    /// it.
    NotRequired,
    /// The program is not in `PATH`.
    Missing(FailureSeverity),
}

impl ProgramState {
    /// The state of `program` for a user that `builds_as_root` or not, on a
    /// machine where the program `is_in_path` or not.
    const fn parse(program: SystemProgram, builds_as_root: bool, is_in_path: bool) -> Self {
        let Some(severity) = program.severity_when_missing(builds_as_root) else {
            return Self::NotRequired;
        };
        if is_in_path {
            return Self::Found;
        }
        Self::Missing(severity)
    }
}

/// One [`SystemProgram`] and what its check finds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProgramCheck {
    pub program: SystemProgram,
    pub state: ProgramState,
}

/// The check of each [`SystemProgram`], in the order of
/// [`SystemProgram::ALL`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPrograms {
    pub checks: [ProgramCheck; 3],
}

impl SystemPrograms {
    /// Looks for each program in `PATH`, for a user that `builds_as_root` or
    /// not.
    pub(crate) fn check(builds_as_root: bool) -> Self {
        Self::parse(builds_as_root, |program| which(program.name()).is_some())
    }

    /// The checks for a user that `builds_as_root` or not, on a machine where
    /// `is_in_path` tells which programs `PATH` has.
    pub(crate) fn parse(builds_as_root: bool, is_in_path: impl Fn(SystemProgram) -> bool) -> Self {
        Self {
            checks: SystemProgram::ALL.map(|program| ProgramCheck {
                program,
                state: ProgramState::parse(program, builds_as_root, is_in_path(program)),
            }),
        }
    }

    /// Each program that is missing, with what its absence does to the
    /// daemon.
    pub fn missing(&self) -> impl Iterator<Item = (SystemProgram, FailureSeverity)> {
        self.checks.iter().filter_map(|check| match check.state {
            ProgramState::Missing(severity) => Some((check.program, severity)),
            ProgramState::Found | ProgramState::NotRequired => None,
        })
    }

    /// What the most severe absence does to the daemon, or `None` when no
    /// program is missing.
    #[must_use]
    pub fn severity(&self) -> Option<FailureSeverity> {
        self.missing().map(|(_, severity)| severity).max()
    }

    /// The shell command that installs the package of each program that is
    /// missing, for the user of this process, or `None` when no program is
    /// missing. It is the step of the fix script that installs the packages.
    #[must_use]
    pub fn install_command(&self) -> Option<String> {
        self.install_step(Privilege::of_this_process())
    }

    /// As [`SystemPrograms::install_command`], for a user that has
    /// `privilege`. The step reads the package lists before the installation:
    /// a machine that has none (a container of a bare image) finds no package
    /// without them.
    pub(crate) fn install_step(&self, privilege: Privilege) -> Option<String> {
        let packages: Vec<&str> = self
            .missing()
            .map(|(program, _)| program.package())
            .collect();
        if packages.is_empty() {
            return None;
        }
        let apt_get = format!("{}{APT_GET}", privilege.prefix());
        Some(format!(
            "{apt_get} update{AND_THEN}{apt_get} install -y {}",
            packages.join(" ")
        ))
    }
}

/// How a command of the fix script gets the rights of root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Privilege {
    /// The user that runs the script is root. A machine of root alone (a
    /// container of a bare image) has no `sudo`.
    Root,
    /// The user that runs the script is not root: each command that needs
    /// the rights of root runs through `sudo`.
    Sudo,
}

impl Privilege {
    /// The privilege of the user of this process, who runs the fix script:
    /// `peppy container setup` runs it, and the error of the daemon gives it
    /// to the user of the daemon.
    pub(crate) fn of_this_process() -> Self {
        if nix::unistd::geteuid().is_root() {
            return Self::Root;
        }
        Self::Sudo
    }

    /// What starts a command that needs the rights of root.
    pub(crate) const fn prefix(self) -> &'static str {
        match self {
            Self::Root => "",
            Self::Sudo => "sudo ",
        }
    }
}

/// The path of the program `name` in `PATH`, or `None` when no directory of
/// `PATH` has it.
fn which(name: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let full = dir.join(name);
        full.is_file().then_some(full)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const UPDATE: &str = "apt-get -o DPkg::Lock::Timeout=300 update";
    const INSTALL: &str = "apt-get -o DPkg::Lock::Timeout=300 install -y";

    /// The checks of a machine that has the programs `in_path` alone.
    fn programs(builds_as_root: bool, in_path: &[SystemProgram]) -> SystemPrograms {
        SystemPrograms::parse(builds_as_root, |program| in_path.contains(&program))
    }

    fn states(programs: &SystemPrograms) -> [ProgramState; 3] {
        programs.checks.map(|check| check.state)
    }

    #[test]
    fn each_program_comes_from_one_package_of_the_distribution() {
        let packages = SystemProgram::ALL.map(|program| (program.name(), program.package()));

        assert_eq!(
            packages,
            [
                ("newuidmap", "uidmap"),
                ("mksquashfs", "squashfs-tools"),
                ("fuse2fs", "fuse2fs"),
            ]
        );
    }

    #[test]
    fn the_checks_are_in_the_order_of_the_list() {
        let programs = programs(false, &[]);

        assert_eq!(
            programs.checks.map(|check| check.program),
            SystemProgram::ALL
        );
    }

    #[test]
    fn a_machine_that_has_each_program_misses_none() {
        for builds_as_root in [false, true] {
            let programs = programs(builds_as_root, &SystemProgram::ALL);

            assert_eq!(programs.missing().count(), 0, "root={builds_as_root}");
            assert_eq!(programs.severity(), None, "root={builds_as_root}");
            for privilege in [Privilege::Root, Privilege::Sudo] {
                assert_eq!(programs.install_step(privilege), None, "{privilege:?}");
            }
        }
    }

    /// A user that is not root builds with fakeroot, which runs `newuidmap`:
    /// without it, and without `mksquashfs`, the daemon stops. Without
    /// `fuse2fs` the daemon starts.
    #[test]
    fn a_user_that_is_not_root_needs_newuidmap_and_mksquashfs() {
        assert_eq!(
            states(&programs(false, &[])),
            [
                ProgramState::Missing(FailureSeverity::StopsTheDaemon),
                ProgramState::Missing(FailureSeverity::StopsTheDaemon),
                ProgramState::Missing(FailureSeverity::Warning),
            ]
        );
    }

    /// Root builds with no fakeroot: the check does not look for `newuidmap`,
    /// on a machine that has it and on one that does not.
    #[test]
    fn root_does_not_need_newuidmap() {
        for in_path in [&[][..], &[SystemProgram::Newuidmap][..]] {
            assert_eq!(
                states(&programs(true, in_path)),
                [
                    ProgramState::NotRequired,
                    ProgramState::Missing(FailureSeverity::StopsTheDaemon),
                    ProgramState::Missing(FailureSeverity::Warning),
                ],
                "in PATH: {in_path:?}"
            );
        }
    }

    #[test]
    fn the_severity_is_the_one_of_the_most_severe_absence() {
        let without_fuse2fs = programs(
            false,
            &[SystemProgram::Newuidmap, SystemProgram::Mksquashfs],
        );
        let without_mksquashfs = programs(false, &[SystemProgram::Newuidmap]);

        assert_eq!(without_fuse2fs.severity(), Some(FailureSeverity::Warning));
        assert_eq!(
            without_mksquashfs.severity(),
            Some(FailureSeverity::StopsTheDaemon)
        );
    }

    /// One step installs the package of each program that is missing, after
    /// it reads the package lists. Root runs the two commands as they are,
    /// and each other user runs them through `sudo`.
    #[test]
    fn the_install_step_holds_each_missing_package_and_sudo_for_a_user_that_is_not_root() {
        // (builds_as_root, programs in PATH, privilege, step)
        let cases = [
            (
                false,
                &[][..],
                Privilege::Sudo,
                format!("sudo {UPDATE} \\\n  && sudo {INSTALL} uidmap squashfs-tools fuse2fs"),
            ),
            (
                true,
                &[][..],
                Privilege::Root,
                format!("{UPDATE} \\\n  && {INSTALL} squashfs-tools fuse2fs"),
            ),
            // `sudo` runs the command of a user that is not root: the builds
            // are the ones of that user, and the script runs as root.
            (
                false,
                &[SystemProgram::Mksquashfs, SystemProgram::Fuse2fs][..],
                Privilege::Root,
                format!("{UPDATE} \\\n  && {INSTALL} uidmap"),
            ),
            (
                false,
                &[SystemProgram::Newuidmap, SystemProgram::Mksquashfs][..],
                Privilege::Sudo,
                format!("sudo {UPDATE} \\\n  && sudo {INSTALL} fuse2fs"),
            ),
        ];
        for (builds_as_root, in_path, privilege, step) in cases {
            assert_eq!(
                programs(builds_as_root, in_path).install_step(privilege),
                Some(step),
                "root={builds_as_root} in PATH: {in_path:?} {privilege:?}"
            );
        }
    }

    /// The command for the user of this process has `sudo` for each user but
    /// root.
    #[test]
    fn the_install_command_of_this_process_has_the_privilege_of_its_user() {
        let programs = programs(false, &[]);
        let is_root = nix::unistd::geteuid().is_root();

        let command = programs
            .install_command()
            .expect("a machine without the programs has packages to install");

        assert_eq!(command.starts_with("sudo "), !is_root, "{command}");
        assert_eq!(
            Some(command),
            programs.install_step(Privilege::of_this_process())
        );
    }

    #[test]
    fn a_program_is_found_in_a_directory_of_path_only() {
        assert!(which("sh").is_some(), "`sh` is in PATH on each Linux host");
        assert_eq!(which("peppy-no-such-program-in-path"), None);
    }
}
