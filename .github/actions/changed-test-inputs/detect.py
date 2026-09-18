#!/usr/bin/env python3
"""Gate the conditionally-run suites of tests.yml on their real inputs.

Emits one output per gated suite naming what the change set requires it to
run: a boolean for the suites that are all or nothing, and a selection of
packages or directories for the ones that split into units testable on their
own (the `peppy-shared` workspace, the standalone crates of
`public-peppy-libs`, its standalone Python libraries). A suite whose whole
selection comes out empty has its job skipped, and the reason is reported to
the job's annotations and step summary.

No crate or library is listed here. A cargo package's inputs are its own
directory plus the directory of every crate it reaches through a path
dependency, read from `cargo metadata`; a Python library's are its own
directory plus those of its `tool.uv.sources` path dependencies. Both walks
cross workspace boundaries, so a `crates/` member depending on the sealed
tree, or a standalone crate depending on a sibling, is a fact of the
manifests rather than a list kept in step by hand, and a crate added to the
tree is picked up the moment its manifest lands.

Detection fails open: when the change set is unknowable or the walk fails,
every suite runs with everything selected, so a bug here can never silently
skip tests.
"""

import json
import os
import subprocess
import tomllib
from fnmatch import fnmatch

# Suites driven by a cargo package of the `peppy` workspace: the package name
# its test command names, plus extra trees the suite reads at run time that
# live outside every package directory (and are therefore invisible to the
# dependency graph).
SUITES = {
    # cargo test -p core-node --features container_e2e --test container_e2e
    "container_e2e": ("core-node", []),
    # cargo test -p docs-integration-tests. The snippet suites walk the docs
    # snippets at run time (tests/snippet_configs.rs, SNIPPETS_ROOT); that
    # directory lives outside the package, so the graph cannot see it.
    "docs_integration": (
        "docs-integration-tests",
        ["docs/src/content/docs/guides/snippets/**"],
    ),
}

# Suites with no cargo package: each one's whole world is a single tree,
# toolchain included.
#
# The release scripts split across two of them because their two halves cost
# three orders of magnitude apart. The mocked half runs the whole tree in
# about a second, so it is gated on the whole tree. The Lima half boots a
# guest per distro and takes a quarter of an hour, so it is gated on the files
# that decide what it installs and how: a change to the release-notes drafter
# or the docs gate cannot alter what install.sh does to a guest, and used to
# boot three of them to prove it.
TREE_SUITES = {
    # pixi run test-fast
    "scripts": ["scripts/**"],
    # pixi run test-vm. install.sh is what the guests run; lima_helpers and
    # the two test modules are what drives them; and build/build_release/cli
    # produce the release archive the guests install from (see
    # conftest._build_release_archives). The manifests pin the pixi
    # environment the guests and limactl come out of.
    "scripts_vm": [
        "scripts/install.sh",
        "scripts/functions/build.py",
        "scripts/functions/build_release.py",
        "scripts/functions/cli.py",
        "scripts/functions/docker.py",
        "scripts/functions/lima.py",
        "scripts/tests/conftest.py",
        "scripts/tests/lima_helpers.py",
        "scripts/tests/test_install.py",
        "scripts/tests/test_install_container.py",
        "scripts/pixi.toml",
        "scripts/pixi.lock",
    ],
}

# The sealed tree, its cargo workspace, and the one member of that workspace
# whose suite is not a cargo test: peppylib-py's cdylib build and
# tests/python_tests.rs only wrap `pixi run test`, which the workflow runs
# directly, so it is selected on a gate of its own and never joins the cargo
# package selection.
TREE = "public-peppy-libs"
PEPPY_SHARED = TREE + "/peppy-shared"
PEPPYLIB_PY = "peppylib-py"

# The outputs each conditionally-run job of tests.yml reads. A job all of
# whose outputs came out empty or false is skipped, and the report says so.
JOBS = {
    "container-e2e": ["container_e2e"],
    "docs-integration": ["docs_integration"],
    "release-scripts": ["scripts"],
    "release-scripts-vm": ["scripts_vm"],
    "public-libs-shared": ["public_libs_shared_packages", "public_libs_peppylib_py"],
    "public-libs-crates": ["public_libs_crates"],
    "public-libs-python": ["public_libs_python"],
}

# CI plumbing every suite is built and run by. This tracks the workflow and
# its actions, not workspace modules, so the list does not grow as crates are
# added.
CI_INPUTS = [
    ".github/actions/**",
    ".github/workflows/tests.yml",
]

# What the `peppy` workspace resolves itself from. The sealed tree resolves
# and locks on its own and CI checks it out without these files, so they are
# an input to the suites of `crates/` alone.
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


