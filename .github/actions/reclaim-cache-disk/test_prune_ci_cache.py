import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

import prune_ci_cache as janitor


KIBIBYTE = 1024
NOW = 1_800_000_000.0


def entry_named(cache: Path, name: str, *, kibibytes: int = 4, days_old: float = 0.0) -> Path:
    """A cache entry of a known size, last used a known number of days ago."""
    path = cache / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"\0" * kibibytes * KIBIBYTE)
    age(path, days_old)
    return path


def age(path: Path, days_old: float) -> None:
    """Date a path, and everything under it, deepest first so a parent keeps
    the time set here rather than the one writing its children gave it."""
    moment = NOW - days_old * 86400
    for child in sorted(path.rglob("*"), key=lambda entry: len(entry.parts), reverse=True):
        os.utime(child, (moment, moment))
    os.utime(path, (moment, moment))


class FakeFilesystem:
    """A filesystem whose free space is what neither the cache nor anything
    outside it is using, so removing a file really does free the disk."""

    def __init__(self, cache: Path, total: int, outside: int = 0):
        self.cache = cache
        self.total = total
        self.outside = outside

    def usage(self, _path) -> tuple[int, int, int]:
        cached = sum(
            child.stat().st_size
            for child in self.cache.rglob("*")
            if child.is_file() and not child.is_symlink()
        )
        used = self.outside + cached
        return (self.total, used, self.total - used)


class CacheDirectory(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="prune-ci-cache-")
        self.addCleanup(self.directory.cleanup)
        self.cache = Path(self.directory.name)

    def reclaim(self, filesystem, *, min_free_percent=50.0, remove=janitor.remove_entry):
        """Run a reclaim pass against a fake filesystem, returning its verdict
        and everything it printed."""
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            reached = janitor.reclaim(
                self.cache, min_free_percent,
                disk_usage=filesystem.usage, remove=remove, now=lambda: NOW,
            )
        return reached, output.getvalue()

    def relative_entries(self) -> set[str]:
        return {
            path.relative_to(self.cache).as_posix()
            for path in janitor.eviction_entries(self.cache)
        }

    def surviving(self) -> set[str]:
        return {path.name for path in self.cache.iterdir()}


class EvictionEntries(CacheDirectory):
    def test_a_repository_cache_splits_into_the_entries_it_holds(self):
        entry_named(self.cache, "nodes-hub-ci-X64/test-images/a.sif")
        entry_named(self.cache, "nodes-hub-ci-X64/test-images/b.sif")
        entry_named(self.cache, "nodes-hub-ci-X64/target/lerobot_recorder/libnode.rlib")
        entry_named(self.cache, "nodes-hub-ci-X64/cargo-home/registry/crate.crate")
        entry_named(self.cache, "peppy-ci/workspace-tests-X64-2026-09/target/peppy")
        entry_named(self.cache, "uv/wheels/peppylib.whl")

        self.assertEqual(
            self.relative_entries(),
            {
                "nodes-hub-ci-X64/test-images/a.sif",
                "nodes-hub-ci-X64/test-images/b.sif",
                "nodes-hub-ci-X64/target/lerobot_recorder",
                "nodes-hub-ci-X64/cargo-home",
                "peppy-ci/workspace-tests-X64-2026-09",
                "uv",
            },
        )

    def test_a_node_built_twice_holds_one_entry_per_fingerprint(self):
        # A node's sources fingerprint to a new image on every change and the
        # one it replaces stays beside it, so the stale half has to be
        # evictable without the image the next launch will reuse going too.
        built = "launchers-hub-ci-X64/built_nodes/mujoco_v1"
        entry_named(self.cache, f"{built}/0123456789abcdef.sif")
        entry_named(self.cache, f"{built}/fedcba9876543210.sif")

        self.assertEqual(
            self.relative_entries(),
            {f"{built}/0123456789abcdef.sif", f"{built}/fedcba9876543210.sif"},
        )

    def test_a_directory_no_pattern_names_is_one_entry(self):
        entry_named(self.cache, "rattler/pkgs/python-3.13/bin/python")

        self.assertEqual(self.relative_entries(), {"rattler"})

    def test_an_empty_split_directory_is_an_entry_of_its_own(self):
        (self.cache / "launchers-hub-ci-X64/built_nodes").mkdir(parents=True)

        self.assertEqual(self.relative_entries(), {"launchers-hub-ci-X64/built_nodes"})

    @unittest.skipIf(os.geteuid() == 0, "root reads a directory whatever its mode")
    def test_a_split_directory_that_cannot_be_listed_is_one_entry(self):
        images = self.cache / "nodes-hub-ci-X64/test-images"
        entry_named(self.cache, "nodes-hub-ci-X64/test-images/a.sif")
        images.chmod(0o000)
        self.addCleanup(images.chmod, 0o700)

        self.assertEqual(self.relative_entries(), {"nodes-hub-ci-X64/test-images"})

    def test_a_symlinked_cache_is_never_descended_into(self):
        entry_named(self.cache, "elsewhere/nodes-hub-ci-X64/test-images/a.sif")
        (self.cache / "nodes-hub-ci-X64").symlink_to(self.cache / "elsewhere/nodes-hub-ci-X64")

        self.assertIn("nodes-hub-ci-X64", self.relative_entries())


