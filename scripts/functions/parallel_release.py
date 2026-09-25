"""Build a peppy release on several machines at once, then publish it.

`.github/workflows/parallel-release.yml` runs the stages of this module on
separate runners, so each archive builds natively on a runner of its own
platform, all three at once, and no stage starts a VM:

- `prepare` (any host): the branch, tag and docs checks and the drafted release
  notes, written to a release plan that every later stage reads.
- `bindings` (macOS ARM64): the peppylib native bindings for every platform.
- `apptainer` (Linux): apptainer for the host's architecture.
- `build` (one per target): one release archive, built from the plan's commit.
- `hub-check` (Linux): every hub of the hub set the release recorded, read at
  its commit by the daemon of the host's archive, installed, and its
  repository index checked there (see release_install_check.py).
- `hub-launch` (any host): launchers-hub's tests, dispatched on the hub set the
  release recorded and on the x86_64 archive of the run, waited for until they
  succeed (see release_hubs.py).
- `publish` (Linux): the tag of the release on every hub of the set, a check
  that the archive of the host, installed, reads the default hubs at that
  tag, the GitHub release from the three archives, then the notes committed
  on `dev` and `main` fast-forwarded to it. A publish started again after a
  failure does only the steps the failure left undone.

The provisioning stages exist because a native build cannot produce everything
it embeds. Every peppy binary carries the peppylib bindings of all three
platforms, and only macOS builds the macOS one. The macOS build ships the
apptainer its Linux VM runs containers with, and a release build (under
PEPPY_CROSS_ARCH) looks for apptainer of both Linux architectures, building a
missing one in a Lima VM. Provisioned by the machines that build them natively,
the same bindings and apptainer reach every archive.

No stage asks a question: every answer a release needs is an option of the
stage that needs it, and the drafted notes are published as they are.
"""

from __future__ import annotations

import argparse
import fnmatch
import hashlib
import json
import os
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
from collections.abc import Sequence
from dataclasses import dataclass
from pathlib import Path

import httpx

from .build import (
    BUILD_COMMANDS,
    BuildArtifact,
    _release_rustflags,
    build_and_package,
    release_dist_dir,
)
from .cli import (
    RELEASE_TRIPLES,
    ReleaseError,
    console,
    get_native_triple,
    is_linux,
    is_macos_arm64,
    need_cmd,
    require_job_token,
    require_release_token,
    run_with_error_handling,
    validate_release_environment,
)
from .github import (
    ReleaseInfo,
    RepoSlug,
    build_github_client,
    delete_draft_release,
    find_draft_releases,
    find_published_release,
    github_api,
    github_repo_slug,
    parse_release_response,
    publish_release,
    replace_and_upload_asset,
)
from .hub_ci import load_release_set, parse_release_version, resolve
from .lima import (
    GO_LINUX_AMD64_SHA256,
    GO_LINUX_ARM64_SHA256,
    GO_VERSION,
    RELEASE_PLATFORM_SO,
    SO_BUILD_STATE_MARKER,
    require_prebuilt_peppylib_so,
)
from .release_docs_gate import verify_docs_gate
from .release_hubs import check_launchers, tag_release_hubs
from .release_install_check import check_hub_set, check_release_install
from .release_line import (
    ALIGNED_BRANCH,
    GIT_REMOTE,
    RELEASE_BRANCH,
    latest_release_tag,
    verify_release_branch_state,
)
from .release_notes import (
    ReleaseNotesInput,
    fetch_release_body_html,
    generate_release_notes_file,
    release_notes_file,
)
from .release_summary import (
    ReleaseContent,
    collect_release_changes,
    generate_release_content,
)
from .repo import (
    commit_paths,
    fetch_tag,
    get_changed_paths,
    get_commit,
    get_parents,
    get_repo_root,
    has_changes_in_paths,
    push_branch,
)
from .verify_release import verify_all_releases

# File names the provisioning stages write and the build stage reads.
BINDINGS_ARCHIVE = "peppylib-bindings.tgz"
BINDINGS_DIR_NAME = "so"


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
            tag=parse_release_version(fields["tag"]),
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


def _print_release_content(content: ReleaseContent) -> None:
    """Print the drafted release content, which nobody reviews before it ships."""
    console.print()
    console.print("[bold]Drafted release content[/bold]")
    console.print(f"  [bold]Title:[/bold] {content.title}")
    console.print(f"  [bold]Description:[/bold] {content.description}")
    console.print("  [bold]Notes:[/bold]")
    for line in content.notes.splitlines():
        console.print(f"    {line}")
    console.print()


