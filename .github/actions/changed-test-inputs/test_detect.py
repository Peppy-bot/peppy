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


def select(*files):
    """The selection for a change set naming `files`."""
    return detect.select(list(files))


def cargo_packages(selection):
    """The peppy-shared packages a selection names, without the `-p` flags."""
    return [
        word for word in selection["peppy_shared_packages"].split() if word != "-p"
    ]


def selected(selection):
    """Every output of a selection that names something to run."""
    return {
        gate: value
        for gate, value in selection.items()
        if value not in ("", "false")
    }


def declared_outputs():
    """The output names action.yml declares, read without a yaml parser.

    The changes job runs these cases with the system python3 and nothing
    installed, so this walks the indentation rather than importing PyYAML.
    """
    path = os.path.join(os.path.dirname(os.path.abspath(__file__)), "action.yml")
    names, inside = [], False
    for line in open(path):
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        if not line[0].isspace():
            inside = line.startswith("outputs:")
            continue
        if inside and line.startswith("  ") and not line.startswith("   "):
            key = line.strip()
            if key.endswith(":"):
                names.append(key[:-1])
    return names


def peppy_shared_directories():
    """Directory of every package of the peppy-shared workspace, by name."""
    manifest = os.path.join(detect.ROOT, detect.PEPPY_SHARED, "Cargo.toml")
    directories, _, manifests = detect.crate_graph([manifest])
    return {
        name: directories[package]
        for name, package in manifests.items()
        if directories[package].startswith(detect.PEPPY_SHARED + "/")
    }


def workspace_member_directories(members):
    """Directory of every named package of the peppy workspace, by name."""
    directories, _, manifests = detect.crate_graph(
        [os.path.join(detect.ROOT, "Cargo.toml")]
    )
    return {name: directories[manifests[name]] for name in members}