class Measure(CacheDirectory):
    def test_a_hard_linked_file_is_counted_once(self):
        linked = entry_named(self.cache, "uv/wheels/peppylib.whl", kibibytes=64)
        os.link(linked, self.cache / "uv/wheels/peppylib-again.whl")

        self.assertLess(janitor.measure(self.cache / "uv").disk_bytes, 2 * 64 * KIBIBYTE)

    def test_the_newest_use_anywhere_in_a_tree_dates_the_entry(self):
        entry_named(self.cache, "uv/wheels/old.whl", days_old=10)
        entry_named(self.cache, "uv/wheels/recent.whl", days_old=2)
        os.utime(self.cache / "uv", (NOW - 10 * 86400, NOW - 10 * 86400))

        self.assertAlmostEqual(
            janitor.measure(self.cache / "uv").last_used, NOW - 2 * 86400, places=2
        )

    def test_a_file_only_ever_read_is_dated_by_that_read(self):
        read = entry_named(self.cache, "rattler", days_old=30)
        os.utime(read, (NOW - 86400, NOW - 30 * 86400))

        self.assertAlmostEqual(
            janitor.measure(read).last_used, NOW - 86400, places=2
        )


class Reclaim(CacheDirectory):
    def test_nothing_is_evicted_while_the_floor_is_met(self):
        entry_named(self.cache, "uv", kibibytes=4)
        filesystem = FakeFilesystem(self.cache, total=40 * KIBIBYTE)

        reached, report = self.reclaim(filesystem)

        self.assertTrue(reached)
        self.assertEqual(self.surviving(), {"uv"})
        self.assertIn("nothing is evicted", report)

    def test_the_coldest_entries_go_first_and_eviction_stops_at_the_floor(self):
        for index in range(8):
            entry_named(self.cache, f"cache-{index}", kibibytes=4, days_old=7 - index)
        filesystem = FakeFilesystem(self.cache, total=40 * KIBIBYTE)

        reached, _ = self.reclaim(filesystem)

        self.assertTrue(reached)
        self.assertEqual(
            self.surviving(), {f"cache-{index}" for index in range(3, 8)}
        )

    def test_an_entry_that_cannot_be_removed_is_reported_and_stepped_over(self):
        for index in range(8):
            entry_named(self.cache, f"cache-{index}", kibibytes=4, days_old=7 - index)
        filesystem = FakeFilesystem(self.cache, total=40 * KIBIBYTE)

        def refuse_the_coldest(path: Path) -> bool:
            return False if path.name == "cache-0" else janitor.remove_entry(path)

        reached, report = self.reclaim(filesystem, remove=refuse_the_coldest)

        self.assertTrue(reached)
        self.assertEqual(
            self.surviving(), {"cache-0", *(f"cache-{index}" for index in range(4, 8))}
        )
        self.assertIn("cache-0 could not be removed", report)

    def test_a_disk_filled_from_outside_the_cache_warns_against_the_box(self):
        entry_named(self.cache, "uv", kibibytes=4)
        filesystem = FakeFilesystem(self.cache, total=40 * KIBIBYTE, outside=36 * KIBIBYTE)

        reached, report = self.reclaim(filesystem)

        self.assertFalse(reached)
        self.assertEqual(self.surviving(), set())
        self.assertIn("::warning title=Runner disk below the floor", report)

    def test_a_filesystem_reporting_no_capacity_is_left_alone(self):
        entry_named(self.cache, "uv", kibibytes=4)
        filesystem = FakeFilesystem(self.cache, total=0)

        reached, report = self.reclaim(filesystem)

        self.assertTrue(reached)
        self.assertEqual(self.surviving(), {"uv"})
        self.assertIn("no capacity", report)

    def test_a_cache_directory_that_does_not_exist_is_not_an_error(self):
        missing = self.cache / "never-created"
        output = io.StringIO()
        with contextlib.redirect_stdout(output):
            reached = janitor.reclaim(missing, 25.0)

        self.assertTrue(reached)
        self.assertIn("nothing to reclaim", output.getvalue())