def workspace_packages(manifest):
    """Every package of the workspace `manifest` belongs to, as (name,
    manifest path, manifests of the crates it depends on by path).

    `--no-deps` reads manifests alone: no lockfile, no registry index, no
    network, and a workspace-inherited dependency comes back carrying the
    path it inherits. Dev and build dependencies are in the list too, so a
    crate only another's tests or build script use still counts as its input.
    """
    meta = json.loads(
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
    for package in meta["packages"]:
        path_deps = [
            os.path.join(dep["path"], "Cargo.toml")
            for dep in package["dependencies"]
            if dep.get("path")
        ]
        yield package["name"], package["manifest_path"], path_deps


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


def python_dirs(library):
    """Directories of the Python library at `library` and of every library it
    depends on by path, at any depth. `tool.uv.sources` is where a path
    dependency is spelled: the dependency itself is a bare name under
    `project.dependencies`, and the source is what points it at a sibling."""
    directories = {library}
    todo = [library]
    while todo:
        current = todo.pop()
        with open(os.path.join(ROOT, current, "pyproject.toml"), "rb") as handle:
            manifest = tomllib.load(handle)
        sources = manifest.get("tool", {}).get("uv", {}).get("sources", {})
        for source in sources.values():
            if not isinstance(source, dict) or "path" not in source:
                continue
            dependency = os.path.normpath(os.path.join(current, source["path"]))
            if dependency in directories:
                continue
            directories.add(dependency)
            todo.append(dependency)
    return directories


def tree_members():
    """The standalone crates and Python libraries of the sealed tree, read
    from the filesystem: a directory holding a Cargo.toml is a crate, and one
    holding a pyproject.toml beside a tests/ directory is a library whose
    suite CI runs. peppy-shared is neither; it is the cargo workspace the
    public-libs-shared job tests package by package."""
    crates, libraries = [], []
    for entry in sorted(os.listdir(os.path.join(ROOT, TREE))):
        directory = os.path.join(ROOT, TREE, entry)
        if directory == os.path.join(ROOT, PEPPY_SHARED):
            continue
        if os.path.isfile(os.path.join(directory, "Cargo.toml")):
            crates.append(entry)
        if os.path.isfile(os.path.join(directory, "pyproject.toml")) and os.path.isdir(
            os.path.join(directory, "tests")
        ):
            libraries.append(entry)
    return crates, libraries


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


def select_everything(crates, libraries):
    """What runs when the change set is unknowable or detection failed: all of
    it. The cargo selection is spelled `--workspace` because the package names
    are exactly what a failed graph walk could not produce."""
    selection = {suite: "true" for suite in list(SUITES) + list(TREE_SUITES)}
    selection.update(
        {
            "public_libs_shared_packages": "--workspace --exclude " + PEPPYLIB_PY,
            "public_libs_peppylib_py": "true",
            "public_libs_crates": " ".join(crates),
            "public_libs_python": " ".join(libraries),
        }
    )
    return selection


def select(files, crates, libraries):
    """What each gated suite must run for the change set `files`."""
    graph = crate_graph(
        [os.path.join(ROOT, "Cargo.toml"), os.path.join(ROOT, PEPPY_SHARED, "Cargo.toml")]
        + [os.path.join(ROOT, TREE, crate, "Cargo.toml") for crate in crates]
    )
    _, _, manifests = graph

    def crate_touched(manifest, globs):
        return touches(files, globs, crate_dirs(manifest, graph))

    selection = {}
    for suite, (package, extra) in SUITES.items():
        selection[suite] = gate(
            crate_touched(manifests[package], extra + CI_INPUTS + PEPPY_WORKSPACE_INPUTS)
        )
    for suite, globs in TREE_SUITES.items():
        selection[suite] = gate(
            touches(files, globs + CI_INPUTS + PEPPY_WORKSPACE_INPUTS, ())
        )

    shared = sorted(
        name
        for name, manifest in manifests.items()
        if repo_path(os.path.dirname(os.path.dirname(manifest))) == PEPPY_SHARED
        and crate_touched(manifest, CI_INPUTS + PEPPY_SHARED_INPUTS)
    )
    selection["public_libs_peppylib_py"] = gate(PEPPYLIB_PY in shared)
    selection["public_libs_shared_packages"] = " ".join(
        "-p " + name for name in shared if name != PEPPYLIB_PY
    )
    selection["public_libs_crates"] = " ".join(
        crate
        for crate in crates
        if crate_touched(os.path.join(ROOT, TREE, crate, "Cargo.toml"), CI_INPUTS)
    )
    selection["public_libs_python"] = " ".join(
        library
        for library in libraries
        if touches(files, CI_INPUTS, python_dirs(TREE + "/" + library))
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
    crates, libraries = tree_members()
    try:
        files = changed_files(base)
        if files is None:
            print("changed file set unknowable; running every suite")
            selection = select_everything(crates, libraries)
        else:
            print("changed files (%d):" % len(files))
            for path in files:
                print("  " + path)
            selection = select(files, crates, libraries)
    except Exception as exc:  # noqa: BLE001 - fail open by design
        print("::warning::change detection failed (%s); running every suite" % exc)
        selection = select_everything(crates, libraries)
    print("suite selection: " + json.dumps(selection, sort_keys=True))
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with open(output_file, "a") as handle:
            for gate, value in selection.items():
                handle.write("%s=%s\n" % (gate, value))
    report(selection, base)


if __name__ == "__main__":
    main()
