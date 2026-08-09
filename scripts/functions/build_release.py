"""Build and publish a GitHub Release for peppy.

On macOS ARM64: builds all 4 targets (native + Linux via Lima VM).
On Linux: builds the native target only (--local mode required).

A full release is cut from `dev` and nothing else: the archives are built from
the `dev` tip, the release notes are committed on `dev`, and `main` is then
fast-forwarded to that commit through a refspec push, so the working tree stays
on `dev` throughout.

Requires:
  - GITHUB_PEPPY_RELEASE_TOKEN env var (repo-scoped token) -- not needed with --local
  - git, cargo, rustc on PATH
  - Lima VM (macOS only, auto-managed)

Outputs:
  - Tar.gz archives in ./dist/
  - A release notes HTML file in ./docs/src/content/releases/ (unless --local),
    committed on `dev` and pushed to `dev` and `main`
"""

from __future__ import annotations

import argparse
import os
import shlex
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

import httpx

from .build import BuildArtifact, build_and_package
from .cli import (
    ReleaseError,
    console,
    get_targets_for_platform,
    is_macos_arm64,
    prompt,
    prompt_choice,
    prompt_yn,
    run_with_error_handling,
    validate_release_environment,
)
from .github import (
    RepoSlug,
    build_github_client,
    delete_release,
    get_latest_release,
    github_api,
    github_repo_slug,
    parse_release_response,
    publish_release,
    replace_and_upload_asset,
)
from .lima import ensure_lima_vm, ensure_rust_in_vm, find_limactl, stop_lima_vm
from .verify_release import verify_all_releases
from .release_notes import (
    ReleaseNotesInput,
    fetch_release_body_html,
    generate_release_notes_file,
)
from .release_summary import ReleaseContent, generate_release_content
from .docker import main as build_base_images_main
from .repo import (
    commit_paths,
    fetch_remote_branches,
    get_commit,
    get_commit_subjects,
    get_current_branch,
    get_repo_root,
    has_changes_in_paths,
    has_uncommitted_changes,
    is_ancestor,
    is_branch_checked_out,
    push_branch,
    set_branch_ref,
)

# A full release is cut from RELEASE_BRANCH, and ALIGNED_BRANCH is fast-forwarded
# to it once the release is published, so the two branches always agree on what
# has shipped.
GIT_REMOTE = "origin"
RELEASE_BRANCH = "dev"
ALIGNED_BRANCH = "main"


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and publish a GitHub Release for peppy."
    )
    parser.add_argument(
        "--local",
        action="store_true",
        help="Build release artifacts locally without uploading to GitHub.",
    )
    parser.add_argument(
        "--base-images",
        action="store_true",
        help="Build and push Docker base images to Docker Hub.",
    )
    parser.add_argument(
        "--skip-prod-cert-check",
        action="store_true",
        help=(
            "Skip the publicly-trusted prod-router certificate gate. Every other "
            "prod check still runs. Only use when the prod routers are known-good "
            "or not yet publicly trusted; the shipped CLI cannot federate to "
            "routers that are not publicly trusted."
        ),
    )
    return parser.parse_args()


def _open_editor(path: Path) -> None:
    """Open the user's preferred editor on the given file.

    EDITOR may include flags (for example "nano -c" or "code --wait"), so the
    command is split into argv rather than treated as a single executable name.
    """
    editor = os.environ.get("EDITOR", "").strip()
    if not editor:
        editor = "nano" if shutil.which("nano") else "vi"

    argv = shlex.split(editor) + [str(path)]
    try:
        result = subprocess.run(argv)
    except FileNotFoundError as e:
        raise ReleaseError(f"editor command not found: {editor!r} ({e})")
    if result.returncode != 0:
        raise ReleaseError(f"editor '{editor}' exited with code {result.returncode}")


_EDIT_HEADER = (
    "# Review and edit the release content below.\n"
    "# Lines starting with '#' are ignored.\n"
    "# Keep the 'Title:' and 'Description:' labels on their own lines,\n"
    "# and the 'Notes:' label on its own line above the Markdown body.\n"
)


