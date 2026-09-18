//! Running external commands from build scripts.

use std::io::{BufRead, Read};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Runs a command and prints a cargo warning on failure. Returns `true` on success.
pub fn run_command(command: &mut Command, description: &str) -> bool {
    match command.status() {
        Ok(status) if status.success() => true,
        Ok(status) => {
            println!("cargo:warning=Failed to {description} (exit status: {status})");
            false
        }
        Err(err) => {
            println!("cargo:warning=Failed to {description}: {err}");
            false
        }
    }
}

/// Output from a streamed command execution.
pub struct CommandOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Runs a command, streaming its stdout and stderr as `cargo:warning=` lines.
///
/// Each output line is forwarded as `cargo:warning=[{label}] {line}` so the
/// user sees real-time progress during long-running build script operations.
/// The full captured stdout and stderr are returned for post-hoc error reporting.
///
/// Any stdout/stderr configuration already set on `command` is overridden
/// with `Stdio::piped()`; stdin is left inherited — pass
/// `.stdin(Stdio::null())` for tools that might prompt for input.
pub fn run_command_streaming(command: &mut Command, label: &str) -> CommandOutput {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            println!("cargo:warning=[{label}] Failed to spawn: {e}");
            return CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };

    // Read stdout and stderr on separate threads to avoid deadlocks.
    // If both are read sequentially, a child that fills one pipe buffer
    // while we're blocked reading the other will hang indefinitely.
    fn stream_pipe(
        pipe: impl Read + Send + 'static,
        label: String,
    ) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            // Drain the pipe byte-by-line so non-UTF-8 output can never stall
            // the reader (and thus block the child on a full pipe). Bytes are
            // converted lossily for logging and capture.
            let mut reader = std::io::BufReader::new(pipe);
            let mut captured = String::new();
            let mut buf = Vec::new();
            while let Ok(n) = reader.read_until(b'\n', &mut buf) {
                if n == 0 {
                    break;
                }
                // Trim a trailing newline (and CR) like BufRead::lines did; the
                // captured copy keeps a normalized '\n' terminator.
                let mut bytes = &buf[..];
                if bytes.last() == Some(&b'\n') {
                    bytes = &bytes[..bytes.len() - 1];
                    if bytes.last() == Some(&b'\r') {
                        bytes = &bytes[..bytes.len() - 1];
                    }
                }
                let line = String::from_utf8_lossy(bytes);
                println!("cargo:warning=[{}] {}", label, line);
                captured.push_str(&line);
                captured.push('\n');
                buf.clear();
            }
            captured
        })
    }

    let stderr_thread = stream_pipe(child.stderr.take().unwrap(), label.to_string());
    let stdout_thread = stream_pipe(child.stdout.take().unwrap(), label.to_string());

    let stdout_captured = stdout_thread.join().unwrap_or_default();
    let stderr_captured = stderr_thread.join().unwrap_or_default();
    let status = match child.wait() {
        Ok(status) => status,
        Err(e) => {
            println!("cargo:warning=[{label}] Failed to wait for child process: {e}");
            return CommandOutput {
                success: false,
                stdout: stdout_captured,
                stderr: stderr_captured,
            };
        }
    };

    if !status.success() {
        println!("cargo:warning=[{label}] Command failed with exit status: {status}");
    }

    CommandOutput {
        success: status.success(),
        stdout: stdout_captured,
        stderr: stderr_captured,
    }
}

