//! The user namespace that the kernel gives to a container build.
//!
//! Before each build for a user that is not root, the `apptainer` program
//! asks the kernel for a user namespace and runs `/bin/true` in it. When the
//! kernel refuses, the build stops with "/etc/subuid mapping found but no
//! user namespace available for fakeroot".
//!
//! [`UnprivilegedUserNamespace::for_build_user`] makes the same request with
//! `unshare --user /bin/true`, as the user of the builds. A program makes the
//! request, and not this process, because a process must fork to make it and
//! the crate has no `unsafe` code.
//!
//! peppy has no fix for a refusal (a sysctl that turns user namespaces off,
//! the seccomp profile of a container): it reports the answer of the kernel.
//!
//! One answer of `unshare` tells nothing about `apptainer`. Where AppArmor
//! restricts user namespaces (`kernel.apparmor_restrict_unprivileged_userns=1`,
//! the default of Ubuntu 24.04 and later), the kernel gives a namespace to a
//! program through an AppArmor profile only. `apptainer` has the profile of
//! its installation (see `ApparmorProfileRef`). `unshare`, of the util-linux
//! package, has none, and gets its namespace through the profile
//! `unprivileged_userns` of the `apparmor` package, which `apparmor.service`
//! loads at boot. On a machine whose kernel does not have that profile,
//! AppArmor refuses `unshare` and not `apptainer`, so the request is
//! [`UnprivilegedUserNamespace::Untested`] there.

use std::io;
use std::os::unix::process::CommandExt;
use std::process::{Command, Output, Stdio};

use super::facade::is_apparmor_profile_loaded;

/// The program that makes the request, of the util-linux package.
const UNSHARE: &str = "unshare";

/// The program that `unshare` runs in the namespace: the one that apptainer
/// runs in the namespace of its own request.
const PROGRAM_IN_NAMESPACE: &str = "/bin/true";

/// The exit code of an `unshare` that the kernel refused the namespace to.
/// It ends with another code (126, 127) when the kernel gave the namespace
/// and the program did not run in it.
const UNSHARE_REFUSED_EXIT_CODE: i32 = 1;

/// The AppArmor profile through which the kernel gives a user namespace to a
/// program that has no profile of its own.
const DEFAULT_PROFILE_NAME: &str = "unprivileged_userns";

/// The answer of the kernel to the request of a user namespace for a
/// container build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnprivilegedUserNamespace {
    /// The user of the builds is root: apptainer builds as root with no user
    /// namespace, so no request is made.
    NotRequired,
    /// The kernel gave the namespace.
    Granted,
    /// The kernel refused the namespace, with `answer`. No fix exists for
    /// it.
    Refused { answer: String },
    /// The request gave no answer that applies to `apptainer`, for `reason`.
    Untested { reason: String },
}

impl UnprivilegedUserNamespace {
    /// Makes the request as the user of the container builds.
    /// `apparmor_restricted` tells whether AppArmor restricts user namespaces
    /// on this machine.
    pub(crate) fn for_build_user(apparmor_restricted: bool) -> Self {
        let user = BuildUser::of_this_process();
        if user == BuildUser::Root {
            return Self::NotRequired;
        }
        let answer = KernelAnswer::parse(request_as(user));
        let apparmor_refuses_unshare =
            apparmor_restricted && !is_apparmor_profile_loaded(DEFAULT_PROFILE_NAME);
        Self::from_answer(answer, apparmor_refuses_unshare)
    }

    /// What `answer`, the answer to `unshare`, tells about `apptainer`.
    /// `apparmor_refuses_unshare` tells whether AppArmor refuses a user
    /// namespace to a program that has no profile on this machine: a refusal
    /// then says nothing about `apptainer`, which has one.
    fn from_answer(answer: KernelAnswer, apparmor_refuses_unshare: bool) -> Self {
        match answer {
            KernelAnswer::Granted => Self::Granted,
            KernelAnswer::Unknown(reason) => Self::Untested { reason },
            KernelAnswer::Refused(answer) if apparmor_refuses_unshare => Self::Untested {
                reason: format!(
                    "AppArmor refuses `{UNSHARE}`, which has no profile ({answer}); \
                     Apptainer has the profile of its installation"
                ),
            },
            KernelAnswer::Refused(answer) => Self::Refused { answer },
        }
    }

