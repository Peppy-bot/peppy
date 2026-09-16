//! Backend-aware probe of the work an `apptainer build` is doing.
//!
//! `apptainer build` is mostly silent off-TTY. While it downloads a docker base
//! image and assembles the SIF it prints one "Copying blob …" line per blob,
//! then nothing until the phase completes. While its `%post` compiles, a
//! compiler prints nothing between the first line of a crate and the last, and
//! one crate can hold the CPU for many minutes. Neither silence is a hang, and
//! both leave traces the probe can read:
//!
//! - the bytes a download or an assembly moves land on disk, in apptainer's
//!   cache directory, its `--tmpdir` scratch, and the output image;
//! - the CPU a compiler burns is accounted to the build's processes: the
//!   process group the build leads, which every `%post` step starts in, and
//!   every descendant of its leader, which also holds the daemons a step
//!   starts in a group of their own (see [`build_cpu_time`]).
//!
//! Sampling both distinguishes "slow but working" from "wedged": a wedged build
//! moves no bytes and consumes no CPU. The disk probe deliberately sums whole
//! directory roots rather than any cache-internal layout: apptainer's cache
//! structure varies across versions, and a whole-root sum is layout-agnostic.

use super::facade::{Apptainer, Backend};
use super::lima;
use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

/// Apptainer's env spelling of its cache-directory override.
const APPTAINER_CACHEDIR_ENV: &str = "APPTAINER_CACHEDIR";

/// The host-side directory apptainer caches OCI blobs and converted layers in:
/// `$APPTAINER_CACHEDIR` when set in this process's environment, else
/// `~/.apptainer/cache` (apptainer's own default).
///
/// peppy deliberately reads this rather than setting or relocating
/// `APPTAINER_CACHEDIR`: pointing an existing installation at a fresh cache
/// directory would cold-start multi-GB re-downloads for exactly the
/// slow-connection users the progress probe serves.
fn effective_host_cache_dir() -> Option<PathBuf> {
    effective_host_cache_dir_from(
        std::env::var_os(APPTAINER_CACHEDIR_ENV),
        std::env::var_os("HOME"),
    )
}

/// Env-injectable core of [`effective_host_cache_dir`], split out so the
/// resolution order is testable without racing other tests over the process
/// environment.
pub(crate) fn effective_host_cache_dir_from(
    cachedir_env: Option<OsString>,
    home: Option<OsString>,
) -> Option<PathBuf> {
    if let Some(dir) = cachedir_env.filter(|v| !v.is_empty()) {
        return Some(PathBuf::from(dir));
    }
    home.filter(|v| !v.is_empty())
        .map(|home| PathBuf::from(home).join(".apptainer/cache"))
}

/// One reading of the work a build has done so far.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BuildActivity {
    /// Bytes on disk across every surface the build writes to.
    pub bytes_on_disk: u64,
    /// CPU time (user plus system, reaped children included) consumed so far
    /// by the build's processes: its leader, the process group the leader
    /// leads, and the leader's descendants.
    pub cpu_time: Duration,
}

/// Samples the work a build has done: the on-disk footprint of every surface
/// it writes to, and the CPU time its processes have consumed.
/// Self-contained (owns every path it needs, borrows nothing from the facade)
/// so callers can hold it across facade moves and hand it to long-lived
/// monitor tasks. Obtained via [`Apptainer::build_activity_probe`] for a
/// container build, or [`BuildActivityProbe::host`] for a build that runs
/// directly on this host.
#[derive(Clone, Debug)]
pub struct BuildActivityProbe {
    /// Host-side roots, each summed recursively without following symlinks.
    pub(super) host_roots: Vec<PathBuf>,
    /// The pid of the host process the build was spawned as, the leader of
    /// its process group. The CPU time of the build's processes (see
    /// [`build_cpu_time`]) is summed off `/proc` on Linux and off a `ps`
    /// listing on other unix hosts. `None` samples no host CPU.
    pub(super) host_build_leader: Option<u32>,
    /// Guest-side sampling for the Lima backend (macOS), where the apptainer
    /// cache and the build's processes live inside the VM.
    pub(super) guest: Option<GuestActivityProbe>,
}

