"""Build a peppy release on several machines at once, then publish it.

`scripts/build_release.sh` builds every archive on one macOS ARM64 host: the
macOS target natively and both Linux targets inside a Lima VM. This module cuts
the same release in stages that `.github/workflows/parallel-release.yml` runs
on separate runners, so each archive builds natively on a runner of its own
platform, all three at once, and no stage starts a VM:

- `prepare` (any host): the branch, tag and docs checks and the drafted release
  notes, written to a release plan that every later stage reads.
- `bindings` (macOS ARM64): the peppylib native bindings for every platform.
- `apptainer` (Linux): apptainer for the host's architecture.
- `build` (one per target): one release archive, built from the plan's commit.
- `publish` (any host): the GitHub release from the three archives, then the
  notes committed on `dev` and `main` fast-forwarded to it.

The provisioning stages exist because a native build cannot produce everything
it embeds. Every peppy binary carries the peppylib bindings of all three
platforms, and only macOS builds the macOS one. The macOS build ships the
apptainer its Linux VM runs containers with, and a release build (under
PEPPY_CROSS_ARCH) looks for apptainer of both Linux architectures, building a
missing one in a Lima VM. Provisioned by the machines that build them natively,
the same bindings and apptainer reach every archive.

No stage asks a question: every answer the single-host release prompts for is
an option of the stage that needs it, and the drafted notes are published as
they are.
"""

from __future__ import annotations

import argparse
import fnmatch
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import httpx

from .build import BuildArtifact, _release_rustflags, build_and_package, release_dist_dir
from .build_release import (
    ALIGNED_BRANCH,
    DOCS_DIR,
    DOCS_POLISH_BRANCH_PREFIX,
    DOCS_SYNC_BRANCH_PREFIX,
    GIT_REMOTE,
    RELEASE_BRANCH,
    _docs_polish_pr_body,
    _docs_sync_pr_body,
    _find_open_docs_sync_pr,
    _open_docs_pr,
    _print_release_content,
    _publish_pending_upload,
    _push_docs_sync_branch,
    _stop_for_docs_pr,
    _verify_release_branch_state,
)
from .cli import (
    RELEASE_TRIPLES,
    ReleaseError,
    console,
    get_native_triple,
    is_linux,
    is_macos_arm64,
    need_cmd,
    require_release_token,
    run_with_error_handling,
    validate_release_environment,
)
from .docs import RequiredChange, check_docs, print_minor_changes, update_docs
from .github import RepoSlug, build_github_client, get_latest_release, github_repo_slug
from .lima import RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER, require_prebuilt_peppylib_so
from .pending_upload import pending_upload_path, record_pending_upload
from .release_summary import (
    ReleaseContent,
    collect_release_changes,
    generate_release_content,
)
from .repo import get_commit, get_repo_root, has_changes_in_paths
from .verify_release import verify_all_releases

# File names the provisioning stages write and the build stage reads.
BINDINGS_ARCHIVE = "peppylib-bindings.tgz"
BINDINGS_DIR_NAME = "so"

# Env var the prepare and publish stages read the GitHub release token from.
# The workflow passes a repository secret of the same name, and GitHub rejects
# secret names that start with GITHUB_.
RELEASE_TOKEN_ENV = "PEPPY_RELEASE_TOKEN"


def apptainer_archive_name(arch: str) -> str:
    return f"apptainer-{arch}.tgz"


# --- the release plan ---


