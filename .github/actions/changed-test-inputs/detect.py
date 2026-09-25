#!/usr/bin/env python3
"""Gate the conditionally-run suites of tests.yml on their real inputs.

Emits one output per gated suite naming what the change set requires it to
run: a boolean for the suites that are all or nothing, and a package selection
for the `peppy-shared` workspace, which splits into units testable on their
own. A suite whose whole selection comes out empty has its job skipped, and
the reason is reported to the job's annotations and step summary.

No crate is listed here. A cargo package's inputs are its own directory plus
the directory of every crate it reaches through a path dependency, read from
`cargo metadata`. The walk crosses workspace boundaries, so a `crates/` member
depending on the sealed tree is a fact of the manifests rather than a list kept
in step by hand, and a crate added to the tree is picked up the moment its
manifest lands.

Detection fails open: when the change set is unknowable or the walk fails,
every suite runs with everything selected, so a bug here can never silently
skip tests.
"""

import functools
import json
import os
import subprocess
from fnmatch import fnmatch

# Suites driven by a cargo package of the `peppy` workspace: the package name
# its test command names, plus extra trees the suite reads at run time that
# live outside every package directory (and are therefore invisible to the
# dependency graph).
SUITES = {
    # cargo test -p core-node --features container_e2e --test container_e2e
    "container_e2e": ("core-node", []),
    # cargo test -p peppy --features multi_daemon_e2e --test multi_daemon_e2e.
    # The federated tests drive the launcher the Federation guide documents,
    # copied out of the docs tree at run time (SPLIT_COMPUTE_LAUNCHER).
    "multi_daemon_e2e": (
        "peppy",
        ["docs/src/content/docs/guides/snippets/launchers/**"],
    ),
    # cargo test -p docs-integration-tests. The snippet suites walk the docs
    # snippets at run time (tests/snippet_configs.rs, SNIPPETS_ROOT); that
    # directory lives outside the package, so the graph cannot see it.
    "docs_integration": (
        "docs-integration-tests",
        ["docs/src/content/docs/guides/snippets/**"],
    ),
    # cargo check --locked --target aarch64-unknown-linux-gnu -p peppy
    "cross_check": ("peppy", []),
}

# The suite whose command names no package: `cargo test --locked` at the
# workspace root, which tests the workspace's default members. Their names
# are read from `cargo metadata` when the selection is made, so a crate added
# under crates/ is gated on the moment its manifest lands.
WORKSPACE_SUITE = "workspace"

# Suites with no cargo package: each one's whole world is a single tree,
# toolchain included.
#
# The release scripts split across two of them because their two halves cost
# orders of magnitude apart. The mocked half runs the whole tree in about a
# second, so it is gated on the whole tree. The install half builds a release
# archive and installs it on a fresh runner, so it is gated on the files that
# decide what gets installed and how: a change to the release-notes drafter
# or the docs gate cannot alter what install.sh does to a machine.
TREE_SUITES = {
    # pixi run test-fast
    "scripts": ["scripts/**"],
    # The install-archive and install-script jobs. install.sh is what the
    # runner runs; build_release.sh and build/build_release/cli produce the
    # archive it installs (`build_release.sh --local --tag test`), with its
    # build cache kept by the cargo-cache action; the install-test action,
    # run_tests.sh, conftest and the two test modules are what drives it; and
    # the manifests pin the pixi environment the tests run in.
    "install_script": [
        "scripts/install.sh",
        "scripts/build_release.sh",
        "scripts/functions/build.py",
        "scripts/functions/build_release.py",
        "scripts/functions/cli.py",
        "scripts/run_tests.sh",
        "scripts/tests/conftest.py",
        "scripts/tests/install_helpers.py",
        "scripts/tests/test_install.py",
        "scripts/tests/test_install_in_docker.py",
        "scripts/pixi.toml",
        "scripts/pixi.lock",
        ".github/actions/cargo-cache/**",
        ".github/actions/install-test/**",
    ],
}

# The sealed tree, and the one member of it whose suite is not a cargo test:
# peppylib-py's cdylib build and tests/python_tests.rs only wrap `pixi run
# test`, which the workflow runs directly, so it is selected on a gate of its
# own and never joins the cargo package selection.
PEPPY_SHARED = "peppy-shared"
PEPPYLIB_PY = "peppylib-py"

# The outputs each conditionally-run job of tests.yml reads. A job all of
# whose outputs came out empty or false is skipped, and the report says so.
JOBS = {
    "workspace-tests": [WORKSPACE_SUITE],
    "container-e2e": ["container_e2e"],
    "multi-daemon-e2e": ["multi_daemon_e2e"],
    "docs-integration": ["docs_integration"],
    "cross-check": ["cross_check"],
    "release-scripts": ["scripts"],
    "install-archive": ["install_script"],
    "install-script": ["install_script"],
    "peppy-shared": ["peppy_shared_packages", "peppy_shared_peppylib_py"],
}

