//! The user namespace that the kernel gives to a program with no privilege.
//!
//! Before each build for a user that is not root, the `apptainer` program
//! asks the kernel for a user namespace and runs `/bin/true` in it. When the
//! kernel refuses, the build stops with "/etc/subuid mapping found but no
//! user namespace available for fakeroot". The AppArmor profile that
//! `peppy container setup` writes is for the `starter` program of apptainer
//! alone, so it has no effect on this request: the kernel reads it as the
//! request of a program that has no profile of its own.
//!
//! [`UnprivilegedUserNamespace::for_this_user`] makes the same request with
//! `unshare --user /bin/true`. `unshare`, of the util-linux package, has no
//! AppArmor profile of its own either, so the kernel gives it the answer that
//! it gives to `apptainer`. A program makes the request, and not this
//! process, because a process must fork to make it and the crate has no
//! `unsafe` code.
//!
//! One cause of a refusal has a repair. Where AppArmor restricts user
//! namespaces (`kernel.apparmor_restrict_unprivileged_userns=1`, the default
//! of Ubuntu 24.04 and later), the kernel gives a namespace to a program that
//! has no profile of its own through the profile `unprivileged_userns` only.
//! The `apparmor` package installs that profile and `apparmor.service` loads
//! it at boot, so the kernel of a machine whose service is stopped does not
//! have it. `peppy container setup` loads it. For every other cause (a sysctl
//! that turns user namespaces off, the seccomp profile of a container) peppy
//! has no repair, and it reports the answer of the kernel.

use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

use super::facade::is_apparmor_profile_loaded;

/// The program that makes the request, of the util-linux package.
const UNSHARE: &str = "unshare";

/// The program that `unshare` runs in the namespace: the one that apptainer
/// runs in the namespace of its own request.
const PROGRAM_IN_NAMESPACE: &str = "/bin/true";

/// The AppArmor profile through which the kernel gives a user namespace to a
/// program that has no profile of its own.
const DEFAULT_PROFILE_NAME: &str = "unprivileged_userns";

/// The file of [`DEFAULT_PROFILE_NAME`], which the `apparmor` package installs.
pub(crate) const DEFAULT_PROFILE_FILE: &str = "/etc/apparmor.d/unprivileged_userns";

/// The answer of the kernel to the request of a user namespace by a program
/// that has no privilege and no AppArmor profile of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnprivilegedUserNamespace {
    /// The user is root: apptainer builds as root with no user namespace, so
    /// no request is made.
    NotRequired,
    /// The kernel gave the namespace.
    Granted,
    /// The kernel refused the namespace, with `answer`, because the AppArmor
    /// profile `unprivileged_userns` is not loaded. `peppy container setup`
    /// loads it.
    RefusedWithoutProfile { answer: String },
    /// The kernel refused the namespace, with `answer`, for a cause that
    /// `peppy container setup` has no repair for.
    Refused { answer: String },
}

impl UnprivilegedUserNamespace {
    /// Makes the request for the user of this process. `apparmor_restricted`
    /// tells whether AppArmor restricts user namespaces on this machine.
    pub(crate) fn for_this_user(apparmor_restricted: bool) -> Self {
        if nix::unistd::geteuid().is_root() {
            return Self::NotRequired;
        }
        let request = Command::new(UNSHARE)
            .args(["--user", PROGRAM_IN_NAMESPACE])
            // The user reads the message of a refusal as `unshare` writes it,
            // and the C locale keeps it the same on every machine.
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .output();
        let Err(answer) = kernel_answer(request) else {
            return Self::Granted;
        };
        let repaired_by_profile = profile_load_repairs(
            apparmor_restricted,
            is_apparmor_profile_loaded(DEFAULT_PROFILE_NAME),
            Path::new(DEFAULT_PROFILE_FILE).is_file(),
        );
        Self::refused(answer, repaired_by_profile)
    }

    /// The refusal that carries `answer`, with or without a repair.
    fn refused(answer: String, repaired_by_profile: bool) -> Self {
        if repaired_by_profile {
            Self::RefusedWithoutProfile { answer }
        } else {
            Self::Refused { answer }
        }
    }

    /// Whether a container build of this user gets what it needs.
    pub fn is_usable(&self) -> bool {
        matches!(self, Self::NotRequired | Self::Granted)
    }

    /// Whether `peppy container setup` repairs the refusal with a load of the
    /// profile `unprivileged_userns`.
    pub fn needs_profile(&self) -> bool {
        matches!(self, Self::RefusedWithoutProfile { .. })
    }

    /// The explanation of a refusal that `peppy container setup` has no
    /// repair for, or `None` for every other answer.
    pub fn refusal_without_repair(&self) -> Option<String> {
        let Self::Refused { answer } = self else {
            return None;
        };
        Some(format!(
            "The kernel refuses a user namespace to a program that has no privilege \
             ({answer}). Apptainer needs such a namespace for each container build, and \
             `peppy container setup` has no repair for this cause."
        ))
    }
}

/// Whether a load of the profile `unprivileged_userns` repairs a refusal:
/// AppArmor restricts user namespaces, the kernel does not have the profile,
/// and the machine has the file to load it from.
fn profile_load_repairs(apparmor_restricted: bool, loaded: bool, on_disk: bool) -> bool {
    apparmor_restricted && !loaded && on_disk
}