def _draft_release_content(
    client: httpx.Client,
    slug: RepoSlug,
    tag: str,
    release_commit: str,
    repo_root: Path,
) -> ReleaseContent:
    """Draft the release content from the changes since the last release.

    Collects everything merged between the previous published release and the
    release commit (the commit subjects, the code diff, and the user
    documentation diff) and asks Claude to draft the title, description and
    notes as a self-contained list of user-facing changes. Nobody reviews the
    draft, so it is printed for the log.
    """
    previous_tag = latest_release_tag(client, slug)
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
    )
    repo_root = get_repo_root()
    os.chdir(repo_root)

    dev_commit = verify_release_branch_state()
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
    # peppylib-py's build script needs uv too, and would only say so minutes
    # into the generator build.
    for cmd in ("cargo", "rustc", "pixi", "uv"):
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


# Go's name for each architecture and the SHA-256 of its pinned toolchain.
_GO_TARBALLS = {
    "aarch64": ("arm64", GO_LINUX_ARM64_SHA256),
    "x86_64": ("amd64", GO_LINUX_AMD64_SHA256),
}


def _download(url: str, destination: Path) -> str:
    """Download *url* to *destination* and return the file's SHA-256."""
    digest = hashlib.sha256()
    try:
        with httpx.stream("GET", url, follow_redirects=True, timeout=60.0) as response:
            response.raise_for_status()
            with destination.open("wb") as out:
                for chunk in response.iter_bytes():
                    digest.update(chunk)
                    out.write(chunk)
    except httpx.HTTPError as e:
        raise ReleaseError(f"failed to download {url}: {e}") from e
    return digest.hexdigest()


def _install_pinned_go(arch: str) -> Path:
    """Install the Go toolchain the Lima builds pin and return its bin directory.

    containers-internal's native Linux build compiles apptainer with the `go`
    on PATH, which a runner image may lack or ship older than apptainer's
    `mconfig` accepts. The pinned, SHA-verified toolchain builds both Linux
    apptainers with the same Go whatever the runner image carries.
    """
    go_arch, sha256 = _GO_TARBALLS[arch]
    go_root = _build_cache_root() / f"go-{GO_VERSION}-{go_arch}"
    go_bin = go_root / "bin"
    if (go_bin / "go").is_file():
        return go_bin

    url = f"https://go.dev/dl/go{GO_VERSION}.linux-{go_arch}.tar.gz"
    console.print(f"Installing pinned Go {GO_VERSION} for {arch}...")
    go_root.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(dir=go_root.parent) as scratch:
        tarball = Path(scratch) / "go.tar.gz"
        if _download(url, tarball) != sha256:
            raise ReleaseError(f"{url} does not match its pinned SHA-256")
        tree = _extract_tree(tarball, Path(scratch) / "tree")
        shutil.rmtree(go_root, ignore_errors=True)
        tree.rename(go_root)
    return go_bin


