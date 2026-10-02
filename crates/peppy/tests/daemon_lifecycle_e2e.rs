//! Real-binary end-to-end coverage of how the daemon process ends.
//!
//! Spawns the actual `peppy service serve` binary as a separate OS process. The
//! signal cases verify it shuts down cleanly on both SIGINT (ctrl+C) and
//! SIGTERM (systemd stop); they run on the mock engine, which needs no router,
//! and take a fraction of a second. The zenohd case (Linux) boots the daemon on
//! zenoh, SIGKILLs it, and verifies the kernel ends its zenohd and the next
//! daemon on the same port boots.
//!
//! These run in the default suite.
//!
//! The daemon-side teardown of node *processes* is covered without a separate
//! binary by `core-node`'s `teardown_all_instances` test, and the watchdog
//! timing by `peppylib`'s `daemon_watchdog` tests.

use crate::common::{MessagingEngine, spawn_daemon, wait_for_exit};
use peppy::test_support::wait_for_log;
use std::time::Duration;

fn run_shutdown_signal_case(signal: rustix::process::Signal) {
    let home = tempfile::tempdir().expect("temp home");
    let (mut guard, logs) = spawn_daemon(home.path(), MessagingEngine::Mock);

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
fn serve_shuts_down_on_sigint() {
    run_shutdown_signal_case(rustix::process::Signal::INT);
}

#[test]
fn serve_shuts_down_on_sigterm() {
    run_shutdown_signal_case(rustix::process::Signal::TERM);
}

/// On Linux the kernel ends a daemon's zenohd when the daemon dies, so a
/// daemon that boots next on the same port is not refused with "port already
/// in use".
#[cfg(target_os = "linux")]
mod zenohd_dies_with_the_daemon {
    use crate::common::{
        DAEMON_BOOT, DaemonGuard, MessagingEngine, poll_for, spawn_daemon, wait_for_daemon,
    };
    use std::net::TcpListener;
    use std::path::Path;
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    /// How long a process change (a spawn, a death, a port release) gets to
    /// show in `/proc` and on the port.
    const PROCESS_CHANGE: Duration = Duration::from_secs(5);
    /// The port comes from the ephemeral range other tests' zenoh sessions draw
    /// from, so a collision retries the whole case on a new port.
    const PORT_ATTEMPTS: usize = 3;

    const ROUTER_STARTED: &str = "Zenoh router started";
    const SERVE_INITIALIZED: &str = "Serve command initialized!";
    /// The daemon's refusal, and zenohd's own bind failure quoted from its log.
    const PORT_IN_USE: [&str; 2] = ["Zenoh router port already in use", "Address already in use"];

    /// A process identified beyond pid reuse.
    #[derive(Clone, Copy)]
    struct Process {
        pid: u32,
        start_time: u64,
    }

    /// The fields of `/proc/<pid>/stat` this module reads.
    struct ProcStat {
        state: char,
        ppid: u32,
        start_time: u64,
    }

    enum Outcome {
        Ready,
        PortCollision,
    }

    fn free_port() -> u16 {
        TcpListener::bind("0.0.0.0:0")
            .expect("bind an ephemeral port")
            .local_addr()
            .expect("local address")
            .port()
    }

    /// `None` once the process is gone.
    fn proc_stat(pid: u32) -> Option<ProcStat> {
        let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
        // The command name sits in parentheses and may contain spaces.
        let after_comm = &stat[stat.rfind(')')? + 1..];
        let mut fields = after_comm.split_whitespace();
        let state = fields.next()?.chars().next()?;
        let ppid = fields.next()?.parse().ok()?;
        // Field 22 of the whole line.
        let start_time = fields.nth(17)?.parse().ok()?;
        Some(ProcStat {
            state,
            ppid,
            start_time,
        })
    }

    /// The zenohd that daemon `daemon_pid` runs on `port`.
    fn zenohd_of(daemon_pid: u32, port: u16) -> Option<Process> {
        let config_file = format!("zenohd_config_{port}.json5");
        std::fs::read_dir("/proc")
            .ok()?
            .flatten()
            .find_map(|entry| {
                let pid: u32 = entry.file_name().to_str()?.parse().ok()?;
                let stat = proc_stat(pid)?;
                if stat.ppid != daemon_pid {
                    return None;
                }
                let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
                String::from_utf8_lossy(&cmdline)
                    .contains(&config_file)
                    .then_some(Process {
                        pid,
                        start_time: stat.start_time,
                    })
            })
    }

    /// Gone, a zombie awaiting its reaper, or a pid reused by another process.
    fn is_gone(process: Process) -> bool {
        match proc_stat(process.pid) {
            None => true,
            Some(stat) => stat.state == 'Z' || stat.start_time != process.start_time,
        }
    }

    /// Whether `port` binds within `bound`; another test's session may take a
    /// freed port in that window.
    fn port_frees_within(port: u16, bound: Duration) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            if TcpListener::bind(("0.0.0.0", port)).is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for `ready` in the daemon's logs, or a port collision.
    fn wait_for_boot(daemon: &mut DaemonGuard, logs: &Arc<Mutex<String>>, ready: &str) -> Outcome {
        wait_for_daemon(daemon, logs, DAEMON_BOOT, ready, |snapshot| {
            if PORT_IN_USE.iter().any(|needle| snapshot.contains(needle)) {
                return Some(Outcome::PortCollision);
            }
            snapshot.contains(ready).then_some(Outcome::Ready)
        })
    }

    fn attempt(home: &Path) -> Outcome {
        let port = free_port();
        let (mut daemon, logs) = spawn_daemon(home, MessagingEngine::Zenoh { port });
        if let Outcome::PortCollision = wait_for_boot(&mut daemon, &logs, ROUTER_STARTED) {
            return Outcome::PortCollision;
        }
        let daemon_pid = daemon.0.id();
        let zenohd = poll_for(PROCESS_CHANGE, "the daemon's zenohd under /proc", || {
            zenohd_of(daemon_pid, port)
        });

        // SIGKILL and reap the daemon: no Drop runs in it.
        drop(daemon);

        poll_for(PROCESS_CHANGE, "zenohd's end", || {
            is_gone(zenohd).then_some(())
        });
        if !port_frees_within(port, PROCESS_CHANGE) {
            return Outcome::PortCollision;
        }

        let (mut second, second_logs) = spawn_daemon(home, MessagingEngine::Zenoh { port });
        wait_for_boot(&mut second, &second_logs, SERVE_INITIALIZED)
    }

    #[test]
    fn a_sigkilled_daemon_takes_its_zenohd_with_it() {
        for _ in 0..PORT_ATTEMPTS {
            let home = tempfile::tempdir().expect("temp home");
            if let Outcome::Ready = attempt(home.path()) {
                return;
            }
        }
        panic!("{PORT_ATTEMPTS} attempts in a row hit a port collision");
    }
}
