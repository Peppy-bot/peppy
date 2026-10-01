#!/usr/bin/env python3
"""Write a report of the job's RunsOn sticky disk to the job summary.

The post step of the sticky-disk-report action runs this script at the end of
the job, before the post step of runs-on/action unmounts the disk and RunsOn
snapshots it. The report tells what the snapshot of this job holds:

- the disk: its size, and how much of it is used now and at the restore;
- each cache on the disk: its size at the restore, now, and the change;
- the largest directories inside the caches, with the same three columns.

runs-on/action keeps each cache in its own directory under `mounts/` on the
disk and bind-mounts it onto the cache path. The script reads those
directories from the disk itself, so a cache that no job mounts any more
still shows, and it names each one by the path the job mounted it on, read
from the mount table.

The sizes at the restore come from a record that this script writes at the
root of the disk at the end of each job, and that the snapshot keeps for the
next job of the lineage. A disk without a record started blank, or came from
a snapshot taken before the report existed.

The script only reports. Anything it cannot read becomes a note in the
report, and the post step turns a crash into a warning, so the job result
never depends on it.
"""

from __future__ import annotations

import dataclasses
import datetime
import json
import os
import subprocess
import sys
from pathlib import Path

RECORD_NAME = "sticky-disk-report.json"
RECORD_VERSION = 1
CACHES_DIR = "mounts"
# Levels of directories below `mounts/` that du reports: the caches (1), and
# two levels inside them (2 and 3), which is where a cache's own layout shows
# (a target dir and its profile, an image store, a uv cache).
DU_DEPTH = 3
DU_TIMEOUT_SECONDS = 300
LARGEST_DIRECTORIES = 10


@dataclasses.dataclass(frozen=True)
class Mount:
    """One line of /proc/self/mountinfo, reduced to what the report reads."""

    device: str  # major:minor of the file system
    root: str  # the directory of that file system mounted here
    mount_point: str


@dataclasses.dataclass(frozen=True)
class DiskUsage:
    total_bytes: int
    used_bytes: int


@dataclasses.dataclass(frozen=True)
class Measurement:
    """The disk as this job leaves it."""

    disk: DiskUsage
    # Bytes used by each directory down to DU_DEPTH below `mounts/`, keyed by
    # its path relative to `mounts/`.
    directories: dict[str, int]
    notes: list[str]


@dataclasses.dataclass(frozen=True)
class Record:
    """What a job leaves on the disk for the next job of the lineage."""

    written_at: str
    run_url: str
    job: str
    disk_used_bytes: int
    directories: dict[str, int]


@dataclasses.dataclass(frozen=True)
class Row:
    label: str
    at_restore: int | None
    now: int


OCTAL_DIGITS = frozenset("01234567")


def unescape_mountinfo(field: str) -> str:
    # The kernel writes a space, a tab, a newline and a backslash in a path
    # as a backslash and three octal digits.
    out, i = [], 0
    while i < len(field):
        escape = field[i + 1 : i + 4]
        if field[i] == "\\" and len(escape) == 3 and set(escape) <= OCTAL_DIGITS:
            out.append(chr(int(escape, 8)))
            i += 4
        else:
            out.append(field[i])
            i += 1
    return "".join(out)


def parse_mountinfo(text: str) -> list[Mount]:
    mounts = []
    for line in text.splitlines():
        fields = line.split()
        if len(fields) < 5:
            continue
        mounts.append(
            Mount(
                device=fields[2],
                root=unescape_mountinfo(fields[3]),
                mount_point=unescape_mountinfo(fields[4]),
            )
        )
    return mounts


def cache_mount_points(mounts: list[Mount], disk_dir: str) -> dict[str, str]:
    """Map each cache directory under `mounts/` to the path it is mounted on.

    The disk is the mount whose mount point is disk_dir. A cache is a bind
    mount of the same file system whose root is a directory under `mounts/`.
    """
    disk_devices = {m.device for m in mounts if m.mount_point == disk_dir}
    prefix = "/" + CACHES_DIR + "/"
    points = {}
    for mount in mounts:
        if mount.device in disk_devices and mount.root.startswith(prefix):
            name = mount.root[len(prefix) :].split("/", 1)[0]
            points.setdefault(name, mount.mount_point)
    return points