def run_apptainer(output_dir: Path) -> None:
    """Build apptainer for this Linux host's architecture and pack it.

    A release build of the containers crate runs its build script, which
    builds apptainer from source into the build cache with the pinned Go, or
    keeps a complete cache already there.
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
    env = dict(os.environ)
    env["PATH"] = f"{_install_pinned_go(arch)}{os.pathsep}{env.get('PATH', '')}"
    # Never let the build swap the pinned toolchain for a downloaded one.
    env["GOTOOLCHAIN"] = "local"
    _run_cargo_build("containers", repo_root, env)

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
    way a Linux build in the Lima VM of `build_release.sh --local` embeds the
    bindings the macOS build made, and the provisioned apptainer builds fill
    the cache the build reads.
    """
    for cmd in BUILD_COMMANDS:
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
    the archive verification and the uploads read them."""
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


def _release_payload(plan: ReleasePlan) -> dict[str, object]:
    """The JSON payload for POST /repos/{owner}/{repo}/releases.

    Always a draft, published only once every archive is uploaded, and tagged
    at the exact commit the archives were built from rather than at a branch
    name, so a push to `dev` during the upload cannot retag the release.
    """
    return {
        "tag_name": plan.tag,
        "name": plan.content.title,
        "target_commitish": plan.release_commit,
        "draft": True,
        "body": plan.content.notes,
    }


def _create_draft_release(
    client: httpx.Client, slug: RepoSlug, plan: ReleasePlan
) -> ReleaseInfo:
    """Create the draft release the archives are uploaded to.

    The draft is invisible until every upload succeeds. Drafts of the same tag
    that an earlier attempt could not clean up (a killed process, a lost CI
    runner, a failed cleanup request) are deleted first, so the release never
    has more than one.
    """
    for draft in find_draft_releases(client, slug, plan.tag):
        console.print(
            f"[yellow]Deleting a draft release of {plan.tag} left by an "
            f"earlier run: {draft.html_url}[/yellow]",
            soft_wrap=True,
        )
        delete_draft_release(client, draft.release_id, slug)

    console.print(f"Creating draft release [bold]{slug.full}@{plan.tag}[/bold]...")
    response = github_api(
        client,
        "POST",
        f"{slug.api_url}/releases",
        json_data=_release_payload(plan),
    )
    return parse_release_response(response)


def _upload_archives_and_publish(
    client: httpx.Client,
    slug: RepoSlug,
    draft: ReleaseInfo,
    artifacts: list[BuildArtifact],
) -> ReleaseInfo:
    """Upload every archive to the draft, then publish it.

    Returns the published release, whose URL names its tag where the draft's
    names an untagged placeholder.

    The draft is deleted on any failure, an interrupt included (a cancelled
    CI run), so no half-uploaded release lingers on GitHub; the failure itself
    propagates to the caller.
    """
    try:
        for artifact in artifacts:
            replace_and_upload_asset(
                client, draft.release_id, artifact.asset_name, artifact.asset_path, slug
            )

        console.print("Publishing release...")
        return parse_release_response(publish_release(client, draft.release_id, slug))
    except BaseException:
        console.print(
            "[red]Upload or publish did not finish. Cleaning up draft release...[/red]"
        )
        try:
            if delete_draft_release(client, draft.release_id, slug):
                console.print("[yellow]Draft release deleted.[/yellow]")
            else:
                console.print(
                    f"[yellow]The release went live before the failure, so it is "
                    f"left in place: https://github.com/{slug.full}/releases[/yellow]",
                    soft_wrap=True,
                )
        except Exception as cleanup_err:
            console.print(
                f"[red]WARNING: Failed to delete draft release "
                f"(id={draft.release_id}): {cleanup_err}[/red]\n"
                f"[red]Manual cleanup required: "
                f"https://github.com/{slug.full}/releases[/red]"
            )
        raise


def _require_tag_at_release_commit(plan: ReleasePlan) -> None:
    """Stop unless the remote tag of *plan* points at the plan's commit.

    A published release of the tag is this run's release only when its tag
    names the commit the archives were built from. Any other commit means the
    tag was published from somewhere else, and nothing of this run belongs on
    it.
    """
    fetch_tag(GIT_REMOTE, plan.tag)
    tagged_commit = get_commit(f"refs/tags/{plan.tag}")
    if tagged_commit != plan.release_commit:
        raise ReleaseError(
            f"a release of {plan.tag} is already published, but its tag points "
            f"at {tagged_commit[:12]}, not at {plan.release_commit[:12]}, the "
            f"commit this run built its archives from. Nothing is published "
            f"over it; find out how {plan.tag} was published, and release this "
            f"commit under a new tag."
        )


def _find_published_release(
    client: httpx.Client, slug: RepoSlug, plan: ReleasePlan
) -> ReleaseInfo | None:
    """The published release of the plan's tag, or None while there is none.

    An earlier attempt of this publish that got past the publish request
    leaves one. It is taken for this run's release only once its tag is
    checked to point at the plan's commit (`_require_tag_at_release_commit`).
    """
    release = find_published_release(client, slug, plan.tag)
    if release is None:
        return None
    _require_tag_at_release_commit(plan)
    return release


def _host_archive(artifacts: list[BuildArtifact]) -> Path:
    """The archive of the platform this host runs, which the install check
    installs: the x86_64 Linux one on the publish job's runner."""
    native = get_native_triple()
    for artifact in artifacts:
        if artifact.target_triple == native:
            return artifact.asset_path
    raise ReleaseError(f"the release has no archive for this host's {native}")


