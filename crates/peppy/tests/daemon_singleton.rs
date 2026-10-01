use crate::common::{
    DAEMON_BOOT, DaemonGuard, MessagingEngine, spawn_daemon, wait_for_daemon, wait_for_exit,
};
use daemon::DaemonError;
use daemon::state::DaemonState;
use peppy::test_support::wait_for_log;
use std::path::Path;
use std::time::Duration;

/// Spawns a daemon in `home` and blocks until it has written its own pid to
/// the state file; panics with the daemon's logs if it exits or times out
/// first.
fn spawn_ready_daemon(home: &Path) -> (DaemonGuard, u32) {
    let (mut daemon, logs) = spawn_daemon(home, MessagingEngine::Mock);
    let pid = daemon.0.id();
    let state_file = DaemonState::state_file_in(home);
    wait_for_daemon(
        &mut daemon,
        &logs,
        DAEMON_BOOT,
        "the daemon's state file",
        |_| {
            DaemonState::read_from(&state_file)
                .ok()
                .filter(|state| state.daemon_pid == Some(pid))
                .map(|_| ())
        },
    );
    (daemon, pid)
}

#[test]
fn second_serve_on_the_same_home_refuses_to_boot() {
    let home = tempfile::tempdir().expect("temp home");
    let (_first, first_pid) = spawn_ready_daemon(home.path());

    let (mut second, second_logs) = spawn_daemon(home.path(), MessagingEngine::Mock);
    let status = wait_for_exit(&mut second.0, Duration::from_secs(30));
    wait_for_log(
        || second_logs.lock().unwrap().clone(),
        &DaemonError::AlreadyRunning.to_string(),
        Duration::from_secs(5),
    );
    assert_eq!(
        status.code(),
        Some(1),
        "second daemon should exit 1, got {status:?}. Logs:\n{}",
        second_logs.lock().unwrap()
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
    let (first, _) = spawn_ready_daemon(home.path());
    drop(first);

    let (_fresh, fresh_pid) = spawn_ready_daemon(home.path());

    let state = DaemonState::read_from(&DaemonState::state_file_in(home.path()))
        .expect("fresh daemon state should be readable");
    assert_eq!(state.daemon_pid, Some(fresh_pid));
}
