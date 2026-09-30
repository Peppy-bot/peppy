#!/usr/bin/env python3
"""Keeps a cargo target dir to the artifacts its builds still use.

Cargo never removes an artifact. A unit whose inputs change (a dependency
version, a feature, a profile setting) is built again under a new hash, and
the old one stays beside it. A change that reaches a crate every test binary
links builds all of them again, so on the sticky disk a target dir that
nothing sweeps grows by a full set of test binaries each time, until a link
has no room left to write its output.

Every unit cargo builds is named by a hash of 16 hex digits: its outputs in
`deps/` or `examples/`, the directories of its build script in `build/`, and
its fingerprint in `.fingerprint/` all end in `-<hash>`. The build lists, in
its JSON messages, the outputs of every unit it used, fresh or rebuilt. The
hashes in those paths are the live units, and an entry with any other hash
is left over from an earlier build.

Cargo also puts a copy of each binary and example it builds outside the unit
directories (`debug/<bin>`, `debug/examples/<example>`) and lists only that
copy. The copy is a hardlink to the output of its unit, so the unit is the
one whose output has the same inode.

Three commands, each on the paths as the host sees them:

    record <target dir> <listing> <build root>
        Reads the JSON messages of a `cargo test --no-run` into the target
        dir (<build root> is that dir as the build saw it), writes the live
        units to the record, and removes every other unit. After a build
        that did not finish, the record keeps the units it already listed
        as well, because the next build can use what either one built.

    sweep <target dir>
        Removes every unit the record does not list, which cleans up after
        a run that stopped before its `record`. A target dir without a
        record is cleared: nothing in it is known to be live.

    prune <target root> <live dir>...
        Removes every entry of <target root> that is not a live dir, inside
        one, or a directory on the path to one. The live dirs are the
        target dirs of the projects the repository holds, relative to the
        root. `record` and `sweep` reach only the target dirs a run builds,
        so without `prune` the target dir of a deleted project stays.

A listed output this script cannot attribute to a unit means cargo uses a
layout the script does not know. `record` then deletes the record and prints
a warning, so the next `sweep` clears the dir: each build starts cold, and
the dir stays bounded.
"""

from __future__ import annotations

from collections.abc import Iterable
from dataclasses import dataclass
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import sys


# `cargo test` builds with the `test` profile, which inherits `dev` and so
# builds into `debug/`. CI builds for the host only, so no target triple
# comes between the target dir and the profile dir.
PROFILE_DIR = "debug"
# The record, in the target dir itself, which cargo does not write to.
RECORD_NAME = "live-units.json"
# The directories of the profile dir whose entries are named by unit.
UNIT_DIRS = ("deps", "build", ".fingerprint", "examples")
# The directories the build lists outputs in; `.fingerprint` is cargo's own.
OUTPUT_DIRS = ("deps", "build", "examples")
UNIT_HASH = re.compile(r"-([0-9a-f]{16})(?=\.|$)")
# The live dir of `prune` that is the target root itself.
ROOT = PurePosixPath(".")


class UnknownLayout(Exception):
    """A listed output that the sweep cannot attribute to a unit."""


@dataclass(frozen=True)
class BuildListing:
    """The outputs a build listed, relative to its target dir."""

    finished: bool
    outputs: tuple[PurePosixPath, ...]


@dataclass(frozen=True)
class LiveUnits:
    """What the builds of a target dir still use.

    `copies` are the copies of binaries and examples outside the unit
    directories, relative to the profile dir.
    """

    hashes: frozenset[str]
    copies: frozenset[str]

    def union(self, other: LiveUnits) -> LiveUnits:
        return LiveUnits(self.hashes | other.hashes, self.copies | other.copies)

    def keeps_unit_entry(self, name: str) -> bool:
        unit_hash = hash_in_name(name)
        return unit_hash is not None and unit_hash in self.hashes

    def keeps_copy(self, path: str) -> bool:
        # Cargo writes the dep-info of each copy beside it.
        return path in self.copies or path.removesuffix(".d") in self.copies


@dataclass(frozen=True)
class Removal:
    """How many entries a sweep removed, and their size in bytes."""

    entries: int
    size: int

    def __add__(self, other: Removal) -> Removal:
        return Removal(self.entries + other.entries, self.size + other.size)


NOTHING_REMOVED = Removal(0, 0)


def hash_in_name(name: str) -> str | None:
    match = UNIT_HASH.search(name)
    return match.group(1) if match else None