/// What the kernel answered to the request that `unshare` made: `Ok` when it
/// gave the namespace and the program ran in it, else the message to show.
/// That message is the last line that `unshare` wrote, which names the system
/// call and the error of the kernel.
fn kernel_answer(request: io::Result<Output>) -> Result<(), String> {
    let output = request.map_err(|e| format!("cannot run `{UNSHARE}`: {e}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let message = stderr.lines().map(str::trim).rfind(|line| !line.is_empty());
    Err(message.map_or_else(
        || format!("`{UNSHARE}` ended with {}", output.status),
        str::to_owned,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    /// The output of an `unshare` that ended with `exit_code` and wrote
    /// `stderr`.
    fn unshare_output(exit_code: i32, stderr: &str) -> Output {
        Output {
            // The exit code is the second byte of a wait status.
            status: ExitStatus::from_raw(exit_code << 8),
            stdout: Vec::new(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    #[test]
    fn a_request_that_ends_with_success_is_a_grant() {
        assert_eq!(kernel_answer(Ok(unshare_output(0, ""))), Ok(()));
    }

    #[test]
    fn a_refusal_carries_the_last_line_that_unshare_wrote() {
        assert_eq!(
            kernel_answer(Ok(unshare_output(
                1,
                "unshare: unshare failed: Permission denied\n"
            ))),
            Err("unshare: unshare failed: Permission denied".to_string())
        );
        assert_eq!(
            kernel_answer(Ok(unshare_output(
                1,
                "a line before\n  unshare: unshare failed: Operation not permitted  \n\n"
            ))),
            Err("unshare: unshare failed: Operation not permitted".to_string())
        );
    }

    #[test]
    fn a_refusal_with_no_message_carries_the_exit_status() {
        assert_eq!(
            kernel_answer(Ok(unshare_output(1, " \n"))),
            Err("`unshare` ended with exit status: 1".to_string())
        );
    }

    #[test]
    fn a_machine_without_unshare_carries_the_error_of_the_start() {
        let no_program = Err(io::Error::new(io::ErrorKind::NotFound, "no such program"));

        assert_eq!(
            kernel_answer(no_program),
            Err("cannot run `unshare`: no such program".to_string())
        );
    }

    /// The load of the profile is the repair only when AppArmor restricts
    /// user namespaces, the kernel does not have the profile, and the machine
    /// has its file.
    #[test]
    fn the_profile_load_repairs_only_a_restricted_machine_that_has_the_file() {
        // (apparmor_restricted, loaded, on_disk, repairs)
        let cases = [
            (true, false, true, true),
            (true, false, false, false),
            (true, true, true, false),
            (true, true, false, false),
            (false, false, true, false),
            (false, false, false, false),
            (false, true, true, false),
            (false, true, false, false),
        ];
        for (apparmor_restricted, loaded, on_disk, repairs) in cases {
            assert_eq!(
                profile_load_repairs(apparmor_restricted, loaded, on_disk),
                repairs,
                "apparmor_restricted={apparmor_restricted} loaded={loaded} on_disk={on_disk}"
            );
        }
    }

    #[test]
    fn a_refusal_names_its_repair_or_explains_that_it_has_none() {
        let answer = "unshare: unshare failed: Permission denied";

        let with_repair = UnprivilegedUserNamespace::refused(answer.to_string(), true);
        assert_eq!(
            with_repair,
            UnprivilegedUserNamespace::RefusedWithoutProfile {
                answer: answer.to_string()
            }
        );
        assert!(with_repair.needs_profile());
        assert!(!with_repair.is_usable());
        assert_eq!(with_repair.refusal_without_repair(), None);

        let without_repair = UnprivilegedUserNamespace::refused(answer.to_string(), false);
        assert_eq!(
            without_repair,
            UnprivilegedUserNamespace::Refused {
                answer: answer.to_string()
            }
        );
        assert!(!without_repair.needs_profile());
        assert!(!without_repair.is_usable());
        assert_eq!(
            without_repair.refusal_without_repair().as_deref(),
            Some(
                "The kernel refuses a user namespace to a program that has no privilege \
                 (unshare: unshare failed: Permission denied). Apptainer needs such a \
                 namespace for each container build, and `peppy container setup` has no \
                 repair for this cause."
            )
        );
    }

    #[test]
    fn a_grant_and_a_root_user_need_no_repair() {
        for answer in [
            UnprivilegedUserNamespace::Granted,
            UnprivilegedUserNamespace::NotRequired,
        ] {
            assert!(answer.is_usable(), "{answer:?}");
            assert!(!answer.needs_profile(), "{answer:?}");
            assert_eq!(answer.refusal_without_repair(), None, "{answer:?}");
        }
    }

    /// The request of this host ends with an answer of the right kind: none
    /// for root, and for every other user a grant or a refusal that carries
    /// a message. With no AppArmor restriction given, no refusal names the
    /// profile as its repair.
    #[test]
    fn the_request_of_this_host_ends_with_an_answer() {
        let is_root = nix::unistd::geteuid().is_root();

        match UnprivilegedUserNamespace::for_this_user(false) {
            UnprivilegedUserNamespace::NotRequired => assert!(is_root),
            UnprivilegedUserNamespace::Granted => assert!(!is_root),
            UnprivilegedUserNamespace::Refused { answer } => {
                assert!(!is_root);
                assert!(!answer.is_empty());
            }
            UnprivilegedUserNamespace::RefusedWithoutProfile { answer } => {
                panic!("no AppArmor restriction was given, yet the repair is the profile: {answer}")
            }
        }
    }
}
