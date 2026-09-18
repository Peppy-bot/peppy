#!/usr/bin/env python3
"""Tests for the test-input detection.

The detection decides which suites a pull request runs, so a defect here is
wasted compute at best and a silently skipped suite at worst. These cases run
against this repository's real manifests: what they assert is what the
selection has to do about the layout, never a second copy of the layout. The
changes job of tests.yml runs them before it trusts what the detection says.
"""

import os
import unittest

import detect

CRATES, LIBRARIES = detect.tree_members()


def select(*files):
    """The selection for a change set naming `files`."""
    return detect.select(list(files), CRATES, LIBRARIES)


def cargo_packages(selection):
    """The peppy-shared packages a selection names, without the `-p` flags."""
    return [
        word
        for word in selection["public_libs_shared_packages"].split()
        if word != "-p"
    ]


def selected(selection):
    """Every output of a selection that names something to run."""
    return {
        gate: value
        for gate, value in selection.items()
        if value not in ("", "false")
    }


def peppy_shared_directories():
    """Directory of every package of the peppy-shared workspace, by name."""
    manifest = os.path.join(detect.ROOT, detect.PEPPY_SHARED, "Cargo.toml")
    directories, _, manifests = detect.crate_graph([manifest])
    return {
        name: directories[package]
        for name, package in manifests.items()
        if directories[package].startswith(detect.PEPPY_SHARED + "/")
    }


def path_dependencies():
    """(crate directory, dependency directory) for every path dependency
    between two standalone crates of the tree."""
    manifests = [
        os.path.join(detect.ROOT, detect.TREE, crate, "Cargo.toml") for crate in CRATES
    ]
    directories, dependencies, _ = detect.crate_graph(manifests)
    return [
        (directories[manifest], directories[dependency])
        for manifest in manifests
        for dependency in dependencies[manifest]
        if dependency in directories
    ]


class Detection(unittest.TestCase):
    def test_a_change_outside_every_suite_runs_nothing(self):
        self.assertEqual(selected(select("Readme.md")), {})

    def test_every_standalone_crate_is_selected_by_its_own_change(self):
        for crate in CRATES:
            with self.subTest(crate=crate):
                selection = select("%s/%s/src/lib.rs" % (detect.TREE, crate))
                self.assertIn(crate, selection["public_libs_crates"].split())

    def test_every_python_library_is_selected_by_its_own_change(self):
        for library in LIBRARIES:
            with self.subTest(library=library):
                selection = select("%s/%s/src/thing.py" % (detect.TREE, library))
                self.assertIn(library, selection["public_libs_python"].split())

    def test_every_peppy_shared_package_is_selected_by_its_own_change(self):
        for name, directory in peppy_shared_directories().items():
            with self.subTest(package=name):
                selection = select(directory + "/src/lib.rs")
                if name == detect.PEPPYLIB_PY:
                    self.assertEqual(selection["public_libs_peppylib_py"], "true")
                    self.assertEqual(cargo_packages(selection), [])
                else:
                    self.assertIn(name, cargo_packages(selection))

    def test_a_crate_is_selected_by_a_change_to_what_it_depends_on(self):
        for crate, dependency in path_dependencies():
            with self.subTest(crate=crate, dependency=dependency):
                selection = select(dependency + "/src/lib.rs")
                self.assertIn(
                    os.path.basename(crate), selection["public_libs_crates"].split()
                )

    def test_a_python_library_is_selected_by_a_change_to_its_path_dependency(self):
        # so101_description takes control_core_py through tool.uv.sources, so
        # its suite is one of the two a change to control_core_py has to run.
        selection = select("%s/control_core_py/src/thing.py" % detect.TREE)
        self.assertEqual(
            selection["public_libs_python"].split(),
            ["control_core_py", "so101_description"],
        )

    def test_the_peppy_workspace_cannot_reach_the_sealed_tree(self):
        selection = select(
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            "crates/daemon-internal/src/lib.rs",
        )
        self.assertEqual(selection["public_libs_shared_packages"], "")
        self.assertEqual(selection["public_libs_peppylib_py"], "false")
        self.assertEqual(selection["public_libs_crates"], "")
        self.assertEqual(selection["public_libs_python"], "")

    def test_the_sealed_tree_reaches_the_peppy_workspace(self):
        # The peppy crates take the tree by path, so a change to one of its
        # packages has to run the suites of crates/ that compile it.
        selection = select(
            "%s/peppy-messaging-interface/src/lib.rs" % detect.PEPPY_SHARED
        )
        self.assertEqual(selection["container_e2e"], "true")

    def test_the_workspace_lockfile_selects_every_package_of_it(self):
        selection = select(detect.PEPPY_SHARED + "/Cargo.lock")
        self.assertEqual(selection["public_libs_peppylib_py"], "true")
        self.assertEqual(
            sorted(cargo_packages(selection)),
            sorted(
                name
                for name in peppy_shared_directories()
                if name != detect.PEPPYLIB_PY
            ),
        )

    def test_a_change_to_the_ci_plumbing_runs_everything(self):
        for path in (".github/workflows/tests.yml", ".github/actions/cargo-cache/action.yml"):
            with self.subTest(path=path):
                selection = select(path)
                self.assertEqual(selection["public_libs_crates"], " ".join(CRATES))
                self.assertEqual(selection["public_libs_python"], " ".join(LIBRARIES))
                self.assertEqual(selection["public_libs_peppylib_py"], "true")
                self.assertEqual(selection["container_e2e"], "true")
                self.assertEqual(selection["docs_integration"], "true")
                self.assertEqual(selection["scripts"], "true")

    def test_an_unknowable_change_set_runs_everything(self):
        selection = detect.select_everything(CRATES, LIBRARIES)
        self.assertEqual(
            selection["public_libs_shared_packages"],
            "--workspace --exclude " + detect.PEPPYLIB_PY,
        )
        self.assertEqual(selected(selection).keys(), selection.keys())

    def test_falling_open_emits_the_same_outputs_as_detecting(self):
        # An output the fail-open path forgot would read as the empty string
        # in the workflow, which is how a skipped job is spelled: the path
        # that exists to run everything would skip a suite instead.
        self.assertEqual(
            set(detect.select_everything(CRATES, LIBRARIES)),
            set(select("Readme.md")),
        )

    def test_every_output_the_workflow_reads_is_emitted(self):
        # The gates tests.yml reads through the changes job's outputs. An
        # output that stops being emitted silently skips a suite, because an
        # unset output is the empty string.
        emitted = set(select("Readme.md"))
        for gates in detect.JOBS.values():
            self.assertLessEqual(set(gates), emitted)


if __name__ == "__main__":
    unittest.main()