@dataclass(frozen=True)
class ReleasePlan:
    """What every stage after `prepare` builds and publishes: the tag, the
    commit the archives are built from, and the drafted release content."""

    tag: str
    release_commit: str
    content: ReleaseContent

    def write(self, path: Path) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "tag": self.tag,
            "release_commit": self.release_commit,
            "title": self.content.title,
            "description": self.content.description,
            "notes": self.content.notes,
        }
        path.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    @classmethod
    def load(cls, path: Path) -> ReleasePlan:
        try:
            payload = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as e:
            raise ReleaseError(f"cannot read the release plan {path}: {e}") from e
        if not isinstance(payload, dict):
            raise ReleaseError(f"the release plan {path} is not a JSON object")
        fields: dict[str, str] = {}
        for name in ("tag", "release_commit", "title", "description", "notes"):
            value = payload.get(name)
            if not isinstance(value, str) or not value.strip():
                raise ReleaseError(f"the release plan {path} has no usable '{name}'")
            fields[name] = value
        return cls(
            tag=fields["tag"],
            release_commit=fields["release_commit"],
            content=ReleaseContent(
                title=fields["title"],
                description=fields["description"],
                notes=fields["notes"],
            ),
        )


# --- helpers shared by the stages ---


def _triple_arch(triple: str) -> str:
    """The CPU architecture a target triple names: 'aarch64' or 'x86_64'."""
    return triple.split("-", 1)[0]


def _build_cache_root() -> Path:
    """The persistent cache the Rust build scripts share.

    build-helpers' `cache_dir` roots it at `$HOME/.peppy/tmp` whatever
    PEPPY_HOME says; `lima._prebuilt_peppylib_so_dir` resolves the same root.
    """
    return Path.home() / ".peppy" / "tmp"


def _apptainer_cache_pattern(arch: str) -> str:
    """Glob for the apptainer cache of *arch* under the build cache root.

    The name mirrors `apptainer_cache_dir_name` in
    crates/containers-internal/build.rs (`apptainer-{version}-{arch}-nosuid`).
    The version stays a wildcard so the Rust side remains its only owner.
    """
    return f"apptainer-*-{arch}-nosuid"


def _apptainer_arches_for(target: str) -> tuple[str, ...]:
    """The apptainer builds a native build of *target* reads from the cache.

    A Linux build ships apptainer for its own architecture. The macOS build
    ships the aarch64 one for its Linux VM and, as a release build under
    PEPPY_CROSS_ARCH, also requires the x86_64 one; either one missing would
    be built in a Lima VM.
    """
    if "apple-darwin" in target:
        return ("aarch64", "x86_64")
    return (_triple_arch(target),)


def _run_cargo_build(package: str, repo_root: Path, env: dict[str, str]) -> None:
    """Release-build *package* so its build script runs; cargo's output goes
    straight to the log."""
    console.print(f"Building [bold]{package}[/bold] in release...")
    result = subprocess.run(
        ["cargo", "build", "-p", package, "--release", "--locked"],
        cwd=repo_root,
        env=env,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"cargo build -p {package} failed (exit {result.returncode})"
        )


def _write_tarball(archive: Path, entries: Sequence[tuple[Path, str]]) -> None:
    """Write *entries* (source path, name in the archive) to a .tgz.

    A tarball rather than the directory itself because CI artifacts keep
    neither permissions nor symlinks, and apptainer needs both.
    """
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, "w:gz") as tar:
        for source, name in entries:
            tar.add(source, arcname=name)
    console.print(f"Wrote [bold]{archive}[/bold]")


def _extract_tree(archive: Path, destination: Path) -> Path:
    """Extract an archive holding one top-level directory and return it."""
    if not archive.is_file():
        raise ReleaseError(f"provisioned archive missing: {archive}")
    with tarfile.open(archive, "r:gz") as tar:
        tops = {Path(member.name).parts[0] for member in tar.getmembers()}
        if len(tops) != 1:
            raise ReleaseError(
                f"{archive} must hold a single top-level directory, "
                f"found {sorted(tops)}"
            )
        destination.mkdir(parents=True, exist_ok=True)
        tar.extractall(destination, filter="tar")
    return destination / tops.pop()


# --- prepare ---


