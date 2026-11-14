"""What `sweep-cargo-target.py` keeps in a target dir or a target root, and what
it removes.

Each case lays out a target dir the way cargo does: every unit in the
directory `build/<package>/<hash>/`, and the copies of binaries and examples
hardlinked to their unit's output.
"""

import contextlib
import importlib.util
import io
import json
import os
import sys
import tempfile
import unittest
from pathlib import Path, PurePosixPath


def load_sweep():
    """Import the script beside this file.

    Scripts the workflow runs are named with hyphens, which no import statement
    can spell, so the module is loaded by path. It is registered before it
    runs, because its dataclasses look their module up while they are built.
    """
    path = Path(__file__).with_name("sweep-cargo-target.py")
    spec = importlib.util.spec_from_file_location("sweep_cargo_target", path)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


sweep = load_sweep()

# The target dir as the build in the container sees it.
BUILD_ROOT = PurePosixPath("/target")

OLD_SERDE = "1111111111111111"
NEW_SERDE = "2222222222222222"
OLD_BUILD_SCRIPT = "3333333333333333"
OLD_BUILD_SCRIPT_RUN = "4444444444444444"
NEW_BUILD_SCRIPT = "5555555555555555"
NEW_BUILD_SCRIPT_RUN = "6666666666666666"
OLD_TOOL = "7777777777777777"
NEW_TOOL = "8888888888888888"
OLD_EXAMPLE = "9999999999999999"
NEW_EXAMPLE = "aaaaaaaaaaaaaaaa"
TEST_BINARY = "bbbbbbbbbbbbbbbb"

# The package of the workspace that the binary, the example and the test
# binary belong to.
WORKSPACE_PACKAGE = "ws"


def unit_dir(package, unit_hash):
    """The directory of a unit, relative to the profile dir."""
    return f"build/{package}/{unit_hash}"


def artifact(*outputs):
    return json.dumps(
        {"reason": "compiler-artifact", "fresh": True, "filenames": list(outputs)}
    )


def build_script_run(out_dir):
    return json.dumps({"reason": "build-script-executed", "out_dir": out_dir})


def build_finished(success):
    return json.dumps({"reason": "build-finished", "success": success})


