//! Running external commands from build scripts.

use std::io::{BufRead, Read};
use std::process::{Command, Stdio};

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
            let mut captured = String::new();
            for line in std::io::BufReader::new(pipe).lines().map_while(Result::ok) {
                println!("cargo:warning=[{}] {}", label, line);
                captured.push_str(&line);
                captured.push('\n');
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

#[cfg(test)]
mod tests {
    use super::*;

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
    fn streaming_handles_mixed_output() {
        let output = run_command_streaming(
            Command::new("bash").args(["-c", "echo out-line; echo err-line >&2"]),
            "test-mixed",
        );
        assert!(output.success);
        assert!(output.stdout.contains("out-line"));
        assert!(output.stderr.contains("err-line"));
    }
}