def _print_release_content(content: ReleaseContent) -> None:
    """Print the proposed release content to the console for review."""
    console.print()
    console.print("[bold]Proposed release content[/bold]")
    console.print(f"  [bold]Title:[/bold] {content.title}")
    console.print(f"  [bold]Description:[/bold] {content.description}")
    console.print("  [bold]Notes:[/bold]")
    for line in content.notes.splitlines():
        console.print(f"    {line}")
    console.print()


def _render_editable(content: ReleaseContent) -> str:
    """Render release content into the labeled text format opened in the editor."""
    return (
        f"{_EDIT_HEADER}"
        f"Title: {content.title}\n"
        f"Description: {content.description}\n"
        f"Notes:\n"
        f"{content.notes}\n"
    )


def _parse_editable(text: str) -> ReleaseContent:
    """Parse the labeled editor text back into a ReleaseContent.

    The '#' comment convention applies only to the header region (above the
    'Notes:' label); everything below it is the body verbatim, because Markdown
    headings also start with '#'. Raises ReleaseError if a label is missing or
    a field is empty.
    """
    title: str | None = None
    description: str | None = None
    notes_lines: list[str] | None = None
    for line in text.splitlines():
        if notes_lines is not None:
            notes_lines.append(line)
        elif line.startswith("#"):
            continue
        elif title is None and line.startswith("Title:"):
            title = line[len("Title:") :].strip()
        elif description is None and line.startswith("Description:"):
            description = line[len("Description:") :].strip()
        elif line.strip() == "Notes:":
            notes_lines = []

    if title is None or description is None or notes_lines is None:
        raise ReleaseError(
            "edited content must keep the 'Title:', 'Description:' and 'Notes:' labels"
        )
    notes = "\n".join(notes_lines).strip()
    if not title or not description or not notes:
        raise ReleaseError("edited content has an empty title, description, or notes")
    return ReleaseContent(title=title, description=description, notes=notes)


def _edit_release_content(content: ReleaseContent) -> ReleaseContent:
    """Open the editor seeded with the content and parse the result.

    Returns the edited content, or the unchanged content if the edited text
    cannot be parsed (the caller re-prompts so the user can try again).
    """
    with tempfile.NamedTemporaryFile(
        mode="w",
        suffix=".md",
        prefix="peppy_release_",
        delete=False,
    ) as f:
        f.write(_render_editable(content))
        notes_path = Path(f.name)

    try:
        _open_editor(notes_path)
        text = notes_path.read_text(encoding="utf-8")
    finally:
        notes_path.unlink(missing_ok=True)

    try:
        return _parse_editable(text)
    except ReleaseError as e:
        console.print(f"[yellow]Keeping previous content: {e}[/yellow]")
        return content


def _confirm_release_content(content: ReleaseContent) -> ReleaseContent:
    """Show the generated content and let the user accept, edit, or abort."""
    while True:
        _print_release_content(content)
        choice = prompt_choice(
            "Use these release notes? (y)es, (e)dit, (a)bort",
            choices=("y", "e", "a"),
            default="y",
        )
        if choice == "y":
            return content
        if choice == "a":
            raise ReleaseError("release aborted by user")
        content = _edit_release_content(content)


