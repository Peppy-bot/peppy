#!/usr/bin/env python3
"""What `report.py` reads from a sticky disk, and the report it writes.

The cases give the script the mount table, the du output and the record the
way a RunsOn runner has them, so they run anywhere and never depend on the
host's disks or clock. One case runs the real du on a directory tree it lays
out, and checks only which directories it reports.
"""

import contextlib
import io
import os
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import report

DISK = "/mnt/runs-on/stickydisk"
HOME = "/home/runner"
NODES_CACHE = "home-runner-.cache-nodes-hub-ci-0a1b2c3d"
UV_CACHE = "home-runner-.cache-uv-11111111"
GONE_CACHE = "home-runner-.cache-old-22222222"

MOUNTINFO = "\n".join(
    [
        "22 1 259:1 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p1 rw",
        "120 22 259:3 / /mnt/runs-on/stickydisk rw,relatime shared:60 - ext4 /dev/nvme1n1 rw",
        f"121 22 259:3 /mounts/{NODES_CACHE} /home/runner/.cache/nodes-hub-ci rw,relatime shared:60 - ext4 /dev/nvme1n1 rw",
        f"122 22 259:3 /mounts/{UV_CACHE} /home/runner/.cache/uv rw,relatime shared:60 - ext4 /dev/nvme1n1 rw",
        # The same layout on another file system is not this disk.
        "123 22 259:1 /mounts/home-runner-elsewhere-33333333 /home/runner/elsewhere rw - ext4 /dev/nvme0n1p1 rw",
        # A directory of the disk mounted outside `mounts/` is not a cache.
        "124 22 259:3 /scratch /home/runner/scratch rw - ext4 /dev/nvme1n1 rw",
    ]
)

GIB = 1 << 30


def record(directories, disk_used_bytes=50 * GIB):
    return report.Record(
        written_at="2026-10-01 14:02 UTC",
        run_url="https://github.com/Peppy-bot/nodes-hub/actions/runs/1/attempts/1",
        job="discover-and-run",
        disk_used_bytes=disk_used_bytes,
        directories=directories,
    )


def measurement(directories, used=60 * GIB, total=200 * GIB, notes=()):
    return report.Measurement(
        disk=report.DiskUsage(total_bytes=total, used_bytes=used),
        directories=directories,
        notes=list(notes),
    )


class MountTableTest(unittest.TestCase):
    def test_each_cache_is_named_by_the_path_it_is_mounted_on(self):
        points = report.cache_mount_points(report.parse_mountinfo(MOUNTINFO), DISK)
        self.assertEqual(
            points,
            {
                NODES_CACHE: "/home/runner/.cache/nodes-hub-ci",
                UV_CACHE: "/home/runner/.cache/uv",
            },
        )

    def test_a_job_without_the_disk_mounted_has_no_cache(self):
        mounts = report.parse_mountinfo(MOUNTINFO)
        self.assertEqual(report.cache_mount_points(mounts, "/mnt/other"), {})

    def test_escaped_characters_of_a_path_are_restored(self):
        line = "121 22 259:3 /mounts/a\\040b /home/runner/my\\040cache\\134x rw - ext4 /dev/nvme1n1 rw"
        (mount,) = report.parse_mountinfo(line)
        self.assertEqual(mount.root, "/mounts/a b")
        self.assertEqual(mount.mount_point, "/home/runner/my cache\\x")

    def test_a_backslash_not_followed_by_three_octal_digits_stays(self):
        self.assertEqual(report.unescape_mountinfo("a\\09b\\12"), "a\\09b\\12")


class DuTest(unittest.TestCase):
    def test_sizes_are_keyed_by_their_path_below_the_caches(self):
        output = "\n".join(
            [
                f"1000\t{DISK}/mounts/a/x/y",
                f"3000\t{DISK}/mounts/a/x",
                f"5000\t{DISK}/mounts/a",
                f"7000\t{DISK}/mounts",
                f"9000\t{DISK}/elsewhere",
                "not a du line",
            ]
        )
        self.assertEqual(
            report.parse_du(output, f"{DISK}/mounts"),
            {"a/x/y": 1000, "a/x": 3000, "a": 5000},
        )

    def test_the_real_du_reports_three_levels_below_the_caches(self):
        with tempfile.TemporaryDirectory() as disk:
            deep = Path(disk, "mounts", "cache", "child", "grandchild", "below")
            deep.mkdir(parents=True)
            (deep / "file").write_bytes(b"x" * 10000)
            result = report.measure(disk)
        self.assertEqual(set(result.directories), {"cache", "cache/child", "cache/child/grandchild"})
        self.assertEqual(result.notes, [])

    def test_a_disk_without_caches_reports_no_directory(self):
        with tempfile.TemporaryDirectory() as disk:
            self.assertEqual(report.measure(disk).directories, {})