def _require_unused_tag(tag: str) -> None:
    """Stop when *tag* already exists on the remote.

    GitHub attaches a release to an existing tag instead of creating it at the
    commit the release names, so the archives built from `dev` would ship
    under a tag that points somewhere else.
    """
    result = subprocess.run(
        ["git", "ls-remote", "--tags", GIT_REMOTE, f"refs/tags/{tag}"],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise ReleaseError(
            f"failed to list the tags on '{GIT_REMOTE}': {result.stderr.strip()}"
        )
    if result.stdout.strip():
        raise ReleaseError(
            f"tag '{tag}' already exists on {GIT_REMOTE}; a release needs a new tag"
        )


def _close_blocking_docs_gaps(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    docs_dir: Path,
    blocking: tuple[RequiredChange, ...],
) -> None:
    """Open the pull request closing *blocking* and stop the release.

    Returns only when the updater verifies that every reported gap is already
    documented, in which case the release continues.
    """
    console.print(f"[yellow]'{DOCS_DIR}/' is out of date:[/yellow]")
    for change in blocking:
        console.print(f"  [bold]{change.file}[/bold]: {change.change}")

    branch = f"{DOCS_SYNC_BRANCH_PREFIX}{release_commit[:12]}"
    pr_url = _find_open_docs_sync_pr(client, slug, branch)
    if pr_url:
        console.print(
            "[yellow]A docs pull request is already open for this commit.[/yellow]"
        )
        _stop_for_docs_pr(pr_url)

    console.print("Asking Claude to update the docs...")
    update = update_docs(f"{GIT_REMOTE}/{ALIGNED_BRANCH}", release_commit, blocking)
    console.print(update.summary)

    if not has_changes_in_paths([docs_dir]):
        if not update.all_already_covered:
            raise ReleaseError(
                f"the check reported '{DOCS_DIR}/' as out of date and the update "
                f"claimed to close gaps, but nothing changed there, so there is "
                f"no pull request to open. Update the docs by hand and push them "
                f"to '{RELEASE_BRANCH}', or rerun with --skip-docs-check if the "
                f"report is wrong."
            )
        console.print(
            f"[green]The updater verified every reported gap is already "
            f"documented; '{DOCS_DIR}/' covers the release.[/green]"
        )
        return

    _push_docs_sync_branch(branch, docs_dir, "docs: sync with the code being released")
    pr_url = _open_docs_pr(
        client,
        slug,
        branch,
        f"docs: sync with the code being released ({release_commit[:12]})",
        _docs_sync_pr_body(release_commit, blocking),
    )
    _stop_for_docs_pr(pr_url)


def _open_minor_docs_pr(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    docs_dir: Path,
    minor: tuple[RequiredChange, ...],
) -> None:
    """Open the optional pull request applying *minor*; the release goes on."""
    branch = f"{DOCS_POLISH_BRANCH_PREFIX}{release_commit[:12]}"
    pr_url = _find_open_docs_sync_pr(client, slug, branch)
    if pr_url:
        console.print(
            f"[yellow]A docs-polish pull request is already open for this "
            f"commit: {pr_url}[/yellow]"
        )
        return

    console.print("Asking Claude to apply the minor suggestions...")
    update = update_docs(f"{GIT_REMOTE}/{ALIGNED_BRANCH}", release_commit, minor)
    console.print(update.summary)

    if not has_changes_in_paths([docs_dir]):
        console.print(
            f"[yellow]The update changed nothing under '{DOCS_DIR}/'; there is "
            f"no pull request to open.[/yellow]"
        )
        return

    _push_docs_sync_branch(branch, docs_dir, "docs: minor polish")
    pr_url = _open_docs_pr(
        client,
        slug,
        branch,
        f"docs: minor polish ({release_commit[:12]})",
        _docs_polish_pr_body(release_commit, minor),
    )
    console.print(f"Opened {pr_url}; it does not block this release.")


def verify_docs_gate(
    client: httpx.Client,
    slug: RepoSlug,
    release_commit: str,
    repo_root: Path,
    *,
    open_minor_docs_pr: bool,
) -> None:
    """The docs freshness gate of the single-host release, answered up front.

    Makes the decisions of `build_release._verify_docs_up_to_date`: blocking
    gaps get a pull request against `dev` and stop the release, minor
    suggestions never block. That gate asks whether to open the optional
    minor-polish pull request; *open_minor_docs_pr* is the answer here.
    """
    docs_dir = repo_root / DOCS_DIR
    if has_changes_in_paths([docs_dir]):
        raise ReleaseError(
            f"'{DOCS_DIR}/' has uncommitted changes. The docs check commits "
            f"that directory onto a branch of its own, which would sweep those "
            f"edits into the pull request."
        )

    base = f"{GIT_REMOTE}/{ALIGNED_BRANCH}"
    console.print(f"Checking '{DOCS_DIR}/' covers the code changes since {base}...")
    result = check_docs(base, release_commit)
    print_minor_changes(result.minor)

    if result.blocking:
        _close_blocking_docs_gaps(
            client, slug, release_commit, docs_dir, result.blocking
        )
        return

    if result.minor and open_minor_docs_pr:
        _open_minor_docs_pr(client, slug, release_commit, docs_dir, result.minor)
    console.print(f"[green]'{DOCS_DIR}/' is up to date.[/green]")


def _draft_release_content(
    client: httpx.Client,
    slug: RepoSlug,
    tag: str,
    release_commit: str,
    repo_root: Path,
) -> ReleaseContent:
    """Draft the release content from the changes since the last release.

    The same draft the single-host release offers for review. Nobody reviews
    it here, so it is printed for the log instead.
    """
    latest = get_latest_release(client, slug)
    previous_tag = latest.get("tag_name") if latest else None
    if previous_tag:
        console.print(f"Listing changes since last release [bold]{previous_tag}[/bold]...")
    else:
        console.print(
            "[yellow]No previous published release found; "
            "listing the full history.[/yellow]"
        )

    changes = collect_release_changes(previous_tag, release_commit, repo_root)
    console.print("Asking Claude to write the release notes...")
    content = generate_release_content(changes, tag, repo_root)
    _print_release_content(content)
    return content


def run_prepare(
    *,
    tag: str,
    release_commit: str,
    open_minor_docs_pr: bool,
    skip_prod_cert_check: bool,
    skip_docs_check: bool,
    plan_path: Path,
) -> None:
    """Check the release can start from *release_commit* and write its plan."""
    token = validate_release_environment(
        required_commands=("git", "claude"),
        skip_prod_router_check=skip_prod_cert_check,
        token_env=RELEASE_TOKEN_ENV,
    )
    repo_root = get_repo_root()
    os.chdir(repo_root)

    dev_commit = _verify_release_branch_state()
    if dev_commit != release_commit:
        raise ReleaseError(
            f"'{RELEASE_BRANCH}' is at {dev_commit[:12]}, but this run builds "
            f"{release_commit[:12]}. Every stage builds the commit the run "
            f"started from; start a new run from the current '{RELEASE_BRANCH}'."
        )
    _require_unused_tag(tag)

    slug = github_repo_slug()
    client = build_github_client(token)

    if skip_docs_check:
        console.print(
            "[yellow]WARNING: skipping the docs freshness check "
            "(--skip-docs-check). This release may document behaviour that no "
            "longer matches the code.[/yellow]"
        )
    else:
        verify_docs_gate(
            client,
            slug,
            release_commit,
            repo_root,
            open_minor_docs_pr=open_minor_docs_pr,
        )

    content = _draft_release_content(client, slug, tag, release_commit, repo_root)
    ReleasePlan(tag=tag, release_commit=release_commit, content=content).write(
        plan_path
    )
    console.print(f"Wrote the release plan for [bold]{tag}[/bold]: {plan_path}")


# --- provisioning ---


def run_bindings(output_dir: Path) -> None:
    """Build the peppylib bindings of every release platform and pack them.

    A release build of the generator crate runs its build script in the
    release profile, which builds the macOS binding and cross-compiles both
    Linux ones, as the macOS release build does. PEPPYLIB_REBUILD forces them
    fresh as build_release.sh does, and the release RUSTFLAGS reach the nested
    binding builds the way they do inside a release build.
    """
    if not is_macos_arm64():
        raise ReleaseError(
            "the bindings of every platform build on macOS ARM64 only: the macOS "
            "binding cannot be cross-compiled from Linux"
        )
    for cmd in ("cargo", "rustc", "pixi"):
        need_cmd(cmd)
    repo_root = get_repo_root()
    os.chdir(repo_root)

    env = {
        **os.environ,
        "PEPPYLIB_REBUILD": "1",
        "RUSTFLAGS": _release_rustflags(repo_root),
    }
    _run_cargo_build("generator", repo_root, env)

    so_dir = require_prebuilt_peppylib_so()
    _write_tarball(
        output_dir / BINDINGS_ARCHIVE,
        [
            (so_dir / name, f"{BINDINGS_DIR_NAME}/{name}")
            for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER)
        ],
    )