    /// The explanation of a refusal, or `None` for every other answer.
    pub fn refusal(&self) -> Option<String> {
        let Self::Refused { answer } = self else {
            return None;
        };
        Some(format!(
            "The kernel refuses a user namespace to a user that has no privilege \
             ({answer}). Apptainer needs such a namespace for each container build, and \
             `peppy container setup` has no fix for this cause."
        ))
    }
}

/// Whether the user of the container builds is root, which builds with no
/// user namespace and no fakeroot.
pub(crate) fn build_user_is_root() -> bool {
    BuildUser::of_this_process() == BuildUser::Root
}

/// The id of the user whose daemon builds and runs the containers: the user
/// of this process, or the caller of `sudo` when `sudo` runs it.
pub(crate) fn build_user_id() -> nix::unistd::Uid {
    match BuildUser::of_this_process() {
        BuildUser::Root => nix::unistd::Uid::from_raw(0),
        BuildUser::ThisUser => nix::unistd::geteuid(),
        BuildUser::SudoCaller { uid, .. } => nix::unistd::Uid::from_raw(uid),
    }
}

/// The user whose container builds the request is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildUser {
    /// Root, which builds with no user namespace.
    Root,
    /// The user of this process, which is not root.
    ThisUser,
    /// The user that runs this process as root through `sudo`: the daemon
    /// that builds the containers is the one of that user.
    SudoCaller { uid: u32, gid: u32 },
}

impl BuildUser {
    fn of_this_process() -> Self {
        Self::parse(
            nix::unistd::geteuid().is_root(),
            std::env::var("SUDO_UID").ok().as_deref(),
            std::env::var("SUDO_GID").ok().as_deref(),
        )
    }

    /// The user of the builds for a process that `is_root` or not, with the
    /// values of `SUDO_UID` and `SUDO_GID`, which `sudo` sets to the ids of
    /// its caller.
    fn parse(is_root: bool, sudo_uid: Option<&str>, sudo_gid: Option<&str>) -> Self {
        if !is_root {
            return Self::ThisUser;
        }
        let id = |value: Option<&str>| value?.parse::<u32>().ok();
        match (id(sudo_uid), id(sudo_gid)) {
            (Some(uid), Some(gid)) if uid != 0 => Self::SudoCaller { uid, gid },
            _ => Self::Root,
        }
    }
}

/// Runs `unshare --user /bin/true` as `user`.
fn request_as(user: BuildUser) -> io::Result<Output> {
    let mut unshare = Command::new(UNSHARE);
    unshare
        .args(["--user", PROGRAM_IN_NAMESPACE])
        // The user reads the message of a refusal as `unshare` writes it,
        // and the C locale keeps it the same on every machine.
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    if let BuildUser::SudoCaller { uid, gid } = user {
        unshare.uid(uid).gid(gid);
    }
    unshare.output()
}

/// What the request of `unshare` tells.
#[derive(Debug, PartialEq, Eq)]
enum KernelAnswer {
    /// The kernel gave the namespace and the program ran in it.
    Granted,
    /// The kernel refused the namespace. The message is the last line that
    /// `unshare` wrote, which names the system call and the error of the
    /// kernel.
    Refused(String),
    /// `unshare` did not start, or the program did not run in the namespace:
    /// the message tells why the request has no answer.
    Unknown(String),
}