def parse_du(text: str, caches_dir: str) -> dict[str, int]:
    """Read `du --block-size=1` output into sizes keyed relative to caches_dir."""
    sizes = {}
    root = caches_dir.rstrip("/") + "/"
    for line in text.splitlines():
        size, _, path = line.partition("\t")
        if not path.startswith(root) or not size.isdigit():
            continue
        sizes[path[len(root) :]] = int(size)
    return sizes


def load_record(path: Path) -> tuple[Record | None, str | None]:
    """Return the record of the previous job, or why there is none to use."""
    try:
        data = json.loads(path.read_text())
    except FileNotFoundError:
        return None, None
    except (OSError, ValueError) as error:
        return None, f"The record of the previous job could not be read ({error})."
    if not isinstance(data, dict) or data.get("version") != RECORD_VERSION:
        return None, "The record of the previous job has another format; the restore column is empty."
    try:
        return (
            Record(
                written_at=str(data["written_at"]),
                run_url=str(data["run_url"]),
                job=str(data["job"]),
                disk_used_bytes=int(data["disk_used_bytes"]),
                directories={str(k): int(v) for k, v in data["directories"].items()},
            ),
            None,
        )
    except (KeyError, TypeError, ValueError, AttributeError) as error:
        return None, f"The record of the previous job is incomplete ({error!r})."


def record_json(record: Record) -> str:
    return json.dumps(
        {
            "version": RECORD_VERSION,
            "written_at": record.written_at,
            "run_url": record.run_url,
            "job": record.job,
            "disk_used_bytes": record.disk_used_bytes,
            "directories": record.directories,
        },
        indent=1,
        sort_keys=True,
    )