def _single_apptainer_cache(arch: str) -> Path:
    """The one complete apptainer cache for *arch* under the build cache."""
    root = _build_cache_root()
    caches = [
        cache
        for cache in sorted(root.glob(_apptainer_cache_pattern(arch)))
        if (cache / "bin" / "apptainer").is_file()
    ]
    if len(caches) != 1:
        found = ", ".join(cache.name for cache in caches) or "none"
        raise ReleaseError(
            f"expected exactly one apptainer build for {arch} under {root}, "
            f"found {found}"
        )
    return caches[0]


def run_apptainer(output_dir: Path) -> None:
    """Build apptainer for this Linux host's architecture and pack it.

    A release build of the containers crate runs its build script, which
    builds apptainer from source into the build cache, or keeps a complete
    cache already there.
    """
    if not is_linux():
        raise ReleaseError(
            "apptainer builds natively on Linux only; run this stage on a Linux "
            "machine of the architecture it is for"
        )
    for cmd in ("cargo", "rustc"):
        need_cmd(cmd)
    repo_root = get_repo_root()
    os.chdir(repo_root)

    arch = _triple_arch(get_native_triple())
    _run_cargo_build("containers", repo_root, dict(os.environ))

    cache = _single_apptainer_cache(arch)
    _write_tarball(output_dir / apptainer_archive_name(arch), [(cache, cache.name)])