class RecordTest(unittest.TestCase):
    def test_a_record_reads_back_as_written(self):
        written = record({"a": 1, "a/b": 2})
        with tempfile.TemporaryDirectory() as disk:
            path = Path(disk, report.RECORD_NAME)
            self.assertIsNone(report.write_record(path, written))
            self.assertEqual(report.load_record(path), (written, None))
            self.assertFalse(path.with_name(path.name + ".partial").exists())

    def test_a_disk_without_a_record_has_no_note(self):
        with tempfile.TemporaryDirectory() as disk:
            self.assertEqual(report.load_record(Path(disk, report.RECORD_NAME)), (None, None))

    def test_an_unusable_record_is_ignored_with_a_note(self):
        cases = {
            "not json": "{",
            "another format": '{"version": 99}',
            "a missing field": '{"version": 1, "written_at": "x"}',
            "a wrong type": '{"version": 1, "written_at": "x", "run_url": "u", "job": "j", "disk_used_bytes": 1, "directories": []}',
        }
        for case, text in cases.items():
            with self.subTest(case=case), tempfile.TemporaryDirectory() as disk:
                path = Path(disk, report.RECORD_NAME)
                path.write_text(text)
                loaded, note = report.load_record(path)
                self.assertIsNone(loaded)
                self.assertIn("record of the previous job", note)

    def test_a_record_that_cannot_be_written_becomes_a_note(self):
        with tempfile.TemporaryDirectory() as disk:
            note = report.write_record(Path(disk, "missing", report.RECORD_NAME), record({}))
        self.assertIn("could not be written", note)


class FormatTest(unittest.TestCase):
    def test_sizes_use_the_largest_unit_that_fits(self):
        cases = {
            0: "0 B",
            1023: "1023 B",
            1024: "1.0 KiB",
            (1 << 20) * 3 // 2: "1.5 MiB",
            GIB * 3 // 2: "1.5 GiB",
        }
        for size, text in cases.items():
            with self.subTest(size=size):
                self.assertEqual(report.format_bytes(size), text)

    def test_a_change_is_signed_and_a_missing_size_is_new(self):
        self.assertEqual(report.format_change(None, 5), "new")
        self.assertEqual(report.format_change(5, 5), "0")
        self.assertEqual(report.format_change(GIB, 2 * GIB), "+1.0 GiB")
        self.assertEqual(report.format_change(2 * GIB, GIB), "-1.0 GiB")

    def test_a_directory_is_named_by_its_mount_point(self):
        points = {NODES_CACHE: "/home/runner/.cache/nodes-hub-ci", "opt": "/opt/cache", "near": "/home/runner2/x"}
        cases = {
            NODES_CACHE: "~/.cache/nodes-hub-ci",
            f"{NODES_CACHE}/target": "~/.cache/nodes-hub-ci/target",
            "opt/sub": "/opt/cache/sub",
            # A path that only starts with the same letters as home is not in it.
            "near": "/home/runner2/x",
            f"{GONE_CACHE}/sub": f"mounts/{GONE_CACHE}/sub (not mounted by this job)",
        }
        for key, label in cases.items():
            with self.subTest(key=key):
                self.assertEqual(report.label_directory(key, points, HOME), label)