# CI plumbing every suite runs through: the workflow, the runner definitions
# its jobs name, and the one action every job of it starts with. An action is
# an input of the jobs that use it and of no other, which is why the rest of
# `.github/actions` is not here: the cargo plumbing is listed below for the
# suites that build with it (and the cargo-cache action again for the install
# suite, whose archive build keeps its cache there), the detection itself is
# tested by the changes job before it is trusted and changes what runs rather
# than how a suite runs, and release-host-env belongs to the release workflow.
CI_INPUTS = [
    ".github/workflows/tests.yml",
    ".github/runs-on.yml",
    ".github/actions/rust-build-env/**",
]

# The plumbing the cargo suites build and run through, on top of CI_INPUTS.
# The two scripts suites never touch it.
CARGO_CI_INPUTS = [
    ".github/actions/cargo-cache/**",
    ".github/actions/cargo-suite/**",
]

# What the `peppy` workspace resolves itself from: an input of the suites that
# build `crates/`, and of them alone. The sealed tree resolves and locks on
# its own and CI checks it out without these files. The install suite builds
# a release archive from the workspace to install, and what it proves is what
# install.sh does with that archive; the archive's own behaviour is what the
# suites of `crates/` test.
PEPPY_WORKSPACE_INPUTS = [
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
]

# What the `peppy-shared` workspace resolves itself from, shared by every one
# of its members. Each member's own directory is covered by the graph.
PEPPY_SHARED_INPUTS = [
    PEPPY_SHARED + "/Cargo.toml",
    PEPPY_SHARED + "/Cargo.lock",
]


def repository_root():
    return subprocess.check_output(
        ["git", "rev-parse", "--show-toplevel"], text=True
    ).strip()


ROOT = repository_root()


def repo_path(absolute):
    """`absolute` as git spells it: relative to the repository root."""
    return os.path.relpath(absolute, ROOT)


def changed_files(base):
    """Files changed on this branch relative to base; None when unknowable."""
    if not base or set(base) == {"0"}:
        return None
    # A force push can strand the recorded base; an unreachable base makes
    # the diff meaningless, so fall back to running everything.
    reachable = subprocess.run(
        ["git", "rev-parse", "--verify", "--quiet", base + "^{commit}"],
        capture_output=True,
    )
    if reachable.returncode != 0:
        return None
    out = subprocess.check_output(
        ["git", "diff", "--name-only", base + "...HEAD"], text=True
    )
    return [line for line in out.splitlines() if line]


@functools.cache
def cargo_metadata(manifest):
    """The metadata of the workspace `manifest` belongs to.

    `--no-deps` reads manifests alone: no lockfile, no registry index, no
    network, and a workspace-inherited dependency comes back carrying the
    path it inherits. Cached because the graph walk and the default-member
    lookup both read the root workspace.
    """
    return json.loads(
        subprocess.check_output(
            [
                "cargo",
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--manifest-path",
                manifest,
            ],
            text=True,
        )
    )


def workspace_packages(manifest):
    """Every package of the workspace `manifest` belongs to, as (name,
    manifest path, manifests of the crates it depends on by path).

    Dev and build dependencies are in the list too, so a crate only another's
    tests or build script use still counts as its input.
    """
    for package in cargo_metadata(manifest)["packages"]:
        path_deps = [
            os.path.join(dep["path"], "Cargo.toml")
            for dep in package["dependencies"]
            if dep.get("path")
        ]
        yield package["name"], package["manifest_path"], path_deps


def workspace_default_members(manifest):
    """Names of the packages `cargo test` at `manifest` tests when it names
    none: the workspace's default members."""
    meta = cargo_metadata(manifest)
    default = set(meta["workspace_default_members"])
    return [package["name"] for package in meta["packages"] if package["id"] in default]


def crate_graph(seeds):
    """Walk out from `seeds` over path dependencies, one `cargo metadata` per
    workspace reached, and return what the walk found: the repository-relative
    directory and the path-dependency manifests of every crate, keyed by
    manifest path, plus the manifest path of every crate, keyed by package
    name.
    """
    directories, dependencies, manifests = {}, {}, {}
    pending = list(seeds)
    while pending:
        manifest = pending.pop()
        if manifest in directories:
            continue
        for name, package_manifest, path_deps in workspace_packages(manifest):
            directories[package_manifest] = repo_path(os.path.dirname(package_manifest))
            dependencies[package_manifest] = path_deps
            manifests[name] = package_manifest
            pending.extend(path_deps)
    return directories, dependencies, manifests