# --- build ---


def _seed_apptainer_cache(provisioned_dir: Path, arch: str) -> Path:
    """Unpack the provisioned apptainer for *arch* where the build looks for it."""
    tree = _extract_tree(
        provisioned_dir / apptainer_archive_name(arch), _build_cache_root()
    )
    if not fnmatch.fnmatch(tree.name, _apptainer_cache_pattern(arch)):
        raise ReleaseError(
            f"the provisioned apptainer for {arch} unpacked to '{tree.name}', "
            f"which is not a cache for {arch}"
        )
    return _single_apptainer_cache(arch)


def _unpack_bindings(provisioned_dir: Path, destination: Path) -> Path:
    """Unpack the provisioned bindings and return the directory holding them."""
    so_dir = _extract_tree(provisioned_dir / BINDINGS_ARCHIVE, destination)
    missing = [
        name
        for name in (*RELEASE_PLATFORM_SO, SO_BUILD_STATE_MARKER)
        if not (so_dir / name).is_file()
    ]
    if missing:
        raise ReleaseError(
            f"the provisioned bindings lack {', '.join(missing)}"
        )
    return so_dir


def run_build(*, plan_path: Path, target: str, provisioned_dir: Path) -> None:
    """Build and verify the release archive of *target* on a host of its own.

    The provisioned bindings are embedded through PEPPYLIB_PREBUILT_SO_DIR, the
    way the Lima build of the single-host release embeds them, and the
    provisioned apptainer builds fill the cache the build reads.
    """
    for cmd in ("git", "cargo", "rustc"):
        need_cmd(cmd)
    plan = ReleasePlan.load(plan_path)
    native = get_native_triple()
    if target != native:
        raise ReleaseError(
            f"this host builds {native}, not {target}: every archive is built "
            f"natively on a machine of its own platform"
        )

    repo_root = get_repo_root()
    os.chdir(repo_root)
    head = get_commit("HEAD")
    if head != plan.release_commit:
        raise ReleaseError(
            f"the checkout is at {head[:12]}, but the release plan builds "
            f"{plan.release_commit[:12]}"
        )

    for arch in _apptainer_arches_for(target):
        cache = _seed_apptainer_cache(provisioned_dir, arch)
        console.print(f"Seeded apptainer for {arch}: {cache}")

    with tempfile.TemporaryDirectory(prefix="peppylib-bindings-") as scratch:
        so_dir = _unpack_bindings(provisioned_dir, Path(scratch))
        os.environ["PEPPYLIB_PREBUILT_SO_DIR"] = str(so_dir)
        artifact = build_and_package(plan.tag, target, repo_root)

    verify_all_releases(artifact.asset_path.parent, [target])
    console.print(f"[green]Built and verified:[/green] {artifact.asset_path}", soft_wrap=True)