class Detection(unittest.TestCase):
    def assert_every_peppy_shared_package_runs(self, selection):
        """The whole workspace selected: every cargo package by name, and
        peppylib-py through the gate of its own."""
        self.assertEqual(selection["peppy_shared_peppylib_py"], "true")
        self.assertEqual(
            sorted(cargo_packages(selection)),
            sorted(
                name
                for name in peppy_shared_directories()
                if name != detect.PEPPYLIB_PY
            ),
        )

    def test_a_change_outside_every_suite_runs_nothing(self):
        self.assertEqual(selected(select("Readme.md")), {})

    def test_every_peppy_shared_package_is_selected_by_its_own_change(self):
        for name, directory in peppy_shared_directories().items():
            with self.subTest(package=name):
                selection = select(directory + "/src/lib.rs")
                if name == detect.PEPPYLIB_PY:
                    self.assertEqual(selection["peppy_shared_peppylib_py"], "true")
                    self.assertEqual(cargo_packages(selection), [])
                else:
                    self.assertIn(name, cargo_packages(selection))

    def test_a_package_is_selected_by_a_change_to_what_it_depends_on(self):
        # peppylib-rs takes peppy-messaging-interface by path, so a change to
        # the latter has to run the former's suite too.
        selection = select(
            "%s/peppy-messaging-interface/src/lib.rs" % detect.PEPPY_SHARED
        )
        self.assertIn("peppylib-rs", cargo_packages(selection))

    def test_the_peppy_workspace_cannot_reach_the_sealed_tree(self):
        selection = select(
            "Cargo.toml",
            "Cargo.lock",
            ".cargo/config.toml",
            "crates/daemon-internal/src/lib.rs",
        )
        self.assertEqual(selection["peppy_shared_packages"], "")
        self.assertEqual(selection["peppy_shared_peppylib_py"], "false")

    def test_the_sealed_tree_reaches_the_peppy_workspace(self):
        # The peppy crates take the tree by path, so a change to one of its
        # packages has to run the suites of crates/ that compile it.
        selection = select(
            "%s/peppy-messaging-interface/src/lib.rs" % detect.PEPPY_SHARED
        )
        self.assertEqual(selection["container_e2e"], "true")

    def test_the_workspace_lockfile_selects_every_package_of_it(self):
        self.assert_every_peppy_shared_package_runs(
            select(detect.PEPPY_SHARED + "/Cargo.lock")
        )

    def test_a_change_to_what_every_job_runs_through_runs_everything(self):
        for path in (
            ".github/workflows/tests.yml",
            ".github/runs-on.yml",
            ".github/actions/rust-build-env/action.yml",
        ):
            with self.subTest(path=path):
                selection = select(path)
                self.assert_every_peppy_shared_package_runs(selection)
                everything = detect.select_everything()
                self.assertEqual(selected(selection).keys(), everything.keys())

    def test_a_change_to_the_cargo_plumbing_runs_the_cargo_suites(self):
        # The install suite builds its archive through the cargo cache too,
        # and never through cargo-suite.
        for path, install_script in (
            (".github/actions/cargo-cache/action.yml", "true"),
            (".github/actions/cargo-suite/action.yml", "false"),
        ):
            with self.subTest(path=path):
                selection = select(path)
                self.assert_every_peppy_shared_package_runs(selection)
                self.assertEqual(selection["workspace"], "true")
                self.assertEqual(selection["container_e2e"], "true")
                self.assertEqual(selection["multi_daemon_e2e"], "true")
                self.assertEqual(selection["docs_integration"], "true")
                self.assertEqual(selection["cross_check"], "true")
                self.assertEqual(selection["scripts"], "false")
                self.assertEqual(selection["install_script"], install_script)

    def test_the_detection_and_the_release_plumbing_gate_no_suite(self):
        # The detection decides what runs, never how a suite runs, and the
        # changes job tests it before trusting it; release-host-env is the
        # release workflow's.
        for path in (
            ".github/actions/changed-test-inputs/detect.py",
            ".github/actions/changed-test-inputs/test_detect.py",
            ".github/actions/changed-test-inputs/action.yml",
            ".github/actions/release-host-env/action.yml",
        ):
            with self.subTest(path=path):
                self.assertEqual(selected(select(path)), {})

    def test_the_workspace_manifests_run_the_suites_of_crates_alone(self):
        for path in ("Cargo.toml", "Cargo.lock", ".cargo/config.toml"):
            with self.subTest(path=path):
                self.assertEqual(
                    selected(select(path)).keys(),
                    {detect.WORKSPACE_SUITE, *detect.SUITES},
                )

    def test_the_workspace_suite_runs_for_every_default_member(self):
        root_manifest = os.path.join(detect.ROOT, "Cargo.toml")
        members = detect.workspace_default_members(root_manifest)
        self.assertIn("peppy", members)
        self.assertNotIn("docs-integration-tests", members)
        for name, directory in workspace_member_directories(members).items():
            with self.subTest(package=name):
                self.assertEqual(select(directory + "/src/lib.rs")["workspace"], "true")

    def test_the_workspace_suite_stays_off_for_what_no_member_compiles(self):
        for path in (
            "docs/src/content/docs/guides/quickstart.mdx",
            "docs/tests/integration/tests/rust_snippets.rs",
            "scripts/install.sh",
        ):
            with self.subTest(path=path):
                self.assertEqual(select(path)["workspace"], "false")

    def test_the_multi_daemon_suite_runs_for_the_launcher_it_drives(self):
        launchers = "docs/src/content/docs/guides/snippets/launchers"
        selection = select(launchers + "/split_compute_manipulation.json5")
        self.assertEqual(selection["multi_daemon_e2e"], "true")
        self.assertEqual(selection["workspace"], "false")
        self.assertEqual(selection["cross_check"], "false")

    def test_the_peppy_binary_suites_run_for_what_the_binary_compiles(self):
        # Both build the `peppy` package, which reaches the sealed tree.
        for path in (
            "crates/daemon-internal/src/lib.rs",
            "%s/peppy-messaging-interface/src/lib.rs" % detect.PEPPY_SHARED,
        ):
            with self.subTest(path=path):
                selection = select(path)
                self.assertEqual(selection["multi_daemon_e2e"], "true")
                self.assertEqual(selection["cross_check"], "true")

    def test_the_mocked_release_scripts_run_for_any_change_under_scripts(self):
        # The cheap half costs about a second, so it is gated on the whole
        # tree and nothing about a change has to be understood to run it.
        for path in (
            "scripts/functions/docs.py",
            "scripts/functions/release_notes.py",
            "scripts/tests/test_github.py",
            "scripts/install.sh",
        ):
            with self.subTest(path=path):
                self.assertEqual(select(path)["scripts"], "true")

    def test_the_install_tests_run_for_what_reaches_the_runner(self):
        for path in (
            "scripts/install.sh",
            "scripts/build_release.sh",
            "scripts/run_tests.sh",
            "scripts/tests/install_helpers.py",
            "scripts/tests/test_install.py",
            "scripts/tests/test_install_in_docker.py",
            "scripts/functions/build_release.py",
            "scripts/pixi.toml",
            ".github/actions/install-test/action.yml",
        ):
            with self.subTest(path=path):
                self.assertEqual(select(path)["install_script"], "true")

    def test_the_install_tests_stay_off_for_the_rest_of_the_tree(self):
        # The modules that churn most: none of them can change what install.sh
        # does to a machine, and a release build to prove it is the whole cost
        # of this suite on a typical release-scripts change. functions/lima.py
        # builds the Linux archives of a macOS release host and never runs on
        # the Linux box that builds the one these tests install.
        for path in (
            "scripts/functions/docs.py",
            "scripts/functions/docker.py",
            "scripts/functions/github.py",
            "scripts/functions/lima.py",
            "scripts/functions/parallel_release.py",
            "scripts/functions/release_notes.py",
            "scripts/functions/release_summary.py",
            "scripts/functions/claude.py",
            "scripts/tests/test_docs.py",
            "scripts/tests/test_github.py",
        ):
            with self.subTest(path=path):
                selection = select(path)
                self.assertEqual(selection["install_script"], "false")
                self.assertEqual(selection["scripts"], "true")

    def test_an_unknowable_change_set_runs_everything(self):
        selection = detect.select_everything()
        self.assertEqual(
            selection["peppy_shared_packages"],
            "--workspace --exclude " + detect.PEPPYLIB_PY,
        )
        self.assertEqual(selected(selection).keys(), selection.keys())

    def test_falling_open_emits_the_same_outputs_as_detecting(self):
        # An output the fail-open path forgot would read as the empty string
        # in the workflow, which is how a skipped job is spelled: the path
        # that exists to run everything would skip a suite instead.
        self.assertEqual(
            set(detect.select_everything()),
            set(select("Readme.md")),
        )

    def test_every_output_the_workflow_reads_is_emitted(self):
        # The gates tests.yml reads through the changes job's outputs. An
        # output that stops being emitted silently skips a suite, because an
        # unset output is the empty string.
        emitted = set(select("Readme.md"))
        for gates in detect.JOBS.values():
            self.assertLessEqual(set(gates), emitted)

    def test_the_action_declares_every_output_the_detection_emits(self):
        # A composite action forwards only what it declares. An output the
        # detection emits and action.yml does not reaches the workflow as the
        # empty string, which is how a skipped job is spelled -- so the suite
        # is silently gated off and nothing anywhere is red. That is exactly
        # what adding the gate of one half of the release scripts did until
        # this case existed.
        self.assertEqual(sorted(declared_outputs()), sorted(select("Readme.md")))


if __name__ == "__main__":
    unittest.main()