/// The `limactl shell` plumbing needed to sample the guest side of a build.
#[derive(Clone, Debug)]
pub(super) struct GuestActivityProbe {
    pub(super) limactl_path: PathBuf,
    pub(super) lima_home: PathBuf,
    /// Guest-native file holding the pid of the guest build's leader, which is
    /// also its PGID, written by the guest wrapper (see
    /// [`lima::lima_guest_pgid_argv`]). `None` samples no guest CPU.
    pub(super) pgid_file: Option<PathBuf>,
}

impl Apptainer {
    /// Builds a [`BuildActivityProbe`] over every surface a build on this
    /// backend writes to, plus `extra_host_roots` supplied by the caller
    /// (typically the output SIF path and the container build cache bind
    /// directory), and over the processes doing the build.
    ///
    /// Native (Linux): the host cache dir and the facade's `APPTAINER_TMPDIR`
    /// scratch plus the extras, and the processes of the build led by
    /// `host_build_leader`, the pid of the spawned `apptainer build` (spawned
    /// as a group leader, so its `%post` steps start in its group). Lima
    /// (macOS): the guest cache and the processes of the guest build recorded
    /// under `build_key` (the key passed to `ApptainerCommand::cancel_pgid`)
    /// are sampled through `limactl shell`; only the extras are sampled
    /// host-side (the build's working dir is host-mounted, so they remain
    /// visible). Each backend ignores the handle that belongs to the other.
    pub fn build_activity_probe(
        &self,
        extra_host_roots: Vec<PathBuf>,
        host_build_leader: Option<u32>,
        build_key: Option<&str>,
    ) -> BuildActivityProbe {
        match &self.backend {
            Backend::Native { tmp_dir, .. } => {
                let mut host_roots = extra_host_roots;
                host_roots.extend(effective_host_cache_dir());
                host_roots.push(tmp_dir.clone());
                BuildActivityProbe::host(host_roots, host_build_leader)
            }
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => BuildActivityProbe {
                host_roots: extra_host_roots,
                host_build_leader: None,
                guest: Some(GuestActivityProbe {
                    limactl_path: limactl_path.clone(),
                    lima_home: lima_home.clone(),
                    pgid_file: build_key.map(lima::guest_pgid_path),
                }),
            },
        }
    }
}

impl BuildActivityProbe {
    /// A probe over `host_roots` and the processes of the build led by
    /// `build_leader`, for a build that runs directly on this host.
    pub fn host(host_roots: Vec<PathBuf>, build_leader: Option<u32>) -> Self {
        Self {
            host_roots,
            host_build_leader: build_leader,
            guest: None,
        }
    }

    /// The work done so far across every sampled root and build process.
    ///
    /// Blocking (filesystem walks, a `/proc` scan or a `ps` subprocess, a
    /// `limactl shell` subprocess under Lima), so call it from a blocking
    /// context. Missing
    /// roots count 0, per-root errors are skipped and an unreadable process
    /// table reads as no CPU time, so a partial reading still detects growth
    /// while a persistently failing probe reads flat and defers nothing: the
    /// probe can hold an idle timeout off only while bytes or CPU time are
    /// really accruing, never neuter it.
    pub fn sample(&self) -> BuildActivity {
        let host_bytes = self
            .host_roots
            .iter()
            .fold(0u64, |sum, root| sum.saturating_add(dir_size_bytes(root)));
        let host_cpu = self
            .host_build_leader
            .map_or(Duration::ZERO, host_build_cpu_time);
        let guest = self
            .guest
            .as_ref()
            .map_or_else(BuildActivity::default, GuestActivityProbe::sample);
        BuildActivity {
            bytes_on_disk: host_bytes.saturating_add(guest.bytes_on_disk),
            cpu_time: host_cpu.saturating_add(guest.cpu_time),
        }
    }
}

impl GuestActivityProbe {
    /// The guest-side cache footprint and the CPU time of the guest build's
    /// processes, read in one `limactl shell` round trip through the same
    /// plumbing every other guest command uses (see
    /// [`lima::lima_guest_activity_argv`]). Any failure (VM unreachable, tools
    /// missing, unparseable output) reads as no bytes and no CPU time.
    fn sample(&self) -> BuildActivity {
        let output = lima::lima_shell_cmd(&self.limactl_path, &self.lima_home, lima::LIMA_INSTANCE)
            .args(lima::lima_guest_activity_argv(self.pgid_file.as_deref()))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output();
        match output {
            Ok(out) if out.status.success() => {
                parse_guest_activity(&String::from_utf8_lossy(&out.stdout))
            }
            _ => BuildActivity::default(),
        }
    }
}

