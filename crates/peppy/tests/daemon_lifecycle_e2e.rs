//! Real-binary end-to-end coverage for daemon shutdown signals.
//!
//! Spawns the actual `peppy service serve` binary as a separate OS process and
//! verifies it shuts down cleanly on both SIGINT (ctrl+C) and SIGTERM (systemd
//! stop). The SIGTERM case is the one that regressed before this work: serve
//! only listened for SIGINT, so `systemctl stop` would not run the node
//! teardown at all.
//!
//! These are `#[ignore]`d: they launch a real daemon (and on Linux run the
//! Apptainer preflight), so they are gated behind an explicit
//! `cargo test -- --ignored` run rather than the default suite. The daemon-side
//! teardown of node *processes* is covered without a separate binary by
//! `core-node`'s `teardown_all_instances` test, and the watchdog timing by
//! `peppylib`'s `daemon_watchdog` tests; this file proves the signal wiring in
//! the shipped binary.

use peppy::test_support::wait_for_log;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// SIGKILLs the daemon on drop so a failing/panicking test never leaks it.
struct DaemonGuard(Child);
impl Drop for DaemonGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawn `peppy service serve --messaging-engine mock` in an isolated home
/// (`TMPDIR` → the debug-build `PeppyDirs`), capturing stdout (where the serve
/// daemon's tracing logs go) so we can wait for the readiness line.
fn spawn_daemon(home: &std::path::Path) -> (DaemonGuard, Arc<Mutex<String>>) {
    let state_file = home.join("daemon_state.json5");
    let mut child = Command::new(env!("CARGO_BIN_EXE_peppy"))
        .args(["service", "serve", "--messaging-engine", "mock"])
        // Pin the child's data root to this per-test home explicitly, so it stays
        // isolated even when the CI job exports its own per-run PEPPY_HOME.
        .env(config::consts::PEPPY_HOME_ENV, home)
        .env("TMPDIR", home)
        .env("PEPPY_DAEMON_STATE_FILE", &state_file)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn peppy service serve");

    // Drain both streams into a shared buffer so we can wait for readiness and
    // the child never blocks on a full pipe. The serve daemon logs to stdout.
    let logs = Arc::new(Mutex::new(String::new()));
    for stream in [
        Box::new(child.stdout.take().expect("piped stdout")) as Box<dyn std::io::Read + Send>,
        Box::new(child.stderr.take().expect("piped stderr")) as Box<dyn std::io::Read + Send>,
    ] {
        let logs_writer = Arc::clone(&logs);
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stream);
            let mut line = String::new();
            while reader.read_line(&mut line).unwrap_or(0) > 0 {
                logs_writer.lock().unwrap().push_str(&line);
                line.clear();
            }
        });
    }

    (DaemonGuard(child), logs)
}

/// Wait for the child to exit, returning its status, or panic after `timeout`.
fn wait_for_exit(child: &mut Child, timeout: Duration) -> std::process::ExitStatus {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if let Some(status) = child.try_wait().expect("try_wait") {
            return status;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    panic!("daemon did not exit within {timeout:?} after the shutdown signal");
}

fn run_shutdown_signal_case(signal: libc::c_int) {
    let home = tempfile::tempdir().expect("temp home");
    let (mut guard, logs) = spawn_daemon(home.path());

    // Wait until the serve loop is fully up before signaling.
    wait_for_log(
        || logs.lock().unwrap().clone(),
        "Serve command initialized!",
        Duration::from_secs(60),
    );

    // SAFETY: kill(2) with a real signal to our own child; no memory effects.
    let pid = guard.0.id() as libc::pid_t;
    let rc = unsafe { libc::kill(pid, signal) };
    assert_eq!(rc, 0, "kill({pid}, {signal}) failed");

    let status = wait_for_exit(&mut guard.0, Duration::from_secs(30));
    assert!(
        status.success(),
        "daemon should exit cleanly after signal {signal}; got {status:?}. Logs:\n{}",
        logs.lock().unwrap()
    );
}

#[test]
#[ignore = "spawns a real peppy daemon; run with --ignored"]
fn serve_shuts_down_on_sigint() {
    run_shutdown_signal_case(libc::SIGINT);
}

#[test]
#[ignore = "spawns a real peppy daemon; run with --ignored"]
fn serve_shuts_down_on_sigterm() {
    run_shutdown_signal_case(libc::SIGTERM);
}
