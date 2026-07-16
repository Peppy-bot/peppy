use crate::common::{DaemonGuard, spawn_daemon, wait_for_exit};
use daemon::state::DaemonState;
use peppy::test_support::wait_for_log;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn wait_for_daemon_state_pid(
    home: &Path,
    daemon: &mut DaemonGuard,
    logs: &Arc<Mutex<String>>,
    pid: u32,
) {
    let state_file = DaemonState::state_file_in(home);
    let timeout = Duration::from_secs(60);
    let start = Instant::now();

    while start.elapsed() < timeout {
        if let Some(status) = daemon.0.try_wait().expect("try_wait daemon") {
            panic!(
                "daemon {pid} exited with {status:?} before writing its state. Logs:\n{}",
                logs.lock().unwrap()
            );
        }

        if DaemonState::read_from(&state_file).is_ok_and(|state| state.daemon_pid == Some(pid)) {
            return;
        }

        std::thread::sleep(Duration::from_millis(25));
    }

    panic!(
        "daemon {pid} did not write its state within {timeout:?}. Logs:\n{}",
        logs.lock().unwrap()
    );
}

#[test]
fn second_serve_on_the_same_home_refuses_to_boot() {
    let home = tempfile::tempdir().expect("temp home");
    let (mut first, first_logs) = spawn_daemon(home.path());
    let first_pid = first.0.id();
    wait_for_daemon_state_pid(home.path(), &mut first, &first_logs, first_pid);

    let (mut second, second_logs) = spawn_daemon(home.path());
    let status = wait_for_exit(&mut second.0, Duration::from_secs(30));
    wait_for_log(
        || second_logs.lock().unwrap().clone(),
        "a peppy daemon is already running on this machine",
        Duration::from_secs(5),
    );
    let output = second_logs.lock().unwrap().clone();
    assert_eq!(
        status.code(),
        Some(1),
        "second daemon should exit 1, got {status:?}. Logs:\n{output}"
    );
    assert!(
        output.contains("a peppy daemon is already running on this machine"),
        "second daemon did not report the singleton conflict. Logs:\n{output}"
    );

    let state = DaemonState::read_from(&DaemonState::state_file_in(home.path()))
        .expect("first daemon state should remain readable");
    assert_eq!(
        state.daemon_pid,
        Some(first_pid),
        "refused daemon must not overwrite the running daemon's state"
    );
}

#[test]
fn a_sigkilled_daemon_releases_the_lock_for_a_fresh_boot() {
    let home = tempfile::tempdir().expect("temp home");
    let (mut first, first_logs) = spawn_daemon(home.path());
    let first_pid = first.0.id();
    wait_for_daemon_state_pid(home.path(), &mut first, &first_logs, first_pid);
    drop(first);

    let (mut fresh, fresh_logs) = spawn_daemon(home.path());
    let fresh_pid = fresh.0.id();
    wait_for_daemon_state_pid(home.path(), &mut fresh, &fresh_logs, fresh_pid);

    let state = DaemonState::read_from(&DaemonState::state_file_in(home.path()))
        .expect("fresh daemon state should be readable");
    assert_eq!(state.daemon_pid, Some(fresh_pid));
}