# --- publish ---


def _stage_archives(archives_dir: Path, dist_dir: Path) -> list[BuildArtifact]:
    """Copy the archive of every release target into the dist directory, where
    the pending-upload manifest and the publish step expect them."""
    dist_dir.mkdir(parents=True, exist_ok=True)
    artifacts: list[BuildArtifact] = []
    for triple in RELEASE_TRIPLES:
        name = f"peppy-{triple}.tgz"
        source = archives_dir / name
        if not source.is_file():
            raise ReleaseError(f"the {triple} archive is missing: {source}")
        destination = dist_dir / name
        shutil.copy2(source, destination)
        artifacts.append(BuildArtifact(name, destination, triple))
    return artifacts


def run_publish(*, plan_path: Path, archives_dir: Path) -> None:
    """Publish the planned release from the archives every build stage made.

    `dev` must still be at the commit the archives were built from: the
    release tags that commit and fast-forwards `main` to `dev`. Past that
    check this is the publish step of the single-host release, pending-upload
    manifest included, so a publish that fails leaves a dist directory that
    build_release.sh offers to publish on its next run.
    """
    need_cmd("git")
    token = require_release_token(RELEASE_TOKEN_ENV)
    plan = ReleasePlan.load(plan_path)

    repo_root = get_repo_root()
    os.chdir(repo_root)
    dev_commit = _verify_release_branch_state()
    if dev_commit != plan.release_commit:
        raise ReleaseError(
            f"'{RELEASE_BRANCH}' moved to {dev_commit[:12]} while the archives "
            f"were built from {plan.release_commit[:12]}. The release tags the "
            f"commit its archives come from and fast-forwards '{ALIGNED_BRANCH}' "
            f"to '{RELEASE_BRANCH}', so the two must match; start a new run "
            f"from the current '{RELEASE_BRANCH}'."
        )

    dist_dir = release_dist_dir(repo_root)
    artifacts = _stage_archives(archives_dir, dist_dir)
    verify_all_releases(dist_dir)
    console.print("[green]All release archives verified successfully.[/green]")

    manifest_path = pending_upload_path(repo_root)
    pending = record_pending_upload(
        manifest_path, plan.tag, plan.release_commit, plan.content, artifacts
    )
    slug = github_repo_slug()
    client = build_github_client(token)
    _publish_pending_upload(client, slug, pending, manifest_path, repo_root)


# --- command line ---


def _release_tag(value: str) -> str:
    tag = value.strip()
    if not tag:
        raise argparse.ArgumentTypeError("release tag cannot be empty")
    return tag


