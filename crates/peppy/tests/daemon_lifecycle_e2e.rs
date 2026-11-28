//! Real-binary end-to-end coverage of how the daemon process ends.
//!
//! Spawns the actual `peppy service serve` binary as a separate OS process. The
//! signal cases verify it shuts down cleanly on both SIGINT (ctrl+C) and
//! SIGTERM (systemd stop); they run on the mock engine, which needs no router,
//! and take a fraction of a second. The zenohd cases (Linux) boot the daemon on
//! zenoh: one verifies that its router listens on the loopback interface
//! alone; the other SIGKILLs it, and verifies the kernel ends its zenohd and
//! the next daemon on the same port boots.
//!
//! These run in the default suite.
//!
//! The daemon-side teardown of node *processes* is covered without a separate
//! binary by `core-node`'s `teardown_all_instances` test, and the watchdog
//! timing by `peppylib`'s `daemon_watchdog` tests.

use crate::common::{
    DAEMON_BOOT, MessagingEngine, SERVE_INITIALIZED, spawn_daemon, wait_for_daemon, wait_for_exit,
};
use std::time::Duration;

fn run_shutdown_signal_case(signal: rustix::process::Signal) {
    let home = tempfile::tempdir().expect("temp home");
    let (mut guard, logs) = spawn_daemon(home.path(), MessagingEngine::Mock);

    // Wait until the serve loop is fully up before signaling.
    wait_for_daemon(
        &mut guard,
        &logs,
        DAEMON_BOOT,
        SERVE_INITIALIZED,
        |snapshot| snapshot.contains(SERVE_INITIALIZED).then_some(()),
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
        Boot, MessagingEngine, PORT_ATTEMPTS, SERVE_INITIALIZED, free_port, poll_for, spawn_daemon,
        wait_for_boot,
    };
    use std::net::TcpListener;
    use std::path::Path;
    use std::time::{Duration, Instant};

    /// How long a process change (a spawn, a death, a port release) gets to
    /// show in `/proc` and on the port.
    const PROCESS_CHANGE: Duration = Duration::from_secs(5);
    const ROUTER_STARTED: &str = "Zenoh router started";

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

    fn attempt(home: &Path) -> Boot {
        let port = free_port();
        let (mut daemon, logs) = spawn_daemon(home, MessagingEngine::Zenoh { port });
        if matches!(
            wait_for_boot(&mut daemon, &logs, ROUTER_STARTED),
            Boot::PortCollision
        ) {
            return Boot::PortCollision;
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
            return Boot::PortCollision;
        }

        let (mut second, second_logs) = spawn_daemon(home, MessagingEngine::Zenoh { port });
        wait_for_boot(&mut second, &second_logs, SERVE_INITIALIZED)
    }

    /// `127.0.0.1` as `/proc/net/tcp` writes it: the address in the byte
    /// order of the host, which is little-endian on the machines that run
    /// the suite.
    const LOOPBACK_IN_PROC_NET_TCP: &str = "0100007F";

    /// The local address of each socket that listens on `port`, as
    /// `/proc/net/tcp` and `/proc/net/tcp6` write it.
    fn listen_addresses(port: u16) -> Vec<String> {
        /// The state of a listening socket in the tables.
        const LISTEN: &str = "0A";
        let mut addresses = Vec::new();
        for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
            let text = std::fs::read_to_string(table).unwrap_or_default();
            for line in text.lines().skip(1) {
                let fields: Vec<&str> = line.split_whitespace().collect();
                let (Some(local), Some(state)) = (fields.get(1), fields.get(3)) else {
                    continue;
                };
                let Some((address, local_port)) = local.split_once(':') else {
                    continue;
                };
                if *state == LISTEN && u16::from_str_radix(local_port, 16) == Ok(port) {
                    addresses.push(address.to_owned());
                }
            }
        }
        addresses
    }

    /// The transport of the managed router has no authentication, so no host
    /// of the network can connect to it: it listens on the loopback
    /// interface alone. "Zenoh router started" comes once the router accepts
    /// connections.
    #[test]
    fn the_managed_router_listens_on_the_loopback_interface_only() {
        for _ in 0..PORT_ATTEMPTS {
            let home = tempfile::tempdir().expect("temp home");
            let port = free_port();
            let (mut daemon, logs) = spawn_daemon(home.path(), MessagingEngine::Zenoh { port });
            if matches!(
                wait_for_boot(&mut daemon, &logs, ROUTER_STARTED),
                Boot::PortCollision
            ) {
                continue;
            }
            assert_eq!(
                listen_addresses(port),
                [LOOPBACK_IN_PROC_NET_TCP],
                "the router of port {port} must listen on 127.0.0.1 alone"
            );
            return;
        }
        panic!("{PORT_ATTEMPTS} attempts in a row hit a port collision");
    }

    #[test]
    fn a_sigkilled_daemon_takes_its_zenohd_with_it() {
        for _ in 0..PORT_ATTEMPTS {
            let home = tempfile::tempdir().expect("temp home");
            if matches!(attempt(home.path()), Boot::Ready) {
                return;
            }
        }
        panic!("{PORT_ATTEMPTS} attempts in a row hit a port collision");
    }
}
