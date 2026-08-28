#!/usr/bin/env python3
"""Gate the conditionally-run suites of tests.yml on their real inputs.

Emits one boolean output per suite, true when the branch's diff against the
base commit touched anything that suite compiles from this workspace or
reads at run time. A suite gated off gets its skip reason reported to the
job's annotations and step summary.

The crate sets are not listed here: they are derived from cargo metadata's
resolve graph (internal path dependencies, transitive, including dev and
build dependencies), so a crate that gains or loses a workspace dependency,
or a new crate added to the workspace, is reflected the moment its manifest
lands. Nothing in this file tracks the module layout.

Detection fails open: on any error every suite runs, so a bug here can never
silently skip tests.
"""

import json
import os
import subprocess
from fnmatch import fnmatch

# Suites driven by a cargo package: the package name its test command names,
# plus extra trees the suite reads at run time that live outside every
# package directory (and are therefore invisible to the dependency graph).
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
TREE_SUITES = {
    # ./scripts/run_tests.sh --all
    "scripts": ["scripts/**"],
}

# The tests.yml job each gate guards, so a skip reason names what CI did
# not run. Presentation only; an unknown suite falls back to its key.
JOB_NAMES = {
    "container_e2e": "container-e2e",
    "docs_integration": "docs-integration",
    "scripts": "release-scripts",
}

# Build and tooling inputs shared by every suite. These track CI and build
# plumbing, not workspace modules, so the list does not grow as crates are
# added.
SHARED = [
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    ".github/actions/rust-build-env/**",
    ".github/actions/cargo-suite/**",
    ".github/actions/changed-test-inputs/**",
    ".github/workflows/tests.yml",
]


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


def workspace_graph():
    """Map each workspace package to its directory and internal deps."""
    meta = json.loads(
        subprocess.check_output(
            ["cargo", "metadata", "--format-version", "1", "--locked"], text=True
        )
    )
    root = meta["workspace_root"] + "/"
    id_to_name = {}
    name_to_dir = {}
    for package in meta["packages"]:
        id_to_name[package["id"]] = package["name"]
        manifest = package.get("manifest_path", "")
        if manifest.startswith(root):
            name_to_dir[package["name"]] = manifest[len(root):].rsplit("/", 1)[0]
    edges = {}
    for node in meta["resolve"]["nodes"]:
        name = id_to_name[node["id"]]
        if name not in name_to_dir:
            continue
        deps = set()
        for dep_id in node["dependencies"]:
            dep = id_to_name[dep_id]
            if dep in name_to_dir and dep != name:
                deps.add(dep)
        edges.setdefault(name, set()).update(deps)
    return name_to_dir, edges


def closure_dirs(name_to_dir, edges, package):
    """Workspace directories of package and its transitive internal deps."""
    seen = {package}
    todo = [package]
    while todo:
        current = todo.pop()
        for dep in edges.get(current, ()):
            if dep not in seen:
                seen.add(dep)
                todo.append(dep)
    return {name_to_dir[name] for name in seen}


def touches(files, globs, dirs):
    for path in files:
        if any(path == glob or fnmatch(path, glob) for glob in globs):
            return True
        if any(path == d or path.startswith(d + "/") for d in dirs):
            return True
    return False


def report_skips(gates, base):
    """Say why each gated-off suite is skipped: a job annotation per suite
    plus a step summary section, because the gray "skipped" mark a workflow
    shows explains nothing."""
    skipped = [
        (suite, JOB_NAMES.get(suite, suite))
        for suite, gate in sorted(gates.items())
        if not gate
    ]
    if not skipped:
        return
    versus = base[:12] if base else "the base commit"
    for _, job in skipped:
        print(
            '::notice::Skipped "%s": none of the files it builds from or '
            "reads changed vs %s" % (job, versus)
        )
    summary_file = os.environ.get("GITHUB_STEP_SUMMARY")
    if summary_file:
        with open(summary_file, "a") as handle:
            handle.write("### Skipped test suites\n")
            handle.write(
                "No file these suites build from or read changed in the "
                "diff vs `%s`, so the Tests workflow skips them:\n" % versus
            )
            for suite, job in skipped:
                handle.write("- **%s** (gate `%s`)\n" % (job, suite))


def main():
    base = os.environ.get("INPUT_BASE", "")
    try:
        files = changed_files(base)
        name_to_dir, edges = workspace_graph()
        if files is None:
            print("changed file set unknowable; running every suite")
            gates = {suite: True for suite in list(SUITES) + list(TREE_SUITES)}
        else:
            print("changed files (%d):" % len(files))
            for path in files:
                print("  " + path)
            gates = {}
            for suite, (package, extra) in SUITES.items():
                dirs = closure_dirs(name_to_dir, edges, package)
                gates[suite] = touches(files, extra + SHARED, dirs)
            for suite, globs in TREE_SUITES.items():
                gates[suite] = touches(files, globs + SHARED, ())
    except Exception as exc:  # noqa: BLE001 - fail open by design
        print("::warning::change detection failed (%s); running every suite" % exc)
        gates = {suite: True for suite in list(SUITES) + list(TREE_SUITES)}
    print("suite gates: " + json.dumps(gates, sort_keys=True))
    output_file = os.environ.get("GITHUB_OUTPUT")
    if output_file:
        with open(output_file, "a") as handle:
            for suite, gate in gates.items():
                handle.write("%s=%s\n" % (suite, "true" if gate else "false"))
    report_skips(gates, base)


if __name__ == "__main__":
    main()