def _commit_sha(value: str) -> str:
    sha = value.strip().lower()
    if not re.fullmatch(r"[0-9a-f]{40}", sha):
        raise argparse.ArgumentTypeError(
            f"expected a full 40-character commit SHA, got {value!r}"
        )
    return sha


def _parse_args(argv: Sequence[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        prog="parallel-release",
        description=(
            "Build a peppy release on several machines at once, one stage at a "
            "time, then publish it. .github/workflows/parallel-release.yml runs "
            "the stages."
        ),
    )
    stages = parser.add_subparsers(dest="stage", required=True)

    prepare = stages.add_parser(
        "prepare",
        help="Check the release can start, run the docs gate, draft the notes.",
    )
    prepare.add_argument(
        "--tag",
        required=True,
        type=_release_tag,
        help="Tag of the release (example: v0.0.1).",
    )
    prepare.add_argument(
        "--release-commit",
        required=True,
        type=_commit_sha,
        help="The commit every stage builds; 'dev' must point at it.",
    )
    prepare.add_argument(
        "--minor-docs-pr",
        required=True,
        action=argparse.BooleanOptionalAction,
        help=(
            "Whether to open the optional pull request applying the docs "
            "check's minor suggestions. The release continues either way."
        ),
    )
    prepare.add_argument(
        "--skip-prod-cert-check",
        action="store_true",
        help=(
            "Skip the publicly-trusted prod-router certificate gate. The "
            "shipped CLI cannot federate to routers that are not publicly "
            "trusted."
        ),
    )
    prepare.add_argument(
        "--skip-docs-check",
        action="store_true",
        help=(
            "Skip the docs freshness gate; a release shipped this way can "
            "document behaviour that no longer exists."
        ),
    )
    prepare.add_argument(
        "--plan", required=True, type=Path, help="Where to write the release plan."
    )

    for name, help_text in (
        ("bindings", "Build the peppylib bindings of every platform (macOS ARM64)."),
        ("apptainer", "Build apptainer for this Linux host's architecture."),
    ):
        stage = stages.add_parser(name, help=help_text)
        stage.add_argument(
            "--output-dir",
            required=True,
            type=Path,
            help="Directory to write the provisioned archive to.",
        )

    build = stages.add_parser(
        "build", help="Build one release archive natively on this host."
    )
    build.add_argument("--plan", required=True, type=Path, help="The release plan.")
    build.add_argument(
        "--target",
        required=True,
        choices=RELEASE_TRIPLES,
        help="The release target this host builds.",
    )
    build.add_argument(
        "--provisioned",
        required=True,
        type=Path,
        help="Directory holding the bindings and apptainer archives.",
    )

    publish = stages.add_parser(
        "publish", help="Publish the release from the built archives."
    )
    publish.add_argument("--plan", required=True, type=Path, help="The release plan.")
    publish.add_argument(
        "--archives",
        required=True,
        type=Path,
        help="Directory holding the archive of every release target.",
    )

    return parser.parse_args(argv)


def _run_stage(args: argparse.Namespace) -> None:
    match args.stage:
        case "prepare":
            run_prepare(
                tag=args.tag,
                release_commit=args.release_commit,
                open_minor_docs_pr=args.minor_docs_pr,
                skip_prod_cert_check=args.skip_prod_cert_check,
                skip_docs_check=args.skip_docs_check,
                plan_path=args.plan,
            )
        case "bindings":
            run_bindings(args.output_dir)
        case "apptainer":
            run_apptainer(args.output_dir)
        case "build":
            run_build(
                plan_path=args.plan,
                target=args.target,
                provisioned_dir=args.provisioned,
            )
        case "publish":
            run_publish(plan_path=args.plan, archives_dir=args.archives)


def main() -> None:
    args = _parse_args()
    run_with_error_handling(lambda: _run_stage(args))