/// Parses what the guest activity script prints: the cache footprint in
/// bytes, the guest's clock-tick rate and the pid of the build's leader, one
/// per line, then the guest's `/proc/<pid>/stat` lines, whose CPU time is
/// summed over the build's processes (see [`build_cpu_time`]). A missing or
/// malformed number reads as 0 bytes, [`DEFAULT_CLOCK_TICKS_PER_SECOND`] and
/// no leader respectively; with no leader the reading holds no CPU time.
pub(super) fn parse_guest_activity(output: &str) -> BuildActivity {
    let mut lines = output.lines();
    let mut next_number = || {
        lines
            .next()
            .and_then(|line| line.trim().parse::<u64>().ok())
    };
    let bytes_on_disk = next_number().unwrap_or(0);
    let ticks_per_second = next_number()
        .and_then(NonZeroU64::new)
        .unwrap_or(DEFAULT_CLOCK_TICKS_PER_SECOND);
    let build_leader = next_number()
        .and_then(|pid| u32::try_from(pid).ok())
        .filter(|pid| *pid != 0);
    let cpu_time = build_leader.map_or(Duration::ZERO, |leader| {
        let processes: Vec<ProcessCpu> = lines
            .filter_map(|line| parse_process_stat(line, ticks_per_second))
            .collect();
        build_cpu_time(&processes, leader)
    });
    BuildActivity {
        bytes_on_disk,
        cpu_time,
    }
}

/// One process of a process table, as far as the CPU accounting reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ProcessCpu {
    pub(super) pid: u32,
    /// The pid of its parent (`ppid`).
    pub(super) parent: u32,
    /// The process group it belongs to (`pgid`).
    pub(super) process_group: u32,
    /// User plus system time, plus that of the children it reaped where the
    /// source accounts for them (`/proc` does, `ps` does not).
    pub(super) cpu_time: Duration,
}

/// CPU time consumed so far by the processes of the build led by `leader`, as
/// a snapshot of the process table records it.
///
/// The build's processes are the leader, the members of the process group it
/// leads, and every descendant of the leader in a group of its own. Every
/// process a build spawns starts in the leader's group, but a daemon leaves
/// it: the sccache server a Rust build's `RUSTC_WRAPPER` starts calls
/// `setsid` and runs every compiler it serves in its new group. The daemon
/// stays a descendant because apptainer, a child subreaper, adopts it when
/// the process that started it exits, so a sum over the group alone would
/// miss every cached compile. A group member that left the leader's tree (an
/// orphan adopted by init) still counts through the group.
///
/// Each counted process contributes the time its source accounts to it, so
/// under `/proc` a compiler that exits hands its time to its parent's reaped
/// counters and the total keeps growing; time reaped outside the build's
/// processes drops out of the sum.
pub(super) fn build_cpu_time(processes: &[ProcessCpu], leader: u32) -> Duration {
    let descendants = descendants_of(processes, leader);
    processes
        .iter()
        .filter(|process| {
            process.pid == leader
                || process.process_group == leader
                || descendants.contains(&process.pid)
        })
        .fold(Duration::ZERO, |sum, process| {
            sum.saturating_add(process.cpu_time)
        })
}

/// The pids of every process that descends from `ancestor` in the table,
/// following parent links down. A snapshot read while processes exit and pids
/// get reused can link a pid back to itself; each pid is walked once.
fn descendants_of(processes: &[ProcessCpu], ancestor: u32) -> HashSet<u32> {
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for process in processes {
        children
            .entry(process.parent)
            .or_default()
            .push(process.pid);
    }
    let mut descendants = HashSet::new();
    let mut pending = vec![ancestor];
    while let Some(parent) = pending.pop() {
        for &child in children.get(&parent).map_or(&[][..], Vec::as_slice) {
            if descendants.insert(child) {
                pending.push(child);
            }
        }
    }
    descendants
}

/// CPU time consumed so far by the processes of the host build led by
/// `leader` (see [`build_cpu_time`]), read off `/proc/<pid>/stat`, which
/// accounts each process's user and system time plus that of the children it
/// reaped.
#[cfg(target_os = "linux")]
fn host_build_cpu_time(leader: u32) -> Duration {
    build_cpu_time(&host_process_table(), leader)
}