def _publish_release_unless_published(
    client: httpx.Client,
    slug: RepoSlug,
    plan: ReleasePlan,
    hub_set: resolve.HubSet,
    *,
    notes_committed: bool,
    archives_dir: Path,
    repo_root: Path,
) -> ReleaseInfo:
    """The published GitHub release of *plan*, published from the archives in
    *archives_dir* unless an earlier attempt already published it.

    Before it publishes, it tags every hub of *hub_set* and checks that the
    installed archive reads the default hubs at that tag; a failure in either
    publishes nothing. A release already published skips both: its hubs were
    tagged and checked before it went live.

    Notes committed on `dev` without a published release are refused: only a
    publish commits them, and only once the release they describe is live.
    """
    release = _find_published_release(client, slug, plan)
    if release is not None:
        console.print(
            f"[yellow]{plan.tag} is already published ({release.html_url}); "
            f"its hubs are not tagged again and its archives are not uploaded "
            f"again.[/yellow]",
            soft_wrap=True,
        )
        return release
    if notes_committed:
        raise ReleaseError(
            f"the release notes of {plan.tag} are committed on "
            f"'{RELEASE_BRANCH}', but no published release of {plan.tag} "
            f"exists. Only a publish commits them, once the release is live, so "
            f"find out what removed the release before publishing it again."
        )

    dist_dir = release_dist_dir(repo_root)
    artifacts = _stage_archives(archives_dir, dist_dir)
    verify_all_releases(dist_dir)
    console.print("[green]All release archives verified successfully.[/green]")
    install_archive = _host_archive(artifacts)
    tag_release_hubs(client, slug.owner, hub_set, plan.tag)
    check_release_install(install_archive)
    draft = _create_draft_release(client, slug, plan)
    return _upload_archives_and_publish(client, slug, draft, artifacts)


def _is_release_notes_commit(
    commit: str, release_commit: str, notes_file: Path
) -> bool:
    """Whether *commit* is the notes commit of the release of *release_commit*.

    A publish commits the notes as a single commit on top of the release
    commit: *release_commit* is its only parent, and the release notes file of
    the tag (*notes_file*, relative to the repository root) is the only path
    it changes.
    """
    if get_parents(commit) != (release_commit,):
        return False
    return get_changed_paths(release_commit, commit) == (notes_file.as_posix(),)


def _dev_carries_release_notes(
    dev_commit: str, plan: ReleasePlan, notes_file: Path
) -> bool:
    """Whether `dev` already carries the release notes commit of *plan*.

    False when `dev` is at the plan's commit, where every publish starts. True
    when `dev` is that commit plus the notes commit alone
    (`_is_release_notes_commit`), which an earlier attempt leaves once it
    pushed the notes. Any other `dev` moved on since the archives were built,
    and the release cannot go on from it: it tags the commit its archives come
    from and fast-forwards `main` to `dev`.
    """
    if dev_commit == plan.release_commit:
        return False
    if _is_release_notes_commit(dev_commit, plan.release_commit, notes_file):
        return True
    raise ReleaseError(
        f"'{RELEASE_BRANCH}' moved to {dev_commit[:12]} while the archives "
        f"were built from {plan.release_commit[:12]}. The release tags the "
        f"commit its archives come from and fast-forwards '{ALIGNED_BRANCH}' "
        f"to '{RELEASE_BRANCH}', so the two must match; start a new run "
        f"from the current '{RELEASE_BRANCH}'."
    )


def _main_already_at(commit: str) -> bool:
    """Whether `origin/main`, as the branch check fetched it, is at *commit*."""
    return get_commit(f"{GIT_REMOTE}/{ALIGNED_BRANCH}") == commit


def _write_release_notes(
    client: httpx.Client,
    slug: RepoSlug,
    plan: ReleasePlan,
    release: ReleaseInfo,
    repo_root: Path,
) -> Path:
    """Write the docs release notes of the published *release*; return the file.

    The notes carry the release as GitHub renders it, its body as HTML and
    its publication date, so they are written from the published release
    rather than from the plan alone.
    """
    console.print("Fetching release notes...")
    body_html = fetch_release_body_html(client, release.release_id, slug)
    release_details = github_api(
        client,
        "GET",
        f"{slug.api_url}/releases/{release.release_id}",
    )
    if not isinstance(release_details, dict):
        raise ReleaseError(
            "unexpected GitHub API response for the release (expected JSON object)"
        )
    notes_input = ReleaseNotesInput(
        tag=plan.tag,
        description=plan.content.description,
        release_details=release_details,
        body_html=body_html,
    )
    return generate_release_notes_file(notes_input, repo_root)