impl KernelAnswer {
    fn parse(request: io::Result<Output>) -> Self {
        let output = match request {
            Ok(output) => output,
            Err(e) => return Self::Unknown(format!("cannot run `{UNSHARE}`: {e}")),
        };
        if output.status.success() {
            return Self::Granted;
        }
        let stderr = String::from_utf8_lossy(&output.stderr);
        let last_line = stderr.lines().map(str::trim).rfind(|line| !line.is_empty());
        let message = last_line.map_or_else(
            || format!("`{UNSHARE}` ended with {}", output.status),
            str::to_owned,
        );
        if output.status.code() == Some(UNSHARE_REFUSED_EXIT_CODE) {
            Self::Refused(message)
        } else {
            Self::Unknown(message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::process::ExitStatusExt;
    use std::process::ExitStatus;

    const REFUSAL: &str = "unshare: unshare failed: Operation not permitted";

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
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(0, ""))),
            KernelAnswer::Granted
        );
    }

    #[test]
    fn a_refusal_carries_the_last_line_that_unshare_wrote() {
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(
                1,
                "unshare: unshare failed: Permission denied\n"
            ))),
            KernelAnswer::Refused("unshare: unshare failed: Permission denied".to_string())
        );
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(
                1,
                "a line before\n  unshare: unshare failed: Operation not permitted  \n\n"
            ))),
            KernelAnswer::Refused(REFUSAL.to_string())
        );
    }

    #[test]
    fn a_refusal_with_no_message_carries_the_exit_status() {
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(1, " \n"))),
            KernelAnswer::Refused("`unshare` ended with exit status: 1".to_string())
        );
    }

    /// A machine without `unshare` gives no answer: the kernel refused
    /// nothing.
    #[test]
    fn a_machine_without_unshare_gives_no_answer() {
        let no_program = Err(io::Error::new(io::ErrorKind::NotFound, "no such program"));

        assert_eq!(
            KernelAnswer::parse(no_program),
            KernelAnswer::Unknown("cannot run `unshare`: no such program".to_string())
        );
    }

    /// `unshare` ends with 127 or 126 when the kernel gave the namespace and
    /// the program did not run in it, and with no exit code when a signal
    /// ended it: none of them is a refusal of the kernel.
    #[test]
    fn a_program_that_did_not_run_in_the_namespace_gives_no_answer() {
        const NOT_FOUND: &str = "unshare: failed to execute /bin/true: No such file or directory";
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(127, &format!("{NOT_FOUND}\n")))),
            KernelAnswer::Unknown(NOT_FOUND.to_string())
        );
        assert_eq!(
            KernelAnswer::parse(Ok(unshare_output(126, ""))),
            KernelAnswer::Unknown("`unshare` ended with exit status: 126".to_string())
        );

        let killed = Output {
            // A wait status that holds a signal number alone: SIGKILL.
            status: ExitStatus::from_raw(9),
            stdout: Vec::new(),
            stderr: Vec::new(),
        };
        assert_eq!(
            KernelAnswer::parse(Ok(killed)),
            KernelAnswer::Unknown("`unshare` ended with signal: 9 (SIGKILL)".to_string())
        );
    }

    #[test]
    fn a_grant_to_unshare_is_a_grant_to_apptainer() {
        for apparmor_refuses_unshare in [false, true] {
            assert_eq!(
                UnprivilegedUserNamespace::from_answer(
                    KernelAnswer::Granted,
                    apparmor_refuses_unshare
                ),
                UnprivilegedUserNamespace::Granted
            );
        }
    }

    #[test]
    fn a_request_with_no_answer_is_untested() {
        for apparmor_refuses_unshare in [false, true] {
            assert_eq!(
                UnprivilegedUserNamespace::from_answer(
                    KernelAnswer::Unknown("cannot run `unshare`: no such program".to_string()),
                    apparmor_refuses_unshare
                ),
                UnprivilegedUserNamespace::Untested {
                    reason: "cannot run `unshare`: no such program".to_string()
                }
            );
        }
    }

    /// A refusal is the answer for `apptainer` too, but on a machine where
    /// AppArmor refuses `unshare` for want of a profile that `apptainer` has.
    #[test]
    fn a_refusal_applies_to_apptainer_unless_apparmor_refuses_unshare() {
        assert_eq!(
            UnprivilegedUserNamespace::from_answer(
                KernelAnswer::Refused(REFUSAL.to_string()),
                false
            ),
            UnprivilegedUserNamespace::Refused {
                answer: REFUSAL.to_string()
            }
        );
        assert_eq!(
            UnprivilegedUserNamespace::from_answer(
                KernelAnswer::Refused("unshare: unshare failed: Permission denied".to_string()),
                true
            ),
            UnprivilegedUserNamespace::Untested {
                reason: "AppArmor refuses `unshare`, which has no profile \
                         (unshare: unshare failed: Permission denied); \
                         Apptainer has the profile of its installation"
                    .to_string()
            }
        );
    }

    #[test]
    fn a_refusal_alone_explains_itself() {
        let refused = UnprivilegedUserNamespace::Refused {
            answer: REFUSAL.to_string(),
        };
        assert_eq!(
            refused.refusal().as_deref(),
            Some(
                "The kernel refuses a user namespace to a user that has no privilege \
                 (unshare: unshare failed: Operation not permitted). Apptainer needs such \
                 a namespace for each container build, and `peppy container setup` has no \
                 fix for this cause."
            )
        );

        for answer in [
            UnprivilegedUserNamespace::Granted,
            UnprivilegedUserNamespace::NotRequired,
            UnprivilegedUserNamespace::Untested {
                reason: "cannot run `unshare`: no such program".to_string(),
            },
        ] {
            assert_eq!(answer.refusal(), None, "{answer:?}");
        }
    }

    /// The request is for the user of this process, but for a process that
    /// `sudo` runs as root: it is then for the caller of `sudo`.
    #[test]
    fn the_build_user_of_a_root_process_is_the_caller_of_sudo() {
        // (is_root, SUDO_UID, SUDO_GID, user)
        let cases = [
            (false, None, None, BuildUser::ThisUser),
            // A user that is not root keeps the variables of an outer `sudo`.
            (false, Some("1000"), Some("1000"), BuildUser::ThisUser),
            (true, None, None, BuildUser::Root),
            (
                true,
                Some("1000"),
                Some("1001"),
                BuildUser::SudoCaller {
                    uid: 1000,
                    gid: 1001,
                },
            ),
            // Root is the caller of `sudo`.
            (true, Some("0"), Some("0"), BuildUser::Root),
            // Values that are not the two ids of a caller.
            (true, Some("1000"), None, BuildUser::Root),
            (true, None, Some("1000"), BuildUser::Root),
            (true, Some("someone"), Some("1000"), BuildUser::Root),
            (true, Some("1000"), Some(""), BuildUser::Root),
        ];
        for (is_root, sudo_uid, sudo_gid, user) in cases {
            assert_eq!(
                BuildUser::parse(is_root, sudo_uid, sudo_gid),
                user,
                "is_root={is_root} SUDO_UID={sudo_uid:?} SUDO_GID={sudo_gid:?}"
            );
        }
    }

    /// The request of this host ends with an answer of the right kind: none
    /// for root, and for every other user a grant, a refusal that carries a
    /// message, or a request that could not be made. With no AppArmor
    /// restriction given, no answer names AppArmor as its cause.
    #[test]
    fn the_request_of_this_host_ends_with_an_answer() {
        let is_root = BuildUser::of_this_process() == BuildUser::Root;

        match UnprivilegedUserNamespace::for_build_user(false) {
            UnprivilegedUserNamespace::NotRequired => assert!(is_root),
            UnprivilegedUserNamespace::Granted => assert!(!is_root),
            UnprivilegedUserNamespace::Refused { answer } => {
                assert!(!is_root);
                assert!(!answer.is_empty());
            }
            UnprivilegedUserNamespace::Untested { reason } => {
                assert!(!is_root);
                assert!(
                    !reason.contains("AppArmor"),
                    "no AppArmor restriction was given, yet it is the cause: {reason}"
                );
            }
        }
    }
}