def format_bytes(size: int) -> str:
    for unit, scale in (("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)):
        if abs(size) >= scale:
            return f"{size / scale:.1f} {unit}"
    return f"{size} B"


def format_change(before: int | None, now: int) -> str:
    if before is None:
        return "new"
    change = now - before
    if change == 0:
        return "0"
    return ("+" if change > 0 else "-") + format_bytes(abs(change))


def label_directory(key: str, mount_points: dict[str, str], home: str) -> str:
    """Name a directory under `mounts/` by the path the job sees it at."""
    cache, _, rest = key.partition("/")
    point = mount_points.get(cache)
    if point is None:
        return f"{CACHES_DIR}/{key} (not mounted by this job)"
    if home and (point == home or point.startswith(home.rstrip("/") + "/")):
        point = "~" + point[len(home.rstrip("/")) :]
    return f"{point}/{rest}" if rest else point


def cache_rows(measurement: Measurement, record: Record | None, mount_points: dict[str, str], home: str) -> list[Row]:
    caches = sorted(k for k in measurement.directories if "/" not in k)
    return [
        Row(
            label=label_directory(key, mount_points, home),
            at_restore=record.directories.get(key) if record else None,
            now=measurement.directories[key],
        )
        for key in caches
    ]


def largest_rows(measurement: Measurement, record: Record | None, mount_points: dict[str, str], home: str) -> list[Row]:
    inner = [k for k in measurement.directories if "/" in k]
    inner.sort(key=lambda k: (-measurement.directories[k], k))
    return [
        Row(
            label=label_directory(key, mount_points, home),
            at_restore=record.directories.get(key) if record else None,
            now=measurement.directories[key],
        )
        for key in inner[:LARGEST_DIRECTORIES]
    ]


def render_table(rows: list[Row], has_record: bool) -> list[str]:
    lines = ["| Directory | At restore | Now | Change |", "|---|---:|---:|---:|"]
    for row in rows:
        before = "-" if row.at_restore is None else format_bytes(row.at_restore)
        change = format_change(row.at_restore, row.now) if has_record else "-"
        lines.append(f"| `{row.label}` | {before} | {format_bytes(row.now)} | {change} |")
    return lines


def render_report(
    name: str,
    measurement: Measurement,
    record: Record | None,
    notes: list[str],
    mount_points: dict[str, str],
    home: str,
) -> str:
    disk = measurement.disk
    share = 100 * disk.used_bytes / disk.total_bytes if disk.total_bytes else 0.0
    used = f"{format_bytes(disk.used_bytes)} of {format_bytes(disk.total_bytes)} ({share:.0f}%)"
    if record:
        restored = f"From the snapshot left by [{record.job}]({record.run_url}) at {record.written_at}"
        used += f", {format_change(record.disk_used_bytes, disk.used_bytes)} since the restore"
    else:
        restored = "No record of an earlier job: the disk started blank, or from a snapshot taken before this report existed"
    lines = [
        f"### Sticky disk `{name}`",
        "",
        "| | |",
        "|---|---|",
        f"| Restored | {restored} |",
        f"| Used | {used} |",
        "",
    ]
    caches = cache_rows(measurement, record, mount_points, home)
    if not caches:
        lines += ["No cache directory on the disk.", ""]
    else:
        lines += ["#### Caches", ""]
        lines += render_table(caches, record is not None)
        lines += [""]
        largest = largest_rows(measurement, record, mount_points, home)
        if largest:
            lines += [f"#### Largest directories in the caches (up to {LARGEST_DIRECTORIES})", ""]
            lines += render_table(largest, record is not None)
            lines += [""]
    for note in notes:
        lines += [f"> {note}", ""]
    return "\n".join(lines)


def measure(disk_dir: str) -> Measurement:
    stat = os.statvfs(disk_dir)
    disk = DiskUsage(
        total_bytes=stat.f_blocks * stat.f_frsize,
        used_bytes=(stat.f_blocks - stat.f_bfree) * stat.f_frsize,
    )
    caches_dir = os.path.join(disk_dir, CACHES_DIR)
    if not os.path.isdir(caches_dir):
        return Measurement(disk=disk, directories={}, notes=[])
    notes = []
    try:
        du = subprocess.run(
            ["du", "-x", "--block-size=1", f"--max-depth={DU_DEPTH}", caches_dir],
            capture_output=True,
            text=True,
            timeout=DU_TIMEOUT_SECONDS,
        )
    except subprocess.TimeoutExpired:
        return Measurement(disk=disk, directories={}, notes=[f"du did not finish within {DU_TIMEOUT_SECONDS} seconds; no directory sizes."])
    if du.returncode != 0:
        first_error = du.stderr.strip().splitlines()[0] if du.stderr.strip() else f"exit code {du.returncode}"
        notes.append(f"du could not read everything, so some sizes are low: {first_error}")
    return Measurement(disk=disk, directories=parse_du(du.stdout, caches_dir), notes=notes)


def write_record(path: Path, record: Record) -> str | None:
    """Write the record for the next job; return a note when it cannot."""
    partial = path.with_name(path.name + ".partial")
    try:
        partial.write_text(record_json(record))
        partial.replace(path)
    except OSError as error:
        return f"The record for the next job could not be written ({error}); its restore column will be empty."
    return None


def append_summary(markdown: str) -> None:
    summary = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary:
        with open(summary, "a") as out:
            out.write(markdown + "\n")
    print(markdown)


def run_url() -> str:
    server = os.environ.get("GITHUB_SERVER_URL", "https://github.com")
    repository = os.environ.get("GITHUB_REPOSITORY", "")
    run_id = os.environ.get("GITHUB_RUN_ID", "")
    attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
    return f"{server}/{repository}/actions/runs/{run_id}/attempts/{attempt}"


def main() -> int:
    disk_dir = os.environ.get("RUNS_ON_STICKYDISK_DIR")
    name = os.environ.get("RUNS_ON_STICKYDISK_NAME", "sticky disk")
    unavailable = os.environ.get("RUNS_ON_STICKYDISK_UNAVAILABLE_FILE")
    if unavailable and os.path.exists(unavailable):
        append_summary(f"### Sticky disk `{name}`\n\nThe disk was not available, so this job ran without its caches.\n")
        return 0
    if not disk_dir or not os.path.ismount(disk_dir):
        print("::warning title=Sticky disk report::This job has no sticky disk mounted; give its runs-on label a sticky= disk or remove the action.")
        return 0

    measurement = measure(disk_dir)
    record_path = Path(disk_dir) / RECORD_NAME
    record, record_note = load_record(record_path)
    mount_points = cache_mount_points(parse_mountinfo(Path("/proc/self/mountinfo").read_text()), disk_dir)
    now = datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    write_note = write_record(
        record_path,
        Record(
            written_at=now,
            run_url=run_url(),
            job=os.environ.get("GITHUB_JOB", "job"),
            disk_used_bytes=measurement.disk.used_bytes,
            directories=measurement.directories,
        ),
    )
    notes = [record_note] if record_note else []
    notes += measurement.notes
    notes += [write_note] if write_note else []
    append_summary(render_report(name, measurement, record, notes, mount_points, os.environ.get("HOME", "")))
    return 0


if __name__ == "__main__":
    sys.exit(main())