class TargetDir:
    """A target dir on disk, with the units and copies cargo leaves in it.

    Each method lays out one unit and returns the JSON message the build
    lists it with.
    """

    def __init__(self, root):
        self.root = root
        self.profile = root / "debug"

    def write(self, relative, content="output"):
        path = self.profile / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content)
        return path

    def listed(self, relative):
        return str(BUILD_ROOT / "debug" / relative)

    def library(self, name, unit_hash):
        unit = unit_dir(name, unit_hash)
        self.write(f"{unit}/out/lib{name}-{unit_hash}.rlib")
        self.write(f"{unit}/out/lib{name}-{unit_hash}.rmeta")
        self.write(f"{unit}/out/{name}-{unit_hash}.d")
        self.write(f"{unit}/fingerprint/lib-{name}")
        return artifact(
            self.listed(f"{unit}/out/lib{name}-{unit_hash}.rlib"),
            self.listed(f"{unit}/out/lib{name}-{unit_hash}.rmeta"),
        )

    def build_script(self, package, compile_hash, run_hash):
        compiled = unit_dir(package, compile_hash)
        run = unit_dir(package, run_hash)
        self.write(f"{compiled}/out/build_script_build")
        self.write(f"{compiled}/fingerprint/build-script-build-script-build")
        self.write(f"{run}/out/generated.rs")
        self.write(f"{run}/run/stdout")
        self.write(f"{run}/fingerprint/run-build-script-build-script-build")
        return [
            artifact(self.listed(f"{compiled}/out/build_script_build")),
            build_script_run(self.listed(f"{run}/out")),
        ]

    def test_binary(self, name, unit_hash):
        unit = unit_dir(WORKSPACE_PACKAGE, unit_hash)
        self.write(f"{unit}/out/{name}-{unit_hash}")
        self.write(f"{unit}/out/{name}-{unit_hash}.d")
        self.write(f"{unit}/fingerprint/test-integration-test-{name}")
        return artifact(self.listed(f"{unit}/out/{name}-{unit_hash}"))

    def binary(self, name, unit_hash):
        """A binary, whose copy in the profile dir is the one cargo lists."""
        unit = unit_dir(WORKSPACE_PACKAGE, unit_hash)
        output = self.write(f"{unit}/out/{name}", content=f"{name} {unit_hash}")
        self.write(f"{unit}/out/{name}.d")
        self.write(f"{unit}/fingerprint/bin-{name}")
        self.link_copy(output, self.profile / name)
        self.write(f"{name}.d")
        return artifact(self.listed(name))

    def example(self, name, unit_hash):
        """An example, whose copy in `examples/` is the one cargo lists."""
        unit = unit_dir(WORKSPACE_PACKAGE, unit_hash)
        output = self.write(f"{unit}/out/{name}", content=f"{name} {unit_hash}")
        self.write(f"{unit}/out/{name}.d")
        self.write(f"{unit}/fingerprint/example-{name}")
        self.link_copy(output, self.profile / "examples" / name)
        self.write(f"examples/{name}.d")
        return artifact(self.listed(f"examples/{name}"))

    @staticmethod
    def link_copy(output, copy):
        """Cargo replaces the copy of a rebuilt unit, as this does."""
        copy.parent.mkdir(parents=True, exist_ok=True)
        copy.unlink(missing_ok=True)
        os.link(output, copy)

    def entries(self):
        """Every unit and copy, the way the sweep sees them: each unit by its
        directory, a package directory that holds no unit by its own name,
        each copy by its path, and each other entry of the profile dir that
        is not hidden by its name."""
        found = set()
        for entry in self.profile.iterdir():
            if entry.name.startswith("."):
                continue
            if entry.name == sweep.UNITS_DIR:
                found.update(self.unit_entries(entry))
            elif entry.name == sweep.EXAMPLES_DIR:
                found.update(f"{entry.name}/{copy.name}" for copy in entry.iterdir())
            elif entry.name not in sweep.CARGO_DIRS:
                found.add(entry.name)
        return found

    @staticmethod
    def unit_entries(units):
        for package_dir in units.iterdir():
            if not package_dir.is_dir():
                yield f"{units.name}/{package_dir.name}"
                continue
            unit_names = [unit.name for unit in package_dir.iterdir()]
            if not unit_names:
                yield f"{units.name}/{package_dir.name}/"
            for unit_name in unit_names:
                yield f"{units.name}/{package_dir.name}/{unit_name}"

    def listing(self, messages, name="listing.jsonl"):
        path = self.root.parent / name
        path.write_text("\n".join(messages) + "\n")
        return path


# Records the sweep cannot read: cut short, without a field, of another shape.
UNREADABLE_RECORDS = ("{", '{"hashes": []}', "[]")


def old_build(target):
    """Lays out the units of a build before serde changed, and lists them."""
    return [
        target.library("serde", OLD_SERDE),
        *target.build_script("serde", OLD_BUILD_SCRIPT, OLD_BUILD_SCRIPT_RUN),
        target.binary("tool", OLD_TOOL),
        target.example("demo", OLD_EXAMPLE),
        target.test_binary("it", TEST_BINARY),
        build_finished(True),
    ]


def new_build(target):
    """Lays out the units of the same build after serde changed, which
    rebuilt all but the test binary, and lists them."""
    return [
        target.library("serde", NEW_SERDE),
        *target.build_script("serde", NEW_BUILD_SCRIPT, NEW_BUILD_SCRIPT_RUN),
        target.binary("tool", NEW_TOOL),
        target.example("demo", NEW_EXAMPLE),
        target.test_binary("it", TEST_BINARY),
        build_finished(True),
    ]


