//! The peppy commands that `peppy update` runs in child processes.
//!
//! The update runs each command of a peppy binary in a child process: the
//! `--version` check of the downloaded binary, the service commands of the
//! binary that runs and of the new one, and the set-up commands of the new
//! one. The service commands set environment variables for `systemctl
//! --user` (`ensure_systemd_user_env`), which is sound only in a process with
//! one thread, and the update process has more threads (zenoh, the progress
//! bars, the download).

use std::fmt;
use std::io;
use std::path::Path;
use std::process::{Command, Output, Stdio};

/// Where the input and output of a child peppy command go.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChildIo {
    /// Stdin is closed and the output is captured, for a command whose
    /// output the update reads or shows only when the command fails.
    Captured,
    /// The child shares the terminal of the update, for a command that
    /// writes its progress or asks a question.
    Terminal,
}

/// Runs a command of a peppy binary in a child process. The tests give a
/// runner that records each command and runs nothing.
pub(super) trait PeppyRunner {
    /// Runs `peppy args`. A [`ChildIo::Terminal`] command gives an empty
    /// stdout and stderr.
    fn run(&mut self, peppy: &Path, args: &[&str], io: ChildIo) -> io::Result<Output>;
}

/// The runner of `peppy update`, which starts real child processes that
/// inherit its environment.
pub(super) struct HostPeppyRunner;

impl PeppyRunner for HostPeppyRunner {
    fn run(&mut self, peppy: &Path, args: &[&str], io: ChildIo) -> io::Result<Output> {
        let mut command = Command::new(peppy);
        command.args(args);
        match io {
            ChildIo::Captured => command.stdin(Stdio::null()).output(),
            ChildIo::Terminal => command.status().map(|status| Output {
                status,
                stdout: Vec::new(),
                stderr: Vec::new(),
            }),
        }
    }
}

/// A peppy command of the update that did not succeed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FailedCommand {
    /// The arguments of the command, for example `container setup`.
    pub(super) args: String,
    /// Why it failed: the spawn error, or the exit status and the stderr.
    pub(super) reason: String,
}

impl fmt::Display for FailedCommand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "`peppy {}`: {}", self.args, self.reason)
    }
}

/// Runs `peppy args` and gives its output when it exits with success.
pub(super) fn run_peppy(
    runner: &mut dyn PeppyRunner,
    peppy: &Path,
    args: &[&str],
    io: ChildIo,
) -> Result<Output, FailedCommand> {
    let failed = |reason: String| FailedCommand {
        args: args.join(" "),
        reason,
    };
    let output = runner
        .run(peppy, args, io)
        .map_err(|e| failed(format!("it could not start: {e}")))?;
    if output.status.success() {
        return Ok(output);
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if stderr.is_empty() {
        return Err(failed(format!("it ended with {}", output.status)));
    }
    Err(failed(format!("it ended with {}: {stderr}", output.status)))
}

#[cfg(test)]
pub(super) mod test_support {
    use std::collections::BTreeMap;
    use std::os::unix::process::ExitStatusExt;
    use std::path::PathBuf;
    use std::process::ExitStatus;

    use super::*;

    /// A runner that records each command and runs nothing. A command
    /// succeeds unless the test gives it an exit code; `--version` writes
    /// `version_output`.
    pub(in super::super) struct ScriptedRunner {
        pub(in super::super) calls: Vec<(PathBuf, String)>,
        exit_codes: BTreeMap<String, i32>,
        version_output: String,
    }

    impl ScriptedRunner {
        pub(in super::super) fn reporting(version_output: &str) -> Self {
            Self {
                calls: Vec::new(),
                exit_codes: BTreeMap::new(),
                version_output: version_output.to_owned(),
            }
        }

        /// Makes `peppy args` end with `exit_code` and write `args broke` to
        /// stderr.
        pub(in super::super) fn failing(mut self, args: &str, exit_code: i32) -> Self {
            self.exit_codes.insert(args.to_owned(), exit_code);
            self
        }

        /// The arguments of each command, in order.
        pub(in super::super) fn commands(&self) -> Vec<&str> {
            self.calls.iter().map(|(_, args)| args.as_str()).collect()
        }
    }

    impl PeppyRunner for ScriptedRunner {
        fn run(&mut self, peppy: &Path, args: &[&str], _io: ChildIo) -> io::Result<Output> {
            let args = args.join(" ");
            self.calls.push((peppy.to_path_buf(), args.clone()));
            let exit_code = self.exit_codes.get(&args).copied().unwrap_or(0);
            let stdout = if args == "--version" {
                self.version_output.clone().into_bytes()
            } else {
                Vec::new()
            };
            let stderr = if exit_code == 0 {
                Vec::new()
            } else {
                format!("{args} broke").into_bytes()
            };
            Ok(Output {
                status: ExitStatus::from_raw(exit_code << 8),
                stdout,
                stderr,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `/bin/sh` is not a file that the tests write, so its exec cannot meet
    /// `ETXTBSY` from a fork of another test.
    #[test]
    fn host_runner_captures_the_output_and_the_exit_status() {
        let mut runner = HostPeppyRunner;

        let output = run_peppy(
            &mut runner,
            Path::new("/bin/sh"),
            &["-c", "echo peppy v0.40.0"],
            ChildIo::Captured,
        )
        .expect("the command succeeds");
        assert_eq!(String::from_utf8_lossy(&output.stdout), "peppy v0.40.0\n");

        let failure = run_peppy(
            &mut runner,
            Path::new("/bin/sh"),
            &["-c", "echo glibc mismatch >&2; exit 3"],
            ChildIo::Captured,
        )
        .expect_err("the command fails");
        assert_eq!(failure.args, "-c echo glibc mismatch >&2; exit 3");
        assert!(failure.reason.contains("exit status: 3"), "{failure}");
        assert!(failure.reason.contains("glibc mismatch"), "{failure}");
    }

    #[test]
    fn host_runner_reports_a_program_that_cannot_start() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("absent-peppy");

        let failure = run_peppy(
            &mut HostPeppyRunner,
            &missing,
            &["--version"],
            ChildIo::Captured,
        )
        .expect_err("a missing program fails");

        assert_eq!(failure.args, "--version");
        assert!(failure.reason.contains("could not start"), "{failure}");
    }
}