def crate_dirs(manifest, graph):
    """Directories of the crate at `manifest` and of every crate it reaches
    through a path dependency, at any depth."""
    directories, dependencies, _ = graph
    seen = {manifest}
    todo = [manifest]
    while todo:
        for dependency in dependencies.get(todo.pop(), ()):
            if dependency not in seen:
                seen.add(dependency)
                todo.append(dependency)
    return {directories[reached] for reached in seen if reached in directories}


def gate(selected):
    """A boolean as a workflow output reads it."""
    return "true" if selected else "false"


def touches(files, globs, dirs):
    for path in files:
        if any(path == glob or fnmatch(path, glob) for glob in globs):
            return True
        if any(path == d or path.startswith(d + "/") for d in dirs):
            return True
    return False


def select_everything():
    """What runs when the change set is unknowable or detection failed: all of
    it. The cargo selection is spelled `--workspace` because the package names
    are exactly what a failed graph walk could not produce."""
    selection = {
        suite: "true" for suite in [WORKSPACE_SUITE] + list(SUITES) + list(TREE_SUITES)
    }
    selection.update(
        {
            "peppy_shared_packages": "--workspace --exclude " + PEPPYLIB_PY,
            "peppy_shared_peppylib_py": "true",
        }
    )
    return selection


def select(files):
    """What each gated suite must run for the change set `files`."""
    root_manifest = os.path.join(ROOT, "Cargo.toml")
    graph = crate_graph([root_manifest, os.path.join(ROOT, PEPPY_SHARED, "Cargo.toml")])
    _, _, manifests = graph

    def crate_touched(manifest, globs):
        return touches(files, globs, crate_dirs(manifest, graph))

    cargo_inputs = CI_INPUTS + CARGO_CI_INPUTS
    peppy_inputs = cargo_inputs + PEPPY_WORKSPACE_INPUTS
    selection = {
        WORKSPACE_SUITE: gate(
            any(
                crate_touched(manifests[member], peppy_inputs)
                for member in workspace_default_members(root_manifest)
            )
        )
    }
    for suite, (package, extra) in SUITES.items():
        selection[suite] = gate(crate_touched(manifests[package], extra + peppy_inputs))
    for suite, globs in TREE_SUITES.items():
        selection[suite] = gate(touches(files, globs + CI_INPUTS, ()))

    shared = sorted(
        name
        for name, manifest in manifests.items()
        if repo_path(os.path.dirname(os.path.dirname(manifest))) == PEPPY_SHARED
        and crate_touched(manifest, cargo_inputs + PEPPY_SHARED_INPUTS)
    )
    selection["peppy_shared_peppylib_py"] = gate(PEPPYLIB_PY in shared)
    selection["peppy_shared_packages"] = " ".join(
        "-p " + name for name in shared if name != PEPPYLIB_PY
    )
    return selection


def report(selection, base):
    """Say what each conditionally-run job runs, and why a skipped one is
    skipped: a job annotation per skipped job plus a step summary section,
    because the gray "skipped" mark a workflow shows explains nothing."""
    versus = base[:12] if base else "the base commit"
    lines = []
    for job, gates in JOBS.items():
        selected = [
            selection[gate] for gate in gates if selection[gate] not in ("", "false")
        ]
        if not selected:
            print(
                '::notice::Skipped "%s": none of the files it builds from or '
                "reads changed vs %s" % (job, versus)
            )
            lines.append(
                "- **%s** is skipped: nothing it builds from or reads changed" % job
            )
            continue
        # A boolean output says only that the job runs; a selection says which
        # packages or libraries of it do.
        detail = ", ".join(value for value in selected if value != "true")
        lines.append("- **%s** runs%s" % (job, ": `%s`" % detail if detail else ""))
    summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
    if not summary_file:
        return
    with open(summary_file, "a") as handle:
        handle.write("### Gated test suites\n")
        handle.write("Measured against the diff vs `%s`.\n\n" % versus)
        handle.write("\n".join(lines) + "\n")


def main():
    base = os.environ.get("INPUT_BASE", "")
    try:
        files = changed_files(base)
        if files is None:
            print("changed file set unknowable; running every suite")
            selection = select_everything()
        else:
            print("changed files (%d):" % len(files))
            for path in files:
                print("  " + path)
            selection = select(files)
    except Exception as exc:  # noqa: BLE001 - fail open by design
        print("::warning::change detection failed (%s); running every suite" % exc)
        selection = select_everything()
    print("suite selection: " + json.dumps(selection, sort_keys=True))
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with open(output_file, "a") as handle:
            for gate, value in selection.items():
                handle.write("%s=%s\n" % (gate, value))
    report(selection, base)


if __name__ == "__main__":
    main()