class RemoveEntry(CacheDirectory):
    def test_a_tree_is_removed_whole(self):
        entry_named(self.cache, "uv/wheels/peppylib.whl")

        self.assertTrue(janitor.remove_entry(self.cache / "uv"))
        self.assertEqual(self.surviving(), set())

    def test_a_dangling_symlink_is_removed_rather_than_followed(self):
        link = self.cache / "nodes-hub-ci-X64"
        link.symlink_to(self.cache / "never-created")

        self.assertTrue(janitor.remove_entry(link))
        self.assertEqual(self.surviving(), set())


class Reporting(unittest.TestCase):
    def test_a_size_is_printed_in_the_largest_unit_holding_it(self):
        self.assertEqual(janitor.humanize_size(512), "512.0 B")
        self.assertEqual(janitor.humanize_size(4 * KIBIBYTE), "4.0 KiB")
        self.assertEqual(janitor.humanize_size(40 * KIBIBYTE**3), "40.0 GiB")
        self.assertEqual(janitor.humanize_size(2 * KIBIBYTE**4), "2.0 TiB")

    def test_an_age_is_printed_in_the_coarsest_unit_holding_it(self):
        self.assertEqual(janitor.humanize_age(90), "2m")
        self.assertEqual(janitor.humanize_age(7200), "2h")
        self.assertEqual(janitor.humanize_age(6 * 86400), "6d")

    def test_a_use_dated_in_the_future_reads_as_no_age_at_all(self):
        self.assertEqual(janitor.humanize_age(-3600), "0m")


class CommandLine(unittest.TestCase):
    def test_a_floor_outside_the_range_is_refused(self):
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaises(SystemExit):
                janitor.main(["--min-free-percent", "120"])

    def test_a_filesystem_that_refuses_the_pass_leaves_the_job_green(self):
        def refuse(*_arguments, **_keywords):
            raise OSError("permission denied")

        output = io.StringIO()
        with patch.object(janitor, "reclaim", refuse):
            with contextlib.redirect_stdout(output):
                status = janitor.main([])

        self.assertEqual(status, 0)
        self.assertIn("::warning title=Cache reclaim failed", output.getvalue())

    def test_a_cache_directory_that_does_not_exist_leaves_the_job_green(self):
        with tempfile.TemporaryDirectory() as directory:
            output = io.StringIO()
            with contextlib.redirect_stdout(output):
                status = janitor.main(["--cache", f"{directory}/never-created"])

        self.assertEqual(status, 0)


if __name__ == "__main__":
    unittest.main()