/// Runs a command with a timeout, capturing stdout/stderr without forwarding it
/// live. If the command does not finish within `timeout`, the child is killed
/// and `success` is `false`.
///
/// Intended for short probes (for example, checking whether a VM guest is
/// reachable over SSH) that must never hang a build. Both pipes are drained on
/// background threads so the child can never block on a full pipe buffer while
/// we poll for exit.
///
/// Any stdout/stderr configuration already set on `command` is overridden
/// with `Stdio::piped()`; stdin is left inherited (a stdin-blocked child is
/// still killed at the deadline).
pub fn run_command_with_timeout(command: &mut Command, timeout: Duration) -> CommandOutput {
    command.stdout(Stdio::piped()).stderr(Stdio::piped());

    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CommandOutput {
                success: false,
                stdout: String::new(),
                stderr: e.to_string(),
            };
        }
    };

    fn drain(mut pipe: impl Read + Send + 'static) -> std::thread::JoinHandle<String> {
        std::thread::spawn(move || {
            // Read raw bytes (not read_to_string) so non-UTF-8 output is still
            // fully drained and captured lossily instead of being discarded.
            let mut bytes = Vec::new();
            pipe.read_to_end(&mut bytes).ok();
            String::from_utf8_lossy(&bytes).into_owned()
        })
    }
    let stdout_thread = drain(child.stdout.take().unwrap());
    let stderr_thread = drain(child.stderr.take().unwrap());

    let deadline = Instant::now() + timeout;
    let exit_status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) if Instant::now() >= deadline => {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => {
                child.kill().ok();
                child.wait().ok();
                break None;
            }
        }
    };

    // Killing the child closes its pipes, so the drain threads reach EOF.
    let stdout = stdout_thread.join().unwrap_or_default();
    let stderr = stderr_thread.join().unwrap_or_default();

    match exit_status {
        Some(status) => CommandOutput {
            success: status.success(),
            stdout,
            stderr,
        },
        None => {
            let timed_out = if stderr.trim().is_empty() {
                format!("command timed out after {timeout:?}")
            } else {
                format!("command timed out after {timeout:?}: {stderr}")
            };
            CommandOutput {
                success: false,
                stdout,
                stderr: timed_out,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An absolute path that is guaranteed not to exist, for spawn-failure tests.
    /// Absolute so the result does not depend on `PATH` contents.
    fn missing_binary(dir: &tempfile::TempDir) -> std::path::PathBuf {
        dir.path().join("no-such-bin")
    }

    #[test]
    fn run_command_reports_success() {
        assert!(run_command(&mut Command::new("true"), "run true"));
    }

    #[test]
    fn run_command_reports_failure() {
        assert!(!run_command(&mut Command::new("false"), "run false"));
    }

    #[test]
    fn run_command_reports_spawn_error() {
        let dir = tempfile::tempdir().expect("temp dir");
        assert!(!run_command(
            &mut Command::new(missing_binary(&dir)),
            "run missing binary"
        ));
    }

    #[test]
    fn streaming_captures_stdout() {
        let output = run_command_streaming(Command::new("echo").arg("hello world"), "test-echo");
        assert!(output.success);
        assert!(output.stdout.contains("hello world"));
    }

    #[test]
    fn streaming_captures_stderr() {
        let output = run_command_streaming(
            Command::new("bash").args(["-c", "echo error-output >&2"]),
            "test-stderr",
        );
        assert!(output.success);
        assert!(output.stderr.contains("error-output"));
    }

    #[test]
    fn streaming_reports_failure() {
        let output = run_command_streaming(&mut Command::new("false"), "test-fail");
        assert!(!output.success);
    }

    #[test]
    fn streaming_reports_spawn_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = run_command_streaming(&mut Command::new(missing_binary(&dir)), "test-spawn");
        assert!(!output.success);
        assert!(!output.stderr.is_empty());
    }

    #[test]
    fn streaming_handles_mixed_output() {
        let output = run_command_streaming(
            Command::new("bash").args(["-c", "echo out-line; echo err-line >&2"]),
            "test-mixed",
        );
        assert!(output.success);
        assert!(output.stdout.contains("out-line"));
        assert!(output.stderr.contains("err-line"));
    }

    #[test]
    fn timeout_runner_succeeds_for_fast_command() {
        let output = run_command_with_timeout(&mut Command::new("true"), Duration::from_secs(5));
        assert!(output.success);
    }

    #[test]
    fn timeout_runner_reports_command_failure() {
        let output = run_command_with_timeout(&mut Command::new("false"), Duration::from_secs(5));
        assert!(!output.success);
    }

    #[test]
    fn timeout_runner_captures_stdout() {
        let output =
            run_command_with_timeout(Command::new("echo").arg("probe-ok"), Duration::from_secs(5));
        assert!(output.success);
        assert!(output.stdout.contains("probe-ok"));
    }

    #[test]
    fn timeout_runner_kills_command_that_exceeds_timeout() {
        // The "timed out" message is emitted only on the kill branch, so it
        // deterministically proves the child was killed rather than waited
        // out. The 10s sleep bounds the damage if that branch regresses.
        let output =
            run_command_with_timeout(Command::new("sleep").arg("10"), Duration::from_millis(200));
        assert!(!output.success);
        assert!(output.stderr.contains("timed out"));
    }

    #[test]
    fn timeout_runner_reports_spawn_failure() {
        let dir = tempfile::tempdir().expect("temp dir");
        let output = run_command_with_timeout(
            &mut Command::new(missing_binary(&dir)),
            Duration::from_secs(5),
        );
        assert!(!output.success);
        assert!(!output.stderr.is_empty());
    }
}