def _prepare_release_content(
    client: httpx.Client,
    slug: RepoSlug,
    tag: str,
    repo_root: Path,
) -> ReleaseContent:
    """Derive the release content from the changes since the last release.

    Collects the commit subjects since the previous published release, asks
    Claude to draft the title/description/notes as a self-contained list of
    changes (no external links), then lets the user review, edit, or abort
    before anything is built or published.
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

    commits = get_commit_subjects(previous_tag)
    console.print("Asking Claude to write the release notes...")
    content = generate_release_content(commits, tag, repo_root)
    return _confirm_release_content(content)


def _build_release_payload(
    tag: str,
    title: str,
    target_commitish: str,
    notes_body: str,
) -> dict:
    """Build the JSON payload for POST /repos/{owner}/{repo}/releases."""
    return {
        "tag_name": tag,
        "name": title,
        "target_commitish": target_commitish,
        "draft": True,
        "body": notes_body,
    }


def _build_all_targets(
    tag: str,
    targets: list[str],
    repo_root: Path,
) -> list[BuildArtifact]:
    """Build and package for all requested targets.

    On macOS: builds the native target first (which triggers Lima download
    via the containers crate build.rs), then uses Lima for Linux targets.
    Targets are built sequentially to avoid cargo metadata conflicts.
    """
    artifacts: list[BuildArtifact] = []
    limactl: Path | None = None

    try:
        for triple in targets:
            if "linux" in triple and is_macos_arm64():
                if limactl is None:
                    limactl = find_limactl(repo_root)
                    ensure_lima_vm(limactl)
                    ensure_rust_in_vm(limactl)
                artifact = build_and_package(tag, triple, repo_root, limactl=limactl)
            else:
                artifact = build_and_package(tag, triple, repo_root)
            artifacts.append(artifact)

        # Verify the release archives that were actually built
        if artifacts:
            dist_dir = artifacts[0].asset_path.parent
            verify_all_releases(dist_dir, targets)
            console.print("[green]All release archives verified successfully.[/green]")

        return artifacts
    finally:
        # The Lima VM is only started for Linux cross-builds. Stop it once the
        # build finishes (successfully or not) so it does not keep holding host
        # RAM. ensure_lima_vm restarts a stopped instance on the next release.
        if limactl is not None:
            stop_lima_vm(limactl)


def _describe_release_branch_drift(local_commit: str, remote_commit: str) -> str:
    """Explain how the local release branch differs from its remote counterpart."""
    remote_ref = f"{GIT_REMOTE}/{RELEASE_BRANCH}"
    if is_ancestor(local_commit, remote_commit):
        return (
            f"'{RELEASE_BRANCH}' is behind {remote_ref}. "
            f"Run `git pull --ff-only` and retry."
        )
    if is_ancestor(remote_commit, local_commit):
        return (
            f"'{RELEASE_BRANCH}' has commits that are not on {remote_ref}. "
            f"Run `git push {GIT_REMOTE} {RELEASE_BRANCH}` and retry."
        )
    return (
        f"'{RELEASE_BRANCH}' and {remote_ref} have diverged. "
        f"Reconcile them and retry."
    )


def _verify_release_branch_state() -> str:
    """Check the git state a full release needs, before anything is built.

    The release publishes from the `dev` tip, commits the generated notes on
    `dev`, and fast-forwards `main` to that commit. All three are checked here
    so a branch problem costs nothing rather than surfacing after a cross-compile
    with a release already published:

    - HEAD is on `dev`, so the release is never cut from another branch,
    - `dev` matches `origin/dev`, so the final push cannot be rejected,
    - `origin/main` is an ancestor of `dev`, so `main` can fast-forward.

    Returns the `dev` commit the release is built from.
    """
    current_branch = get_current_branch()
    if current_branch != RELEASE_BRANCH:
        found = f"'{current_branch}'" if current_branch else "a detached commit"
        raise ReleaseError(
            f"releases are cut from '{RELEASE_BRANCH}' only, but HEAD is on {found}. "
            f"Run `git checkout {RELEASE_BRANCH}` and retry."
        )

    console.print(
        f"Checking '{RELEASE_BRANCH}' against {GIT_REMOTE} "
        f"(and that '{ALIGNED_BRANCH}' can fast-forward to it)..."
    )
    fetch_remote_branches(GIT_REMOTE, (RELEASE_BRANCH, ALIGNED_BRANCH))

    release_commit = get_commit("HEAD")
    remote_release_commit = get_commit(f"{GIT_REMOTE}/{RELEASE_BRANCH}")
    if release_commit != remote_release_commit:
        raise ReleaseError(
            _describe_release_branch_drift(release_commit, remote_release_commit)
        )

    remote_aligned_commit = get_commit(f"{GIT_REMOTE}/{ALIGNED_BRANCH}")
    if not is_ancestor(remote_aligned_commit, release_commit):
        raise ReleaseError(
            f"{GIT_REMOTE}/{ALIGNED_BRANCH} has commits that are not on "
            f"'{RELEASE_BRANCH}', so '{ALIGNED_BRANCH}' cannot fast-forward to it. "
            f"Merge {GIT_REMOTE}/{ALIGNED_BRANCH} into '{RELEASE_BRANCH}' and retry."
        )

    return release_commit


def _commit_notes_and_align_main(notes_path: Path, tag: str) -> None:
    """Commit the release notes on `dev`, push it, and fast-forward `main`.

    Runs only once the GitHub release is published. The commit takes the notes
    file alone, so any other change in the working tree is left untouched, and
    `main` is advanced with a refspec push plus a local `update-ref`, so the
    working tree never leaves `dev`.

    The local `main` ref is left alone when another worktree has it checked out:
    moving it there would leave that worktree's index disagreeing with its HEAD.
    """
    if has_changes_in_paths([notes_path]):
        commit_paths([notes_path], f"docs: add release notes for {tag}")
        console.print(f"Committed release notes on '{RELEASE_BRANCH}'.")
    else:
        console.print(
            f"[yellow]Release notes are already committed on "
            f"'{RELEASE_BRANCH}'; nothing to commit.[/yellow]"
        )

    console.print(f"Pushing '{RELEASE_BRANCH}' to {GIT_REMOTE}...")
    push_branch(GIT_REMOTE, RELEASE_BRANCH, RELEASE_BRANCH)

    console.print(f"Fast-forwarding '{ALIGNED_BRANCH}' to '{RELEASE_BRANCH}'...")
    push_branch(GIT_REMOTE, RELEASE_BRANCH, ALIGNED_BRANCH)

    if is_branch_checked_out(ALIGNED_BRANCH):
        console.print(
            f"[yellow]{GIT_REMOTE}/{ALIGNED_BRANCH} is updated, but the local "
            f"'{ALIGNED_BRANCH}' ref was left alone because a worktree has it "
            f"checked out. Run `git pull --ff-only` in that worktree.[/yellow]"
        )
        return
    set_branch_ref(ALIGNED_BRANCH, get_commit(RELEASE_BRANCH))


def _run_local() -> None:
    """Build release artifacts locally without uploading to GitHub."""
    validate_release_environment(require_token=False)
    repo_root = get_repo_root()
    os.chdir(repo_root)

    if has_uncommitted_changes():
        if not prompt_yn("Working tree has uncommitted changes. Continue?"):
            sys.exit(1)

    tag = prompt("Tag for the build (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    targets = get_targets_for_platform()
    artifacts = _build_all_targets(tag, targets, repo_root)

    console.print()
    for artifact in artifacts:
        console.print(f"[green]Built:[/green] {artifact.asset_path}")


def _run_full(skip_prod_cert_check: bool = False) -> None:
    """Build all 3 targets and publish a full GitHub release from `dev`.

    Only allowed on macOS ARM64, because a complete release requires
    all 3 targets (macOS + 2 Linux) and macOS cannot be built from Linux.
    """
    if not is_macos_arm64():
        raise ReleaseError(
            "full releases can only be created from macOS ARM64 "
            "(a release must contain all 3 targets: "
            "macos-aarch64, linux-x86_64, linux-aarch64)"
        )

    token = validate_release_environment(
        required_commands=("git", "cargo", "rustc", "claude"),
        skip_prod_router_check=skip_prod_cert_check,
    )
    repo_root = get_repo_root()
    os.chdir(repo_root)

    release_commit = _verify_release_branch_state()

    if has_uncommitted_changes():
        if not prompt_yn(
            "Working tree has uncommitted changes; they end up in the archives "
            "but not in the tagged commit. Continue?"
        ):
            sys.exit(1)

    # The tag is the only value typed by hand; Claude writes the rest.
    tag = prompt("Tag of the release (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    # Resolve the repo slug and client up front so the release notes can be
    # drafted from the changes since the last release, and reviewed, before
    # the long cross-compile starts.
    slug = github_repo_slug()
    client = build_github_client(token)
    content = _prepare_release_content(client, slug, tag, repo_root)

    # Build and package all targets
    targets = get_targets_for_platform()
    artifacts = _build_all_targets(tag, targets, repo_root)

    # Create a draft release (invisible until all uploads succeed). The tag is
    # pinned to the exact commit the archives were built from rather than to the
    # branch name, so a push to `dev` during the build cannot retag the release.
    payload = _build_release_payload(
        tag, content.title, release_commit, content.notes
    )
    console.print(f"Creating draft release [bold]{slug.full}@{tag}[/bold]...")
    release_response = github_api(
        client,
        "POST",
        f"https://api.github.com/repos/{slug.full}/releases",
        json_data=payload,
    )
    info = parse_release_response(release_response)

    # Upload all artifacts, then publish. Clean up the draft on any failure.
    try:
        for artifact in artifacts:
            replace_and_upload_asset(
                client, info.release_id, artifact.asset_name, artifact.asset_path, slug
            )

        console.print("Publishing release...")
        publish_release(client, info.release_id, slug)
    except Exception:
        console.print("[red]Upload or publish failed. Cleaning up draft release...[/red]")
        try:
            delete_release(client, info.release_id, slug)
            console.print("[yellow]Draft release deleted.[/yellow]")
        except Exception as cleanup_err:
            console.print(
                f"[red]WARNING: Failed to delete draft release "
                f"(id={info.release_id}): {cleanup_err}[/red]\n"
                f"[red]Manual cleanup required: "
                f"https://github.com/{slug.full}/releases[/red]"
            )
        raise

    # Fetch and write release notes for docs (only after publish succeeds)
    console.print("Fetching release notes...")
    body_html = fetch_release_body_html(client, info.release_id, slug)
    release_details = github_api(
        client,
        "GET",
        f"https://api.github.com/repos/{slug.full}/releases/{info.release_id}",
    )

    notes_input = ReleaseNotesInput(
        tag=tag,
        description=content.description,
        release_details=release_details,
        body_html=body_html,
    )
    releases_dir = repo_root / "docs" / "src" / "content" / "releases"
    notes_path = generate_release_notes_file(notes_input, releases_dir)

    release_url = release_details.get("html_url") or f"https://github.com/{slug.full}/releases/tag/{tag}"
    console.print(f"\n[green]Release created:[/green] {release_url}")

    # The release is live, so a git failure past this point leaves only the
    # docs side unfinished; say exactly how to finish it by hand.
    try:
        _commit_notes_and_align_main(notes_path, tag)
    except ReleaseError as e:
        raise ReleaseError(
            f"{e}\n"
            f"The GitHub release {tag} is published; only the git side is "
            f"unfinished. From '{RELEASE_BRANCH}', complete it with:\n"
            f"  git add {notes_path}\n"
            f'  git commit -m "docs: add release notes for {tag}"\n'
            f"  git push {GIT_REMOTE} {RELEASE_BRANCH}\n"
            f"  git push {GIT_REMOTE} {RELEASE_BRANCH}:{ALIGNED_BRANCH}"
        ) from e

    console.print(
        f"[green]Release notes committed on '{RELEASE_BRANCH}' and "
        f"'{ALIGNED_BRANCH}' fast-forwarded to it.[/green] They feed "
        "https://forum.peppy.bot/c/peppy-os/announcements/6 and "
        "https://docs.peppy.bot/reference/changelog/"
    )


def main() -> None:
    args = _parse_args()
    if args.base_images:
        build_base_images_main()
    elif args.local:
        run_with_error_handling(_run_local)
    else:
        run_with_error_handling(lambda: _run_full(args.skip_prod_cert_check))
