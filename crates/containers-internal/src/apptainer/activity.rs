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
//! - the CPU a compiler burns is accounted to the build's process group, which
//!   every `%post` step belongs to.
//!
//! Sampling both distinguishes "slow but working" from "wedged": a wedged build
//! moves no bytes and consumes no CPU. The disk probe deliberately sums whole
//! directory roots rather than any cache-internal layout: apptainer's cache
//! structure varies across versions, and a whole-root sum is layout-agnostic.

use super::facade::{Apptainer, Backend};
use super::lima;
use std::ffi::OsString;
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
    /// by the build's process group.
    pub cpu_time: Duration,
}

/// Samples the work a build has done: the on-disk footprint of every surface
/// it writes to, and the CPU time its process group has consumed.
/// Self-contained (owns every path it needs, borrows nothing from the facade)
/// so callers can hold it across facade moves and hand it to long-lived
/// monitor tasks. Obtained via [`Apptainer::build_activity_probe`] for a
/// container build, or [`BuildActivityProbe::host`] for a build that runs
/// directly on this host.
#[derive(Clone, Debug)]
pub struct BuildActivityProbe {
    /// Host-side roots, each summed recursively without following symlinks.
    pub(super) host_roots: Vec<PathBuf>,
    /// The host process group the build runs in, named by its leader's pid,
    /// whose CPU time is summed off `/proc`. `None` samples no host CPU.
    pub(super) host_process_group: Option<u32>,
    /// Guest-side sampling for the Lima backend (macOS), where the apptainer
    /// cache and the build's processes live inside the VM.
    pub(super) guest: Option<GuestActivityProbe>,
}

/// The `limactl shell` plumbing needed to sample the guest side of a build.
#[derive(Clone, Debug)]
pub(super) struct GuestActivityProbe {
    pub(super) limactl_path: PathBuf,
    pub(super) lima_home: PathBuf,
    /// Guest-native file holding the PGID of the build's process group, written
    /// by the guest wrapper (see [`lima::lima_guest_pgid_argv`]). `None`
    /// samples no guest CPU.
    pub(super) pgid_file: Option<PathBuf>,
}

