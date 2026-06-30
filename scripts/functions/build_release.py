"""Build and publish a GitHub Release for peppy.

On macOS ARM64: builds all 4 targets (native + Linux via Lima VM).
On Linux: builds the native target only (--local mode required).

Requires:
  - GITHUB_PEPPY_RELEASE_TOKEN env var (repo-scoped token) -- not needed with --local
  - git, cargo, rustc on PATH
  - Lima VM (macOS only, auto-managed)

Outputs:
  - Tar.gz archives in ./dist/
  - A release notes HTML file in ./docs/src/content/releases/ (unless --local)
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
from .lima import ensure_lima_vm, ensure_rust_in_vm, find_limactl
from .verify_release import verify_all_releases
from .release_notes import (
    ReleaseNotesInput,
    fetch_release_body_html,
    generate_release_notes_file,
)
from .release_summary import ReleaseContent, generate_release_content
from .docker import main as build_base_images_main
from .repo import (
    get_commit_subjects,
    get_current_branch,
    get_repo_root,
    has_uncommitted_changes,
)


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
    """Build all 3 targets and publish a full GitHub release.

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

    # Branch targeting check
    target_commitish = os.environ.get("PEPPY_RELEASE_TARGET", "main")
    current_branch = get_current_branch()
    if current_branch and current_branch != target_commitish:
        if not prompt_yn(
            f"Current branch is '{current_branch}'. "
            f"Release will target '{target_commitish}'. Continue?",
        ):
            sys.exit(1)

    if has_uncommitted_changes():
        if not prompt_yn("Working tree has uncommitted changes. Continue?"):
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

    # Create a draft release (invisible until all uploads succeed)
    payload = _build_release_payload(
        tag, content.title, target_commitish, content.notes
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
    generate_release_notes_file(notes_input, releases_dir)

    # Final output
    release_url = release_details.get("html_url") or f"https://github.com/{slug.full}/releases/tag/{tag}"
    console.print(f"\n[green]Release created:[/green] {release_url}")
    console.print(
        "[red]Do not forget to commit the new release note[/red] "
        "(to update https://forum.peppy.bot/c/peppy-os/announcements/6 "
        "and https://docs.peppy.bot/reference/changelog/)"
    )


def main() -> None:
    args = _parse_args()
    if args.base_images:
        build_base_images_main()
    elif args.local:
        run_with_error_handling(_run_local)
    else:
        run_with_error_handling(lambda: _run_full(args.skip_prod_cert_check))