def read_listing(lines: Iterable[str], build_root: PurePosixPath) -> BuildListing:
    """Parses the JSON messages of a cargo build.

    Lines that are not JSON objects are skipped: a proc macro or a build tool
    that prints to stdout puts its lines between cargo's messages.
    """
    finished = False
    outputs: list[PurePosixPath] = []
    for line in lines:
        try:
            message = json.loads(line)
        except json.JSONDecodeError:
            continue
        if not isinstance(message, dict):
            continue
        reason = message.get("reason")
        if reason == "compiler-artifact":
            paths = message["filenames"]
        elif reason == "build-script-executed":
            paths = [message["out_dir"]]
        elif reason == "build-finished":
            finished = message["success"] is True
            continue
        else:
            continue
        outputs.extend(relative_to_build_root(path, build_root) for path in paths)
    return BuildListing(finished, tuple(outputs))


def relative_to_build_root(path: str, build_root: PurePosixPath) -> PurePosixPath:
    try:
        return PurePosixPath(path).relative_to(build_root)
    except ValueError:
        raise UnknownLayout(f"{path} is outside the build's target dir {build_root}") from None


def live_units(target_dir: Path, outputs: Iterable[PurePosixPath]) -> LiveUnits:
    """Attributes each listed output to the unit that built it."""
    profile_dir = target_dir / PROFILE_DIR
    hashes: set[str] = set()
    copies: set[str] = set()
    inode_indexes: dict[str, dict[tuple[int, int], str]] = {}
    for output in outputs:
        parts = output.parts
        if len(parts) < 2 or parts[0] != PROFILE_DIR:
            raise UnknownLayout(f"{output} is outside the profile dir {PROFILE_DIR}/")
        inner = parts[1:]
        if len(inner) >= 2 and inner[0] in OUTPUT_DIRS:
            unit_hash = hash_in_name(inner[1])
            if unit_hash is not None:
                hashes.add(unit_hash)
                continue
            if inner[0] == "examples" and len(inner) == 2:
                copy, unit_dir = f"examples/{inner[1]}", "examples"
            else:
                raise UnknownLayout(f"{output} names no unit hash")
        elif len(inner) == 1:
            copy, unit_dir = inner[0], "deps"
        else:
            raise UnknownLayout(f"{output} is in no directory cargo lists outputs in")
        if unit_dir not in inode_indexes:
            inode_indexes[unit_dir] = inode_index(profile_dir / unit_dir)
        unit_hash = inode_indexes[unit_dir].get(inode_of(profile_dir / copy))
        if unit_hash is None:
            raise UnknownLayout(f"{output} is a hardlink to the output of no unit in {unit_dir}/")
        hashes.add(unit_hash)
        copies.add(copy)
    return LiveUnits(frozenset(hashes), frozenset(copies))


def inode_of(path: Path) -> tuple[int, int]:
    status = path.stat(follow_symlinks=False)
    return status.st_dev, status.st_ino


def inode_index(directory: Path) -> dict[tuple[int, int], str]:
    """Maps the inode of each unit output in a directory to its unit hash."""
    index = {}
    with os.scandir(directory) as entries:
        for entry in entries:
            unit_hash = hash_in_name(entry.name)
            if unit_hash is not None and entry.is_file(follow_symlinks=False):
                status = entry.stat(follow_symlinks=False)
                index[(status.st_dev, status.st_ino)] = unit_hash
    return index


def read_record(target_dir: Path) -> LiveUnits | None:
    record = target_dir / RECORD_NAME
    if not record.exists():
        return None
    content = json.loads(record.read_text())
    return LiveUnits(frozenset(content["hashes"]), frozenset(content["copies"]))


def write_record(target_dir: Path, live: LiveUnits) -> None:
    """Replaces the record in one rename, so a run that stops halfway through
    leaves the previous record, never a partial one."""
    record = target_dir / RECORD_NAME
    partial = record.with_name(f"{RECORD_NAME}.partial")
    partial.write_text(
        json.dumps({"hashes": sorted(live.hashes), "copies": sorted(live.copies)}, indent=1) + "\n"
    )
    os.replace(partial, record)


def remove(path: Path) -> Removal:
    size = size_of(path)
    if path.is_dir() and not path.is_symlink():
        shutil.rmtree(path)
    else:
        path.unlink()
    return Removal(1, size)


def size_of(path: Path) -> int:
    if not path.is_dir() or path.is_symlink():
        return path.lstat().st_size
    total = 0
    for directory, _, files in os.walk(path):
        for name in files:
            total += os.lstat(os.path.join(directory, name)).st_size
    return total