/// Every process `/proc` lists, with its CPU time converted at the kernel's
/// clock-tick rate. An unreadable `/proc` reads as an empty table.
#[cfg(target_os = "linux")]
fn host_process_table() -> Vec<ProcessCpu> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let ticks_per_second = host_clock_ticks_per_second();
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| is_pid_dir_name(&entry.file_name()))
        // A process that exits between the listing and the read is skipped.
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter_map(|stat| parse_process_stat(&stat, ticks_per_second))
        .collect()
}

/// Unix hosts without `/proc` (macOS, where a `build_cmd` runs on the host
/// while container builds run in the Lima guest) read the table off `ps`.
#[cfg(all(unix, not(target_os = "linux")))]
fn host_build_cpu_time(leader: u32) -> Duration {
    ps_build_cpu_time(leader)
}

/// A host with neither `/proc` nor `ps` watches a build through its output
/// and its disk writes alone.
#[cfg(not(unix))]
fn host_build_cpu_time(_leader: u32) -> Duration {
    Duration::ZERO
}

/// CPU time consumed so far by the processes of the build led by `leader`
/// (see [`build_cpu_time`]), as `ps` reports it: `ps -A -o pid= -o ppid= -o
/// pgid= -o cputime=` lists every process's place in the table and its
/// accumulated user plus system time. A process that exits takes its time
/// with it, so the total dips when a compiler finishes a crate; the monitor
/// rebases on a dip and measures the next crate's growth from the new floor.
/// Any failure reads as no CPU time. Compiled into Linux tests as well, so
/// the procps `ps` there exercises the listing end to end.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(super) fn ps_build_cpu_time(leader: u32) -> Duration {
    let output = std::process::Command::new("ps")
        .args([
            "-A", "-o", "pid=", "-o", "ppid=", "-o", "pgid=", "-o", "cputime=",
        ])
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(out) if out.status.success() => build_cpu_time(
            &parse_ps_process_table(&String::from_utf8_lossy(&out.stdout)),
            leader,
        ),
        _ => Duration::ZERO,
    }
}

/// Reads a `ps -o pid= -o ppid= -o pgid= -o cputime=` listing into a process
/// table. A line that does not parse, such as one holding the `-` BSD `ps`
/// prints for a zombie's time, is left out.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(super) fn parse_ps_process_table(listing: &str) -> Vec<ProcessCpu> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            Some(ProcessCpu {
                pid: fields.next()?.parse().ok()?,
                parent: fields.next()?.parse().ok()?,
                process_group: fields.next()?.parse().ok()?,
                cpu_time: parse_ps_cputime(fields.next()?)?,
            })
        })
        .collect()
}

/// Reads the `cputime` column of `ps`. BSD `ps` (macOS) prints
/// `minutes:seconds.hundredths` with the minutes unbounded; procps (Linux)
/// prints `[[days-]hours:]minutes:seconds`. Both are read: an optional
/// `days-` prefix, two or three colon-separated clock fields, an optional
/// fraction of a second. `None` for anything else.
#[cfg(any(test, all(unix, not(target_os = "linux"))))]
pub(super) fn parse_ps_cputime(field: &str) -> Option<Duration> {
    let field = field.trim();
    let (days, clock) = match field.split_once('-') {
        Some((days, clock)) => (days.parse::<u64>().ok()?, clock),
        None => (0, field),
    };
    let (whole, fraction) = clock.split_once('.').unwrap_or((clock, ""));
    let fields = whole
        .split(':')
        .map(|part| part.parse::<u64>().ok())
        .collect::<Option<Vec<u64>>>()?;
    let (hours, minutes, seconds) = match fields[..] {
        [minutes, seconds] => (0, minutes, seconds),
        [hours, minutes, seconds] => (hours, minutes, seconds),
        _ => return None,
    };
    let millis = match fraction {
        "" => 0,
        digits if digits.bytes().all(|byte| byte.is_ascii_digit()) => {
            // The first three digits, right-padded: ".5" is 500 ms, ".05" 50.
            let leading: String = digits.chars().take(3).collect();
            format!("{leading:0<3}").parse::<u64>().ok()?
        }
        _ => return None,
    };
    let secs = ((days * 24 + hours) * 60 + minutes) * 60 + seconds;
    Some(Duration::from_secs(secs) + Duration::from_millis(millis))
}