def _commit_release_notes(notes_path: Path, tag: str) -> None:
    """Commit the release notes on `dev` and push it.

    The commit takes the notes file alone, so any other change in the working
    tree is left untouched. Notes the tree already holds as written leave
    nothing to commit, and `dev` as it is.
    """
    if not has_changes_in_paths([notes_path]):
        console.print(
            f"[yellow]'{RELEASE_BRANCH}' already holds the release notes of "
            f"{tag} as written; there is nothing to commit.[/yellow]"
        )
        return
    commit_paths([notes_path], f"docs: add release notes for {tag}")
    console.print(f"Pushing the release notes on '{RELEASE_BRANCH}' to {GIT_REMOTE}...")
    push_branch(GIT_REMOTE, RELEASE_BRANCH, RELEASE_BRANCH)


def _commit_notes_and_align_main(
    client: httpx.Client,
    slug: RepoSlug,
    plan: ReleasePlan,
    release: ReleaseInfo,
    *,
    notes_committed: bool,
    repo_root: Path,
) -> None:
    """Commit the notes of the published *release* on `dev`, then fast-forward
    `main` to `dev`, each unless an earlier attempt already did it.

    `main` moves with a refspec push, so the working tree never leaves `dev`.
    """
    if notes_committed:
        console.print(
            f"[yellow]The release notes of {plan.tag} are already committed on "
            f"'{RELEASE_BRANCH}'.[/yellow]"
        )
    else:
        notes_path = _write_release_notes(client, slug, plan, release, repo_root)
        _commit_release_notes(notes_path, plan.tag)

    if _main_already_at(get_commit("HEAD")):
        console.print(
            f"[yellow]'{ALIGNED_BRANCH}' is already at '{RELEASE_BRANCH}'.[/yellow]"
        )
        return
    console.print(f"Fast-forwarding '{ALIGNED_BRANCH}' to '{RELEASE_BRANCH}'...")
    push_branch(GIT_REMOTE, RELEASE_BRANCH, ALIGNED_BRANCH)


def run_publish(*, plan_path: Path, archives_dir: Path, hub_set_path: Path) -> None:
    """Publish the planned release, doing only the steps not done yet.

    A publish is these steps, in this order:

    1. `dev` is checked: it must be at the plan's commit, or that commit plus
       the notes commit alone. Any other `dev` took a merge since the build,
       and the stage stops before it writes anything, a hub tag included.
    2. The tag of the release, `peppy-release/<tag>`, is put on every hub of
       the hub set at the commit the set records (`tag_release_hubs`). A tag
       already there at that commit stays; one at another commit stops the
       stage.
    3. The archive of this host's platform, the x86_64 Linux one on the
       publish job's runner, is installed and must read every default hub at
       that tag (`check_release_install`).
    4. The GitHub release: a draft of the plan's tag at the plan's commit,
       every archive uploaded to it, then published.
    5. The release notes, written from the published release and committed on
       `dev`. Skipped when `dev` already carries that commit.
    6. `main` fast-forwarded to `dev`. Skipped when `origin/main` is there.

    Steps 2 to 4 are skipped when a published release of the tag exists and
    its tag points at the plan's commit. A failure in step 1, 2 or 3 publishes
    nothing; a hub tag without a published binary is read by no peppy, and a
    new run of the same version tests that hub at the tagged commit.

    A publish that fails is started again with "Re-run failed jobs" while the
    run's artifacts exist, and the steps the failed attempt finished are then
    found done.
    """
    need_cmd("git")
    token = require_release_token()
    plan = ReleasePlan.load(plan_path)
    hub_set = load_release_set(hub_set_path)

    repo_root = get_repo_root()
    os.chdir(repo_root)
    dev_commit = verify_release_branch_state()
    notes_committed = _dev_carries_release_notes(
        dev_commit, plan, release_notes_file(plan.tag)
    )

    slug = github_repo_slug()
    client = build_github_client(token)
    release = _publish_release_unless_published(
        client,
        slug,
        plan,
        hub_set,
        notes_committed=notes_committed,
        archives_dir=archives_dir,
        repo_root=repo_root,
    )

    # The release is live, so a failure past this point leaves only the git
    # side unfinished, which is what the re-run of this stage is for.
    try:
        _commit_notes_and_align_main(
            client,
            slug,
            plan,
            release,
            notes_committed=notes_committed,
            repo_root=repo_root,
        )
    except ReleaseError as e:
        raise ReleaseError(
            f"{e}\n"
            f"The GitHub release {plan.tag} is published ({release.html_url}), "
            f"so what is left is on '{RELEASE_BRANCH}' and '{ALIGNED_BRANCH}'. "
            f"Re-run the failed publish job: it finds the release published and "
            f"does only the steps left."
        ) from e

    console.print(
        f"[green]Release notes committed on '{RELEASE_BRANCH}' and "
        f"'{ALIGNED_BRANCH}' fast-forwarded to it.[/green] They feed "
        "https://forum.peppy.bot/c/peppy-os/announcements/6 and "
        "https://docs.peppy.bot/reference/changelog/"
    )
    # Last, so the run's output ends on the outcome and where to read it.
    # soft_wrap keeps the URL whole on a narrow CI console.
    console.print(
        f"\n[bold green]Released {plan.tag}.[/bold green] "
        f"Release notes: {release.html_url}",
        soft_wrap=True,
    )


