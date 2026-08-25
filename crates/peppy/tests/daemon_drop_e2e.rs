//! In-process end-to-end: a daemon that goes away without a teardown takes
//! the instances it spawned with it.
//!
//! `ServeCommandEmulation` hosts the daemon on the test's tokio runtime.
//! Dropping that runtime drops the core node, and with it the last handle to
//! its node stack: what happens whenever a test ends (or panics) without a
//! `stack reset`, or a serve task is aborted. The node's keep-alive process
//! cannot exit on its own here (its sentinel stays in place and its owner,
//! this test binary, stays alive), so the only way it can be gone afterwards
//! is the stack's SIGKILL on drop.

use super::common::{
    TEST_NODE_TAG, emulate_startup_services, node_add_command, node_run_command,
    read_daemon_git_hash, setup, write_node_config_for_helper,
};
use peppy::commands::Command;
use peppy::test_support::{InstanceLifetime, LogCapture};
use peppylib::MessengerHandle;
use rustix::process::{Pid, Signal, WaitOptions, WaitStatus, test_kill_process, waitpid};
use std::sync::mpsc;
use std::time::Duration;

/// Hang guard only. The SIGKILL is sent synchronously while the runtime is
/// dropped, before the reap begins, so a passing run never depends on this
/// bound; a missing kill turns into a failed assertion instead of a hung test.
const REAP_BOUND: Duration = Duration::from_secs(60);

/// The pid the daemon reported for `instance_id`, read from the CLI's
/// `Started node instance '<id>' (pid: <n>)` line, which `node run` logs
/// before it returns.
fn started_pid(logs: &str, instance_id: &str) -> Pid {
    let marker = format!("Started node instance '{instance_id}' (pid: ");
    let raw = logs
        .split(&marker)
        .nth(1)
        .and_then(|rest| rest.split(')').next())
        .unwrap_or_else(|| panic!("`node run` should log the started pid. Logs:\n{logs}"));
    let pid: i32 = raw.parse().expect("the logged pid is a number");
    Pid::from_raw(pid).expect("the logged pid is positive")
}

/// Reaps `pid`, this process's child, and returns its exit status; `None`
/// when it was already reaped (another runtime's orphan reaper can get to it
/// first), which still means it exited.
fn reap_within(pid: Pid, bound: Duration) -> Option<WaitStatus> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(waitpid(Some(pid), WaitOptions::empty()));
    });
    match rx.recv_timeout(bound) {
        Ok(Ok(reaped)) => Some(reaped.expect("a blocking waitpid reports a status").1),
        Ok(Err(rustix::io::Errno::CHILD)) => None,
        Ok(Err(error)) => panic!("waitpid({pid:?}) failed: {error}"),
        Err(_) => panic!(
            "instance {pid:?} is still alive {bound:?} after the daemon that spawned it was \
             dropped: the node stack did not kill it"
        ),
    }
}

#[test]
fn dropping_the_daemon_kills_the_instances_it_spawned() {
    let (rt, serve, ctx, work_dir) = setup();
    let core_node_name = serve.core_node_name().to_string();
    let node_name = "kept_alive_node";
    let instance_id = "kept_alive_inst";

    // Alive for as long as this guard exists: the process cannot end on its own
    // during the test.
    let instances = InstanceLifetime::new();
    let git_hash = read_daemon_git_hash(serve.daemon_state_path());
    let node_dir = write_node_config_for_helper(
        work_dir.path(),
        node_name,
        TEST_NODE_TAG,
        &git_hash,
        &instances.keep_alive_argv(),
        None,
        None,
        None,
    );
    node_add_command(&node_dir)
        .execute(&ctx)
        .expect("node add should succeed");

    // The keep-alive shell answers no ready/health probe; the test does, from
    // the daemon's runtime, so `node run` completes.
    let messenger = MessengerHandle::from_shared(serve.messenger());
    rt.block_on(emulate_startup_services(
        &messenger,
        &core_node_name,
        node_name,
        instance_id,
    ));

    let log_capture = LogCapture::new();
    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .without_time()
        .with_writer(log_capture.clone())
        .finish();
    let _guard = tracing::subscriber::set_default(subscriber);
    node_run_command(instance_id, node_name, Vec::new(), Vec::new())
        .execute(&ctx)
        .expect("node run should succeed");
    let pid = started_pid(&log_capture.logs(), instance_id);
    test_kill_process(pid).expect("the instance should be alive after `node run`");

    // The daemon goes away without a `stack reset` or a shutdown signal: the
    // runtime drops the core node's task, and with it the node stack.
    drop(rt);

    if let Some(status) = reap_within(pid, REAP_BOUND) {
        assert_eq!(
            status.terminating_signal(),
            Some(Signal::KILL.as_raw()),
            "the instance should have been SIGKILLed by the dropped stack; got {status:?}"
        );
    }

    drop(serve);
    drop(ctx);
    drop(instances);
    drop(work_dir);
}