/// Whether a `/proc` entry names a process (all digits) rather than one of the
/// kernel's own files.
#[cfg(target_os = "linux")]
fn is_pid_dir_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
}

/// Reads one `/proc/<pid>/stat` line into a process table entry, converting
/// its clock ticks at `ticks_per_second`. Read on the host under Linux and,
/// for the Lima backend, off the lines the guest prints.
///
/// The pid leads the line. The second field, the command name, sits in
/// parentheses and may itself contain spaces and parentheses, so the fixed
/// fields are counted from the last closing parenthesis on the line: state,
/// ppid, pgrp, session, tty_nr, tpgid, flags, minflt, cminflt, majflt,
/// cmajflt, utime, stime, cutime, cstime, in that order (`proc_pid_stat(5)`).
/// `None` for a line that does not hold them all, which includes either half
/// of a line a command name holding a newline split in two. The
/// reaped-children counters are signed in the kernel's format; a negative one
/// counts as 0.
pub(super) fn parse_process_stat(line: &str, ticks_per_second: NonZeroU64) -> Option<ProcessCpu> {
    let (pid, _) = line.split_once(' ')?;
    let pid = pid.parse().ok()?;
    let after_comm = &line[line.rfind(')')? + 1..];
    let mut fields = after_comm.split_ascii_whitespace();
    let parent = fields.nth(1)?.parse().ok()?;
    let process_group = fields.next()?.parse().ok()?;
    let mut cpu_fields = fields.skip(8);
    let mut cpu_ticks = 0u64;
    for _ in 0..4 {
        let ticks: i64 = cpu_fields.next()?.parse().ok()?;
        cpu_ticks = cpu_ticks.saturating_add(u64::try_from(ticks).unwrap_or(0));
    }
    Some(ProcessCpu {
        pid,
        parent,
        process_group,
        cpu_time: clock_ticks_to_duration(cpu_ticks, ticks_per_second),
    })
}

/// The clock-tick rate `/proc` counters are read at when the kernel's own
/// cannot be learned: 100, the `USER_HZ` every Linux architecture peppy runs
/// on reports.
const DEFAULT_CLOCK_TICKS_PER_SECOND: NonZeroU64 = NonZeroU64::new(100).unwrap();

/// The kernel's `USER_HZ`, read through `sysconf(_SC_CLK_TCK)`, or
/// [`DEFAULT_CLOCK_TICKS_PER_SECOND`] when `sysconf` cannot answer.
#[cfg(target_os = "linux")]
fn host_clock_ticks_per_second() -> NonZeroU64 {
    nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .and_then(|hz| u64::try_from(hz).ok())
        .and_then(NonZeroU64::new)
        .unwrap_or(DEFAULT_CLOCK_TICKS_PER_SECOND)
}

/// Converts `/proc` clock ticks to time at `ticks_per_second`.
fn clock_ticks_to_duration(ticks: u64, ticks_per_second: NonZeroU64) -> Duration {
    Duration::from_micros(ticks.saturating_mul(1_000_000) / ticks_per_second.get())
}

/// Recursive file-size sum of `root`, never following symlinks (a symlink's
/// own metadata is counted, not its target's). Missing paths and unreadable
/// entries contribute 0 so one bad subtree cannot zero out the whole sample.
fn dir_size_bytes(root: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(root) {
        Ok(meta) => meta,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        dir_tree_size(root)
    } else {
        meta.len()
    }
}

/// Sums the entries of a directory known to exist. Leans on `read_dir`'s own
/// per-entry handles (`file_type`, free on most filesystems, to pick the
/// recursion; `metadata`, an lstat with no path building, for sizes) because
/// this runs over the whole apptainer cache every sample tick.
fn dir_tree_size(dir: &Path) -> u64 {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    entries
        .filter_map(|entry| entry.ok())
        .fold(0, |sum, entry| {
            let size = match entry.file_type() {
                Ok(file_type) if file_type.is_dir() => dir_tree_size(&entry.path()),
                Ok(_) => entry.metadata().map_or(0, |meta| meta.len()),
                Err(_) => 0,
            };
            sum.saturating_add(size)
        })
}
