#!/usr/bin/env python3
"""Evict the coldest cache entries until a share of the runner's disk is free.

The self-hosted boxes are persistent: every cache a job leaves under ~/.cache
is still there for the next one, and since the move off ephemeral runners
nothing evicts any of it. Several repositories share each box, and each keeps
container images, cargo target directories and package downloads that only
ever grow, so the disk fills and a later job dies partway through with no
space left on device.

Everything under ~/.cache is, by that directory's definition, regenerable, so
this keeps a share of the filesystem free by deleting the least recently used
entries until the floor is met. Losing one costs the run that wanted it a cold
rebuild; losing the disk costs every job on the box until someone clears it by
hand. When the floor is already met this walks nothing and deletes nothing.

Eviction is deliberately not scoped to the repository that runs it. The box is
shared, its disk is shared, and the entry that has gone longest unread is the
cheapest one to lose whichever repository wrote it. Each repository ships this
same file, so whichever job runs next is the one that makes room.

Run it at the start of a job, before the steps that write: the pass a job
makes on the way in is what leaves room for it, and also what clears whatever
the job before it left behind. It reports and never fails, housekeeping being
no reason to redden a suite; a floor it could not reach is a warning naming
the box, because that is a box to look at rather than a run to rerun.

    python3 prune_ci_cache.py [--cache DIR] [--min-free-percent N]
"""

import argparse
import os
from pathlib import Path, PurePosixPath
import shutil
import stat
import subprocess
import sys
import time
from collections.abc import Callable, Iterator
from dataclasses import dataclass


# Directories whose children are the cache entries, so an eviction takes one
# image or one suite's target directory rather than a whole repository's
# cache. Matched against the path relative to the cache directory, at exactly
# the depth written here; anything the list does not name is an entry whole.
#
# The list describes every CI cache any Peppy-bot repository keeps on these
# boxes, not only the one shipping this copy, because eviction spans the
# directory. A pattern matching nothing on a given box costs nothing.
SPLIT_PATTERNS = (
    "peppy-ci",               # one directory per cargo suite and month
    "*-ci-*",                 # one per cache a hub repository keeps
    "*-ci-*/target",          # one per workspace's cargo target directory
    "*-ci-*/test-images",     # one per image derived from a node's def
    "*-ci-*/built_nodes",     # one per node the daemon has built
    "*-ci-*/built_nodes/*",   # one per fingerprint that node was built at
)


@dataclass(frozen=True)
class Entry:
    """A cache entry: what one eviction step deletes, sized and dated."""

    path: Path
    disk_bytes: int
    last_used: float


def splits(relative: PurePosixPath) -> bool:
    """Whether this path's children are entries rather than the path itself."""
    return any(
        len(relative.parts) == len(PurePosixPath(pattern).parts)
        and relative.match(pattern)
        for pattern in SPLIT_PATTERNS
    )


def eviction_entries(cache: Path) -> list[Path]:
    """Every path eviction may delete, at the granularity of one cache entry.

    A split directory with nothing in it is an entry of its own, so an empty
    shell left by earlier evictions goes too.
    """
    entries = []
    pending = list(cache.iterdir())
    while pending:
        path = pending.pop()
        relative = PurePosixPath(path.relative_to(cache).as_posix())
        children = []
        if splits(relative) and path.is_dir() and not path.is_symlink():
            # A directory that cannot be listed is an entry whole, and
            # removing it is where the escalation below gets its chance.
            try:
                children = list(path.iterdir())
            except OSError:
                children = []
        if children:
            pending.extend(children)
        else:
            entries.append(path)
    return entries


def statuses(path: Path) -> Iterator[os.stat_result]:
    """lstat of the path and of everything below it, each file exactly once.

    Symlinks are stated, never followed: a link into another filesystem is
    metadata this entry owns, and the tree it points at is not.
    """
    try:
        top = path.lstat()
    except OSError:
        return
    yield top
    if not stat.S_ISDIR(top.st_mode):
        return
    for directory, directories, filenames in os.walk(path, onerror=lambda _: None):
        for name in (*directories, *filenames):
            try:
                yield os.lstat(os.path.join(directory, name))
            except OSError:
                continue


def measure(path: Path) -> Entry:
    """Disk the entry occupies and the most recent use of anything inside it.

    Sizes come from the allocated block count, which is what `du` reports and
    what freeing the entry actually returns; a file carrying more than one
    link is counted once, since uv and apptainer link their caches together
    heavily. Both atime and mtime count as a use: a filesystem mounted
    `noatime` never advances the first, every cache here is written as well as
    read, so the newer of the two is the signal that survives either mount.
    """
    linked: set[tuple[int, int]] = set()
    disk_bytes = 0
    last_used = 0.0
    for status in statuses(path):
        if status.st_nlink > 1:
            identity = (status.st_dev, status.st_ino)
            if identity in linked:
                continue
            linked.add(identity)
        disk_bytes += status.st_blocks * 512
        last_used = max(last_used, status.st_atime, status.st_mtime)
    return Entry(path, disk_bytes, last_used)