impl Apptainer {
    /// Builds a [`BuildActivityProbe`] over every surface a build on this
    /// backend writes to, plus `extra_host_roots` supplied by the caller
    /// (typically the output SIF path and the container build cache bind
    /// directory), and over the processes doing the build.
    ///
    /// Native (Linux): the host cache dir and the facade's `APPTAINER_TMPDIR`
    /// scratch plus the extras, and the host process group led by
    /// `host_process_group`, the pid of the spawned `apptainer build` (spawned
    /// as a group leader, so its `%post` steps share the group). Lima (macOS):
    /// the guest cache and the guest process group recorded under `build_key`
    /// (the key passed to `ApptainerCommand::cancel_pgid`) are sampled through
    /// `limactl shell`; only the extras are sampled host-side (the build's
    /// working dir is host-mounted, so they remain visible). Each backend
    /// ignores the handle that belongs to the other.
    pub fn build_activity_probe(
        &self,
        extra_host_roots: Vec<PathBuf>,
        host_process_group: Option<u32>,
        build_key: Option<&str>,
    ) -> BuildActivityProbe {
        match &self.backend {
            Backend::Native { tmp_dir, .. } => {
                let mut host_roots = extra_host_roots;
                host_roots.extend(effective_host_cache_dir());
                host_roots.push(tmp_dir.clone());
                BuildActivityProbe::host(host_roots, host_process_group)
            }
            Backend::Lima {
                limactl_path,
                lima_home,
                ..
            } => BuildActivityProbe {
                host_roots: extra_host_roots,
                host_process_group: None,
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
    /// A probe over `host_roots` and the host process group led by
    /// `process_group`, for a build that runs directly on this host.
    pub fn host(host_roots: Vec<PathBuf>, process_group: Option<u32>) -> Self {
        Self {
            host_roots,
            host_process_group: process_group,
            guest: None,
        }
    }

    /// The work done so far across every sampled root and process group.
    ///
    /// Blocking (filesystem walks, a `/proc` scan, a `limactl shell`
    /// subprocess under Lima), so call it from a blocking context. Missing
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
            .host_process_group
            .map_or(Duration::ZERO, host_process_group_cpu_time);
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
    /// The guest-side cache footprint and the CPU time of the recorded process
    /// group, read in one `limactl shell` round trip through the same plumbing
    /// every other guest command uses (see [`lima::lima_guest_activity_argv`]).
    /// Any failure (VM unreachable, tools missing, unparseable output) reads as
    /// no bytes and no CPU time.
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

/// Parses the two lines the guest activity script prints: the cache
/// footprint in bytes, then the process group's CPU time in milliseconds. A
/// missing or malformed line reads as 0 for that reading alone.
pub(super) fn parse_guest_activity(output: &str) -> BuildActivity {
    let mut lines = output.lines();
    let mut next_number = || {
        lines
            .next()
            .and_then(|line| line.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let bytes_on_disk = next_number();
    let cpu_millis = next_number();
    BuildActivity {
        bytes_on_disk,
        cpu_time: Duration::from_millis(cpu_millis),
    }
}

/// CPU time consumed so far by the live processes of the host process group
/// led by `process_group`: each one's user and system time plus the time of
/// the children it has already reaped, read off `/proc/<pid>/stat`. A process
/// that exits hands its time to its parent's reaped-children counters, so the
/// group's total keeps growing as a compiler finishes one crate and starts
/// the next; only time reaped outside the group (an orphan adopted by init)
/// drops out of the sum.
#[cfg(target_os = "linux")]
fn host_process_group_cpu_time(process_group: u32) -> Duration {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Duration::ZERO;
    };
    let ticks = entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| is_pid_dir_name(&entry.file_name()))
        // A process that exits between the listing and the read is skipped.
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter_map(|stat| parse_process_stat(&stat))
        .filter(|stat| stat.process_group == process_group)
        .fold(0u64, |sum, stat| sum.saturating_add(stat.cpu_ticks));
    clock_ticks_to_duration(ticks)
}

/// Off Linux there is no `/proc` to read: the native backend runs only on
/// Linux, and a host build elsewhere is watched through its output alone.
#[cfg(not(target_os = "linux"))]
fn host_process_group_cpu_time(_process_group: u32) -> Duration {
    Duration::ZERO
}

/// Whether a `/proc` entry names a process (all digits) rather than one of the
/// kernel's own files.
#[cfg(target_os = "linux")]
fn is_pid_dir_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|name| !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit()))
}

/// The fields of one `/proc/<pid>/stat` line the CPU accounting reads.
#[cfg(target_os = "linux")]
#[derive(Debug, PartialEq, Eq)]
pub(super) struct ProcessStat {
    /// The process group the process belongs to (`pgrp`).
    pub(super) process_group: u32,
    /// `utime + stime + cutime + cstime`, in clock ticks.
    pub(super) cpu_ticks: u64,
}

/// Reads the process group and the CPU time off one `/proc/<pid>/stat` line.
///
/// The second field, the command name, sits in parentheses and may itself
/// contain spaces and parentheses, so the fixed fields are counted from the
/// last closing parenthesis on the line: state, ppid, pgrp, session, tty_nr,
/// tpgid, flags, minflt, cminflt, majflt, cmajflt, utime, stime, cutime,
/// cstime, in that order (`proc_pid_stat(5)`). `None` for a line that does not
/// hold them all. The reaped-children counters are signed in the kernel's
/// format; a negative one counts as 0.
#[cfg(target_os = "linux")]
pub(super) fn parse_process_stat(line: &str) -> Option<ProcessStat> {
    let after_comm = &line[line.rfind(')')? + 1..];
    let mut fields = after_comm.split_ascii_whitespace();
    let process_group = fields.nth(2)?.parse().ok()?;
    let mut cpu_fields = fields.skip(8);
    let mut cpu_ticks = 0u64;
    for _ in 0..4 {
        let ticks: i64 = cpu_fields.next()?.parse().ok()?;
        cpu_ticks = cpu_ticks.saturating_add(u64::try_from(ticks).unwrap_or(0));
    }
    Some(ProcessStat {
        process_group,
        cpu_ticks,
    })
}

/// Converts `/proc` clock ticks to time using the kernel's `USER_HZ`, read
/// through `sysconf(_SC_CLK_TCK)`; 100, the value every Linux architecture
/// peppy runs on reports, stands in when `sysconf` cannot answer.
#[cfg(target_os = "linux")]
fn clock_ticks_to_duration(ticks: u64) -> Duration {
    const USER_HZ: u64 = 100;
    let ticks_per_second = nix::unistd::sysconf(nix::unistd::SysconfVar::CLK_TCK)
        .ok()
        .flatten()
        .and_then(|hz| u64::try_from(hz).ok())
        .filter(|hz| *hz > 0)
        .unwrap_or(USER_HZ);
    Duration::from_micros(ticks.saturating_mul(1_000_000) / ticks_per_second)
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
