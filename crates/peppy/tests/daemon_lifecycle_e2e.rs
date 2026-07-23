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

use crate::common::{spawn_daemon, wait_for_exit};
use peppy::test_support::wait_for_log;
use std::time::Duration;

fn run_shutdown_signal_case(signal: rustix::process::Signal) {
    let home = tempfile::tempdir().expect("temp home");
    let (mut guard, logs) = spawn_daemon(home.path());

    // Wait until the serve loop is fully up before signaling.
    wait_for_log(
        || logs.lock().unwrap().clone(),
        "Serve command initialized!",
        Duration::from_secs(60),
    );

    let pid = rustix::process::Pid::from_child(&guard.0);
    rustix::process::kill_process(pid, signal)
        .unwrap_or_else(|e| panic!("kill({pid:?}, {signal:?}) failed: {e}"));

    let status = wait_for_exit(&mut guard.0, Duration::from_secs(30));
    assert!(
        status.success(),
        "daemon should exit cleanly after signal {signal:?}; got {status:?}. Logs:\n{}",
        logs.lock().unwrap()
    );
}

#[test]
#[ignore = "spawns a real peppy daemon; run with --ignored"]
fn serve_shuts_down_on_sigint() {
    run_shutdown_signal_case(rustix::process::Signal::INT);
}

#[test]
#[ignore = "spawns a real peppy daemon; run with --ignored"]
fn serve_shuts_down_on_sigterm() {
    run_shutdown_signal_case(rustix::process::Signal::TERM);
}