def remove_entry(path: Path) -> bool:
    """Delete one cache entry and report whether it is gone.

    A tree apptainer built inside a user namespace belongs to a mapped subuid
    this user cannot unlink from outside that namespace, and the box's
    passwordless sudo is what clears it. Nothing here raises: an entry that
    survives is one the caller reports and steps over.
    """
    try:
        if path.is_dir() and not path.is_symlink():
            shutil.rmtree(path)
        else:
            path.unlink()
    except OSError:
        subprocess.run(
            ["sudo", "-n", "rm", "-rf", str(path)],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False,
        )
    return not os.path.lexists(path)


def humanize_size(count: float) -> str:
    """A byte count in the largest unit that keeps it above one."""
    size = float(count)
    for unit in ("B", "KiB", "MiB", "GiB"):
        if size < 1024:
            return f"{size:.1f} {unit}"
        size /= 1024
    return f"{size:.1f} TiB"


def humanize_age(seconds: float) -> str:
    """How long ago an entry was last used, in the coarsest useful unit."""
    seconds = max(seconds, 0.0)
    if seconds < 3600:
        return f"{seconds / 60:.0f}m"
    if seconds < 86400:
        return f"{seconds / 3600:.0f}h"
    return f"{seconds / 86400:.0f}d"


def reclaim(
    cache: Path,
    min_free_percent: float,
    *,
    disk_usage: Callable[[Path], tuple[int, int, int]] = shutil.disk_usage,
    remove: Callable[[Path], bool] = remove_entry,
    now: Callable[[], float] = time.time,
) -> bool:
    """Evict until the floor is met, and report whether the box reached it."""
    if not cache.is_dir():
        print(f"{cache} does not exist, so there is nothing to reclaim.")
        return True

    total, _, free = disk_usage(cache)
    if total == 0:
        print(f"::warning::{cache}'s filesystem reports no capacity; skipping.")
        return True

    floor = int(total * min_free_percent / 100)
    print(
        f"{cache} is on a filesystem of {humanize_size(total)} with "
        f"{humanize_size(free)} free ({100 * free / total:.1f}%). "
        f"The floor is {humanize_size(floor)} ({min_free_percent:g}%)."
    )
    if free >= floor:
        print("At or above the floor, so nothing is evicted.")
        return True

    # Measuring walks every cache entry on the box, which is why it waits
    # until the floor is breached instead of running on the way past.
    print(f"::group::Evicting the least recently used entries under {cache}")
    entries = sorted(
        (measure(path) for path in eviction_entries(cache)),
        key=lambda entry: entry.last_used,
    )
    moment = now()
    for entry in entries:
        if free >= floor:
            break
        if remove(entry.path):
            print(
                f"  {humanize_size(entry.disk_bytes):>10}  "
                f"unused {humanize_age(moment - entry.last_used):>4}  "
                f"{entry.path.relative_to(cache)}"
            )
        else:
            print(f"::warning::{entry.path} could not be removed and still holds its disk.")
        total, _, free = disk_usage(cache)
    print("::endgroup::")

    print(f"{humanize_size(free)} free ({100 * free / total:.1f}%) after eviction.")
    if free >= floor:
        return True
    print(
        f"::warning title=Runner disk below the floor on {os.environ.get('RUNNER_NAME', 'this runner')}"
        f"::Every entry under {cache} was evicted and the filesystem is still under "
        f"{min_free_percent:g}% free. The disk is being filled by something outside that "
        f"directory, so this box needs looking at rather than this run rerunning."
    )
    return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(
        description="Evict the coldest cache entries until a share of the disk is free.",
    )
    parser.add_argument(
        "--cache", type=Path, default=Path.home() / ".cache",
        help="the directory holding the caches to evict from (default: ~/.cache)",
    )
    parser.add_argument(
        "--min-free-percent", type=float, default=25.0,
        help=(
            "the share of the filesystem to leave free (default: 25). It has to "
            "exceed what one job writes, the largest measured being a 40 GiB cargo "
            "target directory, or the job that follows this one fills the disk again."
        ),
    )
    arguments = parser.parse_args(argv)
    if not 0 < arguments.min_free_percent < 100:
        parser.error("--min-free-percent must name a share between 0 and 100")

    # Housekeeping never reddens a suite: a floor this could not reach is
    # reported as a warning against the box, and so is a filesystem that
    # refused this pass outright, since a job whose caches went untended still
    # has its own result to report. Anything but an OSError is a defect in
    # this file rather than a state of the box, and is left to surface.
    try:
        reclaim(arguments.cache, arguments.min_free_percent)
    except OSError as error:
        print(
            f"::warning title=Cache reclaim failed on "
            f"{os.environ.get('RUNNER_NAME', 'this runner')}::{error}"
        )
    return 0


if __name__ == "__main__":
    sys.exit(main())