class SweepTestCase(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.target = self.target_dir("target")

    def target_dir(self, name):
        target = TargetDir(self.scratch / name)
        target.profile.mkdir(parents=True)
        return target

    def record(self, messages):
        with contextlib.redirect_stdout(io.StringIO()) as output:
            removed = sweep.record(
                self.target.root, self.target.listing(messages), BUILD_ROOT
            )
        self.output = output.getvalue()
        return removed

    def recorded(self):
        return sweep.read_record(self.target.root)

    def outside_directory(self):
        """A directory outside the target dir, with a file in a directory and
        a hidden file, and the two files."""
        outside = self.scratch / "outside"
        files = (outside / "kept" / "file", outside / ".hidden")
        for file in files:
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text("outside")
        return outside, files

    def assertNotFollowed(self, symlink, files):
        """The sweep removed `symlink` and none of `files`, which it leads to."""
        self.assertFalse(symlink.is_symlink(), symlink)
        for file in files:
            self.assertTrue(file.is_file(), file)


class ReadListingTest(unittest.TestCase):
    def test_lists_unit_outputs_and_build_script_outputs_under_the_build_root(self):
        listing = sweep.read_listing(
            [
                artifact(
                    "/target/debug/build/serde/1111111111111111/out/libserde-1111111111111111.rlib"
                ),
                build_script_run("/target/debug/build/serde/4444444444444444/out"),
                build_finished(True),
            ],
            BUILD_ROOT,
        )
        self.assertTrue(listing.finished)
        self.assertEqual(
            listing.outputs,
            (
                PurePosixPath(
                    "debug/build/serde/1111111111111111/out/libserde-1111111111111111.rlib"
                ),
                PurePosixPath("debug/build/serde/4444444444444444/out"),
            ),
        )

    def test_a_build_that_listed_no_end_is_unfinished(self):
        listing = sweep.read_listing(
            [
                artifact(
                    "/target/debug/build/serde/1111111111111111/out/libserde-1111111111111111.rlib"
                )
            ],
            BUILD_ROOT,
        )
        self.assertFalse(listing.finished)

    def test_a_failed_build_is_unfinished(self):
        self.assertFalse(
            sweep.read_listing([build_finished(False)], BUILD_ROOT).finished
        )

    def test_skips_what_is_not_a_cargo_message(self):
        listing = sweep.read_listing(
            [
                "a proc macro printing to stdout",
                "[1, 2]",
                json.dumps({"reason": "compiler-message", "message": {}}),
                build_finished(True),
            ],
            BUILD_ROOT,
        )
        self.assertEqual(listing, sweep.BuildListing(finished=True, outputs=()))

    def test_an_output_outside_the_build_root_is_an_unknown_layout(self):
        with self.assertRaises(sweep.UnknownLayout):
            sweep.read_listing(
                [
                    artifact(
                        "/elsewhere/debug/build/serde/1111111111111111/out/libserde-1111111111111111.rlib"
                    )
                ],
                BUILD_ROOT,
            )


class LiveUnitsTest(SweepTestCase):
    def live(self, messages):
        listing = sweep.read_listing(messages, BUILD_ROOT)
        return sweep.live_units(self.target.root, listing.outputs)

    def test_attributes_each_output_to_the_unit_directory_in_its_path(self):
        live = self.live(
            [
                self.target.library("serde", OLD_SERDE),
                *self.target.build_script(
                    "serde", OLD_BUILD_SCRIPT, OLD_BUILD_SCRIPT_RUN
                ),
                self.target.test_binary("it", TEST_BINARY),
            ]
        )
        self.assertEqual(
            live.hashes,
            {OLD_SERDE, OLD_BUILD_SCRIPT, OLD_BUILD_SCRIPT_RUN, TEST_BINARY},
        )
        self.assertEqual(live.copies, set())

    def test_attributes_a_copy_to_the_unit_output_it_is_a_hardlink_to(self):
        self.target.binary("tool", OLD_TOOL)
        self.target.example("demo", OLD_EXAMPLE)
        live = self.live(
            [
                self.target.binary("tool", NEW_TOOL),
                self.target.example("demo", NEW_EXAMPLE),
            ]
        )
        self.assertEqual(live.hashes, {NEW_TOOL, NEW_EXAMPLE})
        self.assertEqual(live.copies, {"tool", "examples/demo"})

    def test_a_copy_that_is_no_hardlink_is_an_unknown_layout(self):
        self.target.write(
            f"{unit_dir(WORKSPACE_PACKAGE, OLD_TOOL)}/out/tool", content="tool"
        )
        self.target.write("tool", content="tool")
        with self.assertRaisesRegex(sweep.UnknownLayout, "hardlink"):
            self.live([artifact("/target/debug/tool")])

    def test_an_output_in_the_output_directory_of_no_unit_is_an_unknown_layout(self):
        for output in (
            "/target/debug/build/ws/libmember.so",
            "/target/debug/build/ws/not-a-unit-hash/out/libmember.so",
            f"/target/debug/build/ws/{OLD_TOOL}/fingerprint/bin-tool",
            f"/target/debug/build/ws/{OLD_TOOL}",
        ):
            with (
                self.subTest(output=output),
                self.assertRaisesRegex(
                    sweep.UnknownLayout, "output directory of no unit"
                ),
            ):
                self.live([artifact(output)])

    def test_an_output_outside_the_profile_dir_is_an_unknown_layout(self):
        with self.assertRaisesRegex(sweep.UnknownLayout, "profile dir"):
            self.live(
                [
                    artifact(
                        "/target/release/build/serde/1111111111111111/out/libserde-1111111111111111.rlib"
                    )
                ]
            )

    def test_an_output_in_no_output_dir_is_an_unknown_layout(self):
        with self.assertRaisesRegex(sweep.UnknownLayout, "no directory"):
            self.live(
                [artifact("/target/debug/incremental/serde-1111111111111111/s-x")]
            )


class SweepTest(SweepTestCase):
    def test_without_a_record_clears_the_target_dir(self):
        old_build(self.target)
        (self.target.root / "tmp").mkdir()
        removed = sweep.sweep(self.target.root)
        self.assertEqual(list(self.target.root.iterdir()), [])
        self.assertEqual(removed.entries, 2)

    def test_an_unreadable_record_clears_the_target_dir(self):
        for content in UNREADABLE_RECORDS:
            with self.subTest(record=content):
                old_build(self.target)
                (self.target.root / sweep.RECORD_NAME).write_text(content)
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    sweep.sweep(self.target.root)
                self.assertEqual(list(self.target.root.iterdir()), [])
                self.assertIn("::warning::", output.getvalue())

    def test_a_record_it_cannot_open_clears_the_target_dir(self):
        old_build(self.target)
        (self.target.root / sweep.RECORD_NAME).mkdir()
        with contextlib.redirect_stdout(io.StringIO()) as output:
            sweep.sweep(self.target.root)
        self.assertEqual(list(self.target.root.iterdir()), [])
        self.assertIn("::warning::", output.getvalue())

    def test_removes_every_unit_and_copy_the_record_does_not_list(self):
        old_build(self.target)
        sweep.write_record(
            self.target.root,
            sweep.LiveUnits(frozenset({OLD_SERDE, TEST_BINARY}), frozenset()),
        )
        sweep.sweep(self.target.root)
        self.assertEqual(
            self.target.entries(),
            {
                unit_dir("serde", OLD_SERDE),
                unit_dir(WORKSPACE_PACKAGE, TEST_BINARY),
            },
        )

    def test_removes_the_directory_of_a_package_that_keeps_no_unit(self):
        old_build(self.target)
        sweep.write_record(
            self.target.root, sweep.LiveUnits(frozenset({TEST_BINARY}), frozenset())
        )
        sweep.sweep(self.target.root)
        self.assertEqual(
            self.target.entries(), {unit_dir(WORKSPACE_PACKAGE, TEST_BINARY)}
        )

    def test_removes_what_the_units_directory_holds_besides_the_units(self):
        self.target.library("serde", OLD_SERDE)
        sweep.write_record(
            self.target.root, sweep.LiveUnits(frozenset({OLD_SERDE}), frozenset())
        )
        self.target.write("build/serde/rustcAbC123/symbols.o")
        self.target.write("build/stray.o")
        outside = self.target.root / "outside"
        outside.mkdir()
        (self.target.profile / "build" / "linked").symlink_to(outside)
        sweep.sweep(self.target.root)
        self.assertEqual(self.target.entries(), {unit_dir("serde", OLD_SERDE)})
        self.assertTrue(outside.is_dir())

    def test_keeps_the_copies_the_record_lists_and_their_dep_info(self):
        old_build(self.target)
        sweep.write_record(
            self.target.root,
            sweep.LiveUnits(
                frozenset({OLD_TOOL, OLD_EXAMPLE}), frozenset({"tool", "examples/demo"})
            ),
        )
        sweep.sweep(self.target.root)
        self.assertEqual(
            self.target.entries(),
            {
                unit_dir(WORKSPACE_PACKAGE, OLD_TOOL),
                "tool",
                "tool.d",
                unit_dir(WORKSPACE_PACKAGE, OLD_EXAMPLE),
                "examples/demo",
                "examples/demo.d",
            },
        )

    def test_removes_a_directory_of_the_profile_dir_that_cargo_does_not_write(self):
        self.target.library("serde", OLD_SERDE)
        sweep.write_record(
            self.target.root, sweep.LiveUnits(frozenset({OLD_SERDE}), frozenset())
        )
        self.target.write("native/libmember.a")
        self.target.write(".hidden/leftover")
        sweep.sweep(self.target.root)
        self.assertEqual(self.target.entries(), {unit_dir("serde", OLD_SERDE)})
        self.assertFalse((self.target.profile / ".hidden").exists())

    def test_removes_a_profile_dir_that_is_a_symlink_without_following_it(self):
        outside, files = self.outside_directory()
        self.target.profile.rmdir()
        self.target.profile.symlink_to(outside)
        sweep.write_record(self.target.root, sweep.LiveUnits(frozenset(), frozenset()))
        removed = sweep.sweep(self.target.root)
        self.assertEqual(removed.entries, 1)
        self.assertNotFollowed(self.target.profile, files)

    def test_removes_a_cargo_directory_that_is_a_symlink_without_following_it(self):
        outside, files = self.outside_directory()
        for cargo_dir in (sweep.UNITS_DIR, sweep.EXAMPLES_DIR, "incremental"):
            with self.subTest(cargo_dir=cargo_dir):
                symlink = self.target.profile / cargo_dir
                symlink.symlink_to(outside)
                sweep.write_record(
                    self.target.root, sweep.LiveUnits(frozenset(), frozenset())
                )
                removed = sweep.sweep(self.target.root)
                self.assertEqual(removed.entries, 1)
                self.assertNotFollowed(symlink, files)

    def test_the_command_follows_a_target_dir_that_is_a_symlink(self):
        old_build(self.target)
        given = self.scratch / "given"
        given.symlink_to(self.target.root)
        with contextlib.redirect_stdout(io.StringIO()):
            status = sweep.main(["sweep", str(given)])
        self.assertEqual(status, 0)
        self.assertTrue(given.is_symlink())
        self.assertEqual(list(self.target.root.iterdir()), [])

    def test_leaves_the_locks_and_what_is_not_named_by_unit(self):
        old_build(self.target)
        sweep.write_record(self.target.root, sweep.LiveUnits(frozenset(), frozenset()))
        cargo_lock = self.target.write(".cargo-lock")
        incremental = self.target.write(
            "incremental/serde-0123456789abcdef/s-x/query-cache.bin"
        )
        scratch = self.target.root / "tmp" / "scratch"
        scratch.parent.mkdir()
        scratch.write_text("scratch")
        rustc_info = self.target.root / ".rustc_info.json"
        rustc_info.write_text("{}")
        sweep.sweep(self.target.root)
        self.assertEqual(self.target.entries(), set())
        for path in (
            cargo_lock,
            incremental,
            scratch,
            rustc_info,
            self.target.root / sweep.RECORD_NAME,
        ):
            self.assertTrue(path.exists(), path)


class RecordTest(SweepTestCase):
    def test_after_a_finished_build_keeps_exactly_the_units_it_used(self):
        self.record(old_build(self.target))
        removed = self.record(new_build(self.target))

        self.assertEqual(self.output, "")
        self.assertEqual(
            self.recorded(),
            sweep.LiveUnits(
                frozenset(
                    {
                        NEW_SERDE,
                        NEW_BUILD_SCRIPT,
                        NEW_BUILD_SCRIPT_RUN,
                        NEW_TOOL,
                        NEW_EXAMPLE,
                        TEST_BINARY,
                    }
                ),
                frozenset({"tool", "examples/demo"}),
            ),
        )
        # A target dir the new build alone filled holds the same entries.
        fresh = self.target_dir("fresh")
        new_build(fresh)
        self.assertEqual(self.target.entries(), fresh.entries())
        # The unit of serde, the two units of its build script, the unit of
        # the binary and the unit of the example.
        self.assertEqual(removed.entries, 1 + 2 + 1 + 1)

    def test_after_an_unfinished_build_keeps_the_units_the_record_listed_too(self):
        self.record(old_build(self.target))
        self.record([self.target.library("serde", NEW_SERDE)])
        self.assertEqual(
            self.recorded().hashes,
            {
                OLD_SERDE,
                OLD_BUILD_SCRIPT,
                OLD_BUILD_SCRIPT_RUN,
                OLD_TOOL,
                OLD_EXAMPLE,
                TEST_BINARY,
                NEW_SERDE,
            },
        )
        self.assertIn(unit_dir("serde", OLD_SERDE), self.target.entries())
        self.assertIn(unit_dir("serde", NEW_SERDE), self.target.entries())

    def test_after_an_unfinished_first_build_records_what_it_built(self):
        self.record([self.target.library("serde", NEW_SERDE)])
        self.assertEqual(
            self.recorded(), sweep.LiveUnits(frozenset({NEW_SERDE}), frozenset())
        )

    def test_an_unknown_layout_drops_the_record_and_removes_nothing(self):
        self.record(old_build(self.target))
        self.target.write("build/ws/libmember.so")
        before = self.target.entries()
        removed = self.record(
            [*old_build(self.target), artifact("/target/debug/build/ws/libmember.so")]
        )
        self.assertEqual(removed, sweep.NOTHING_REMOVED)
        self.assertEqual(self.target.entries(), before)
        self.assertIsNone(self.recorded())
        self.assertIn("::warning::", self.output)

    def test_a_cargo_directory_that_is_a_symlink_is_an_unknown_layout(self):
        outside, files = self.outside_directory()
        for cargo_dir in (".", sweep.UNITS_DIR, sweep.EXAMPLES_DIR):
            with self.subTest(cargo_dir=cargo_dir):
                target = self.target_dir(f"target-{len(cargo_dir)}")
                sweep.write_record(
                    target.root, sweep.LiveUnits(frozenset(), frozenset())
                )
                symlink = target.profile / cargo_dir
                if cargo_dir == ".":
                    symlink = target.profile
                    symlink.rmdir()
                symlink.symlink_to(outside)
                with contextlib.redirect_stdout(io.StringIO()) as output:
                    removed = sweep.record(
                        target.root, target.listing([build_finished(True)]), BUILD_ROOT
                    )
                self.assertEqual(removed, sweep.NOTHING_REMOVED)
                self.assertTrue(symlink.is_symlink())
                self.assertIsNone(sweep.read_record(target.root))
                self.assertIn("::warning::", output.getvalue())
                for file in files:
                    self.assertTrue(file.is_file(), file)

    def test_replaces_an_unreadable_record_with_the_units_the_build_used(self):
        for content in UNREADABLE_RECORDS:
            with self.subTest(record=content):
                (self.target.root / sweep.RECORD_NAME).write_text(content)
                self.record(
                    [self.target.library("serde", NEW_SERDE), build_finished(False)]
                )
                self.assertEqual(
                    self.recorded(),
                    sweep.LiveUnits(frozenset({NEW_SERDE}), frozenset()),
                )
                self.assertIn("::warning::", self.output)

    def test_replaces_the_record_in_place(self):
        self.record(old_build(self.target))
        self.record(new_build(self.target))
        self.assertEqual(
            sorted(path.name for path in self.target.root.iterdir()),
            ["debug", sweep.RECORD_NAME],
        )


# The output of a unit, relative to the profile dir, that marks a target dir
# a prune must leave whole.
SERDE_OUTPUT = f"{unit_dir('serde', OLD_SERDE)}/out/libserde-{OLD_SERDE}.rlib"


class PruneTest(unittest.TestCase):
    """A target root that holds the target dir of each project at the path of
    the project."""

    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.scratch = Path(scratch.name)
        self.root = self.scratch / "target"

    def project_target(self, project):
        target = TargetDir(self.root / project)
        target.write(SERDE_OUTPUT)
        return target.root

    def prune(self, *live_dirs):
        return sweep.prune(
            self.root, [PurePosixPath(live_dir) for live_dir in live_dirs]
        )

    def assertIntact(self, target_dir):
        self.assertTrue(
            (target_dir / "debug" / SERDE_OUTPUT).is_file(),
            target_dir,
        )

    def test_removes_the_target_dir_of_a_project_that_is_gone(self):
        arm = self.project_target("openarm/arm")
        sim_arm = self.project_target("openarm/sim_arm")
        zed = self.project_target("zed_camera")
        removed = self.prune("openarm/arm", "zed_camera")
        self.assertIntact(arm)
        self.assertIntact(zed)
        self.assertFalse(sim_arm.exists())
        self.assertEqual(removed.entries, 1)

    def test_removes_a_directory_that_leads_to_no_live_dir(self):
        self.project_target("old/deep/project")
        arm = self.project_target("example_robot/my_robot_arm/my_rust_robot_arm")
        removed = self.prune("example_robot/my_robot_arm/my_rust_robot_arm")
        self.assertEqual(list(self.root.iterdir()), [self.root / "example_robot"])
        self.assertIntact(arm)
        self.assertEqual(removed.entries, 1)

    def test_keeps_a_live_dir_inside_another_live_dir(self):
        node = self.project_target("node")
        member = self.project_target("node/member")
        removed = self.prune("node", "node/member")
        self.assertIntact(node)
        self.assertIntact(member)
        self.assertEqual(removed, sweep.NOTHING_REMOVED)

    def test_removes_what_a_directory_on_the_path_holds_besides_the_path(self):
        node = self.project_target("node")
        member = self.project_target("node/member")
        self.prune("node/member")
        self.assertIntact(member)
        self.assertEqual(list(node.iterdir()), [member])

    def test_removes_a_file_beside_the_target_dirs(self):
        kept = self.project_target("kept")
        (self.root / "stray.log").write_text("stray")
        self.prune("kept")
        self.assertEqual(list(self.root.iterdir()), [kept])

    def test_removes_a_symlink_on_the_path_without_following_it(self):
        outside = self.scratch / "outside"
        outside_target = TargetDir(outside / "member")
        outside_target.write(SERDE_OUTPUT)
        self.root.mkdir()
        (self.root / "node").symlink_to(outside)
        self.prune("node/member")
        self.assertEqual(list(self.root.iterdir()), [])
        self.assertIntact(outside_target.root)

    def test_the_command_follows_a_target_root_that_is_a_symlink(self):
        kept = self.project_target("a")
        self.project_target("b")
        given = self.scratch / "given"
        given.symlink_to(self.root)
        with contextlib.redirect_stdout(io.StringIO()):
            status = sweep.main(["prune", str(given), "a"])
        self.assertEqual(status, 0)
        self.assertTrue(given.is_symlink())
        self.assertEqual(list(self.root.iterdir()), [kept])

    def test_without_live_dirs_removes_every_target_dir(self):
        self.project_target("a")
        self.project_target("b/c")
        removed = self.prune()
        self.assertEqual(list(self.root.iterdir()), [])
        self.assertEqual(removed.entries, 2)

    def test_the_root_as_a_live_dir_keeps_everything(self):
        target = self.project_target("a")
        self.assertEqual(self.prune("."), sweep.NOTHING_REMOVED)
        self.assertIntact(target)

    def test_a_missing_target_root_removes_nothing(self):
        self.assertEqual(self.prune("a"), sweep.NOTHING_REMOVED)

    def test_the_command_takes_the_live_dirs_relative_to_the_root(self):
        kept = self.project_target("openarm/arm")
        self.project_target("openarm/sim_arm")
        with contextlib.redirect_stdout(io.StringIO()) as output:
            status = sweep.main(["prune", str(self.root), "openarm/arm/"])
        self.assertEqual(status, 0)
        self.assertEqual(list((self.root / "openarm").iterdir()), [kept])
        self.assertIn("removed 1 stale entries", output.getvalue())

    def test_the_command_refuses_a_live_dir_outside_the_root_and_removes_nothing(self):
        target = self.project_target("a")
        for live_dir in ("", "/target/a", "../target/a", "a/../../target/a"):
            with (
                self.subTest(live_dir=live_dir),
                contextlib.redirect_stderr(io.StringIO()) as errors,
            ):
                self.assertEqual(
                    sweep.main(["prune", str(self.root), "a", live_dir]), 2
                )
                self.assertIn("not a path under the target root", errors.getvalue())
        self.assertIntact(target)


class MainTest(unittest.TestCase):
    def test_refuses_an_unknown_command(self):
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(sweep.main(["clean", "/target"]), 2)


if __name__ == "__main__":
    unittest.main()