def children(directory: Path) -> list[Path]:
    if not directory.is_dir():
        return []
    return sorted(directory.iterdir())


def clear(target_dir: Path) -> Removal:
    removed = NOTHING_REMOVED
    for entry in children(target_dir):
        removed += remove(entry)
    return removed


def sweep_to(target_dir: Path, live: LiveUnits) -> Removal:
    """Removes every unit entry and copy of the profile dir that `live` does
    not keep.

    The profile dir's other entries stay: cargo's lock files, and
    `incremental/`, whose entries are not named by unit (CI builds with
    incremental compilation off).
    """
    profile_dir = target_dir / PROFILE_DIR
    removed = NOTHING_REMOVED
    for unit_dir in UNIT_DIRS:
        for entry in children(profile_dir / unit_dir):
            copy = f"{unit_dir}/{entry.name}"
            if not live.keeps_unit_entry(entry.name) and not live.keeps_copy(copy):
                removed += remove(entry)
    for entry in children(profile_dir):
        is_copy = entry.is_file() and not entry.is_symlink() and not entry.name.startswith(".")
        if is_copy and not live.keeps_copy(entry.name):
            removed += remove(entry)
    return removed


def sweep(target_dir: Path) -> Removal:
    live = read_record(target_dir)
    if live is None:
        return clear(target_dir)
    return sweep_to(target_dir, live)


def record(target_dir: Path, listing_path: Path, build_root: PurePosixPath) -> Removal:
    with listing_path.open() as lines:
        listing = read_listing(lines, build_root)
    try:
        built = live_units(target_dir, listing.outputs)
    except UnknownLayout as layout:
        # The tests about to run use what this build left, so the dir stays
        # until the `sweep` of the next run, which clears it for want of a
        # record.
        print(
            f"::warning::cannot tell which units {target_dir} still uses ({layout}); "
            "the next run clears it and builds it cold"
        )
        (target_dir / RECORD_NAME).unlink(missing_ok=True)
        return NOTHING_REMOVED
    previous = read_record(target_dir)
    live = built if listing.finished or previous is None else previous.union(built)
    write_record(target_dir, live)
    return sweep_to(target_dir, live)


def parse_live_dir(argument: str) -> PurePosixPath:
    """A live dir of `prune`: a path under the target root."""
    live_dir = PurePosixPath(argument)
    if not argument or live_dir.is_absolute() or ".." in live_dir.parts:
        raise ValueError(f"live dir {argument!r} is not a path under the target root")
    return live_dir


def prune(target_root: Path, live_dirs: Iterable[PurePosixPath]) -> Removal:
    """Removes every entry of the target root that is not a live dir, inside
    one, or a directory on the path to one.

    A live dir inside another live dir stays with it. A symlink is removed,
    never followed, even where a directory on the path to a live dir would be.
    """
    live = frozenset(live_dirs)
    if ROOT in live:
        return NOTHING_REMOVED
    on_the_path = frozenset(parent for live_dir in live for parent in live_dir.parents)
    removed = NOTHING_REMOVED
    pending = [target_root]
    while pending:
        directory = pending.pop()
        for entry in children(directory):
            relative = PurePosixPath(entry.relative_to(target_root).as_posix())
            if relative in live:
                continue
            if relative in on_the_path and entry.is_dir() and not entry.is_symlink():
                pending.append(entry)
                continue
            removed += remove(entry)
    return removed


def describe(removed: Removal, directory: Path) -> str:
    return f"removed {removed.entries} stale entries ({removed.size / 2**30:.1f} GiB) from {directory}"


USAGE = """\
usage: sweep-cargo-target.py sweep <target dir>
       sweep-cargo-target.py record <target dir> <listing> <build root>
       sweep-cargo-target.py prune <target root> <live dir>..."""


def main(arguments: list[str]) -> int:
    match arguments:
        case ["sweep", target_dir]:
            removed = sweep(Path(target_dir))
        case ["record", target_dir, listing, build_root]:
            removed = record(Path(target_dir), Path(listing), PurePosixPath(build_root))
        case ["prune", target_root, *live_dir_arguments]:
            try:
                live_dirs = [parse_live_dir(argument) for argument in live_dir_arguments]
            except ValueError as invalid:
                print(f"{invalid}\n{USAGE}", file=sys.stderr)
                return 2
            removed = prune(Path(target_root), live_dirs)
        case _:
            print(USAGE, file=sys.stderr)
            return 2
    print(describe(removed, Path(arguments[1])))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