class RenderTest(unittest.TestCase):
    points = {NODES_CACHE: "/home/runner/.cache/nodes-hub-ci"}

    def test_the_report_compares_each_directory_with_the_restore(self):
        now = measurement(
            {
                NODES_CACHE: 60 * GIB,
                f"{NODES_CACHE}/target": 30 * GIB,
                f"{NODES_CACHE}/test-images": 20 * GIB,
                f"{NODES_CACHE}/uv": 10 * GIB,
                GONE_CACHE: 1 * GIB,
            }
        )
        before = record(
            {
                NODES_CACHE: 50 * GIB,
                f"{NODES_CACHE}/target": 30 * GIB,
                f"{NODES_CACHE}/test-images": 18 * GIB,
                GONE_CACHE: 1 * GIB,
            }
        )
        self.assertEqual(
            report.render_report("node-tests", now, before, ["A note."], self.points, HOME),
            "\n".join(
                [
                    "### Sticky disk `node-tests`",
                    "",
                    "| | |",
                    "|---|---|",
                    "| Restored | From the snapshot left by [discover-and-run](https://github.com/Peppy-bot/nodes-hub/actions/runs/1/attempts/1) at 2026-10-01 14:02 UTC |",
                    "| Used | 60.0 GiB of 200.0 GiB (30%), +10.0 GiB since the restore |",
                    "",
                    "#### Caches",
                    "",
                    "| Directory | At restore | Now | Change |",
                    "|---|---:|---:|---:|",
                    "| `~/.cache/nodes-hub-ci` | 50.0 GiB | 60.0 GiB | +10.0 GiB |",
                    f"| `mounts/{GONE_CACHE} (not mounted by this job)` | 1.0 GiB | 1.0 GiB | 0 |",
                    "",
                    "#### Largest directories in the caches (up to 10)",
                    "",
                    "| Directory | At restore | Now | Change |",
                    "|---|---:|---:|---:|",
                    "| `~/.cache/nodes-hub-ci/target` | 30.0 GiB | 30.0 GiB | 0 |",
                    "| `~/.cache/nodes-hub-ci/test-images` | 18.0 GiB | 20.0 GiB | +2.0 GiB |",
                    "| `~/.cache/nodes-hub-ci/uv` | - | 10.0 GiB | new |",
                    "",
                    "> A note.",
                    "",
                ]
            ),
        )

    def test_without_a_record_only_the_current_sizes_show(self):
        now = measurement({NODES_CACHE: 2 * GIB, f"{NODES_CACHE}/target": 2 * GIB}, used=2 * GIB)
        text = report.render_report("node-tests", now, None, [], self.points, HOME)
        self.assertIn("| Restored | No record of an earlier job:", text)
        self.assertIn("| Used | 2.0 GiB of 200.0 GiB (1%) |", text)
        self.assertIn("| `~/.cache/nodes-hub-ci` | - | 2.0 GiB | - |", text)
        self.assertIn("| `~/.cache/nodes-hub-ci/target` | - | 2.0 GiB | - |", text)

    def test_a_disk_without_caches_says_so(self):
        text = report.render_report("node-tests", measurement({}), None, [], {}, HOME)
        self.assertIn("No cache directory on the disk.", text)
        self.assertNotIn("#### Caches", text)

    def test_the_largest_directories_are_the_ten_biggest_inside_the_caches(self):
        directories = {NODES_CACHE: 100 * GIB}
        directories.update({f"{NODES_CACHE}/d{i:02}": i * GIB for i in range(1, 13)})
        # Two of the same size are listed by name.
        directories[f"{NODES_CACHE}/a-tie"] = 12 * GIB
        rows = report.largest_rows(measurement(directories), None, self.points, HOME)
        self.assertEqual(
            [row.label.rsplit("/", 1)[1] for row in rows],
            ["a-tie", "d12", "d11", "d10", "d09", "d08", "d07", "d06", "d05", "d04"],
        )


class MainTest(unittest.TestCase):
    def run_main(self, environment):
        out = io.StringIO()
        with tempfile.TemporaryDirectory() as directory:
            summary = Path(directory, "summary.md")
            summary.touch()
            environment = {"GITHUB_STEP_SUMMARY": str(summary), **environment}
            with mock.patch.dict(os.environ, environment, clear=True), contextlib.redirect_stdout(out):
                code = report.main()
            return code, out.getvalue(), summary.read_text()

    def test_a_disk_that_was_not_available_is_reported(self):
        with tempfile.TemporaryDirectory() as directory:
            unavailable = Path(directory, "unavailable")
            unavailable.touch()
            code, _, summary = self.run_main(
                {
                    "RUNS_ON_STICKYDISK_NAME": "node-tests",
                    "RUNS_ON_STICKYDISK_UNAVAILABLE_FILE": str(unavailable),
                    "RUNS_ON_STICKYDISK_DIR": DISK,
                }
            )
        self.assertEqual(code, 0)
        self.assertIn("The disk was not available", summary)

    def test_a_job_without_a_sticky_disk_gets_a_warning_and_succeeds(self):
        for environment in ({}, {"RUNS_ON_STICKYDISK_DIR": "/nonexistent/stickydisk"}):
            with self.subTest(environment=environment):
                code, out, summary = self.run_main(environment)
                self.assertEqual(code, 0)
                self.assertIn("::warning title=Sticky disk report::", out)
                self.assertEqual(summary, "")


if __name__ == "__main__":
    unittest.main()