# --- hub-check ---


def run_hub_check(*, hub_set_path: Path, archive: Path) -> None:
    """Check every hub of the hub set at its commit with *archive*, the archive
    of this host's platform: its daemon, installed, reads every hub, and each
    hub's repository index is checked at its commit (`check_hub_set`)."""
    need_cmd("git")
    check_hub_set(load_release_set(hub_set_path), archive)


# --- hub-launch ---


def run_hub_launch(*, hub_set_path: Path, peppy_run_id: int) -> None:
    """Run launchers-hub's tests on the hub set and on the archive of the
    release run *peppy_run_id*, and wait until they succeed.

    The release token dispatches the run. The job's own token reads it until
    it completes, which takes longer than the hour a release token lasts.
    """
    dispatch_token = require_release_token()
    read_token = require_job_token()
    hub_set = load_release_set(hub_set_path)
    slug = github_repo_slug()
    run = check_launchers(
        build_github_client(dispatch_token),
        build_github_client(read_token),
        slug.owner,
        hub_set,
        peppy_run_id,
        sleep=time.sleep,
        clock=time.monotonic,
    )
    console.print(
        f"[green]Every launcher of the hub set launches with this release:"
        f"[/green] {run.html_url}",
        soft_wrap=True,
    )


# --- command line ---


def _release_tag(value: str) -> str:
    try:
        return parse_release_version(value)
    except ReleaseError as e:
        raise argparse.ArgumentTypeError(str(e)) from e


def _run_id(value: str) -> int:
    run_id = value.strip()
    if not re.fullmatch(r"[1-9][0-9]*", run_id):
        raise argparse.ArgumentTypeError(
            f"expected the id of a workflow run, got {value!r}"
        )
    return int(run_id)


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
        help="Tag of the release, v<MAJOR>.<MINOR>.<PATCH> (example: v0.31.2).",
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

    hub_check = stages.add_parser(
        "hub-check",
        help="Check every hub of the hub set at its commit with this run's archive.",
    )
    hub_check.add_argument(
        "--hub-set", required=True, type=Path, help="The hub set of the release."
    )
    hub_check.add_argument(
        "--archive",
        required=True,
        type=Path,
        help="The archive of this host's platform, built by this run.",
    )

    hub_launch = stages.add_parser(
        "hub-launch",
        help="Run launchers-hub's tests on the hub set and this run's archive.",
    )
    hub_launch.add_argument(
        "--hub-set", required=True, type=Path, help="The hub set of the release."
    )
    hub_launch.add_argument(
        "--peppy-run-id",
        required=True,
        type=_run_id,
        help="The release run whose x86_64 archive the launchers-hub run installs.",
    )

    publish = stages.add_parser(
        "publish",
        help="Tag the hubs, check the install, and publish the release.",
    )
    publish.add_argument("--plan", required=True, type=Path, help="The release plan.")
    publish.add_argument(
        "--archives",
        required=True,
        type=Path,
        help="Directory holding the archive of every release target.",
    )
    publish.add_argument(
        "--hub-set",
        required=True,
        type=Path,
        help="The hub set of the release: the hub commits publish tags.",
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
        case "hub-check":
            run_hub_check(hub_set_path=args.hub_set, archive=args.archive)
        case "hub-launch":
            run_hub_launch(hub_set_path=args.hub_set, peppy_run_id=args.peppy_run_id)
        case "publish":
            run_publish(
                plan_path=args.plan,
                archives_dir=args.archives,
                hub_set_path=args.hub_set,
            )


def main() -> None:
    args = _parse_args()
    run_with_error_handling(lambda: _run_stage(args))
