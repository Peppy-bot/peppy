"""Build and publish a GitHub Release for peppy for the current host architecture.

Requires:
  - GITHUB_PEPPY_RELEASE_TOKEN env var (repo-scoped token) — not needed with --local
  - git, cargo, rustc on PATH

Outputs:
  - A tar.gz archive in ./dist/
  - A release notes HTML file in ./docs/src/content/releases/ (unless --local)
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

from .build import build_and_package, build_wheel, publish_wheel
from .cli import (
    ReleaseError,
    console,
    prompt,
    prompt_yn,
    run_with_error_handling,
    validate_pypi_token,
    validate_release_environment,
)
from .github import (
    build_github_client,
    github_api,
    github_repo_slug,
    parse_release_response,
    replace_and_upload_asset,
)
from .release_notes import (
    ReleaseNotesInput,
    fetch_release_body_html,
    generate_release_notes_file,
)
from .repo import get_current_branch, get_repo_root, has_uncommitted_changes


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Build and publish a GitHub Release for peppy."
    )
    parser.add_argument(
        "--local",
        action="store_true",
        help="Build the release artifact locally without uploading to GitHub.",
    )
    return parser.parse_args()


def _open_editor(path: Path) -> None:
    """Open the user's preferred editor on the given file."""
    editor = os.environ.get("EDITOR", "")
    if not editor:
        editor = "nano" if shutil.which("nano") else "vi"

    result = subprocess.run([editor, str(path)])
    if result.returncode != 0:
        raise ReleaseError(f"editor '{editor}' exited with code {result.returncode}")


def _get_release_notes_via_editor() -> str:
    """Open an editor for manual release notes, strip comment lines, return content."""
    with tempfile.NamedTemporaryFile(
        mode="w",
        suffix=".md",
        prefix="peppy_release_notes_",
        delete=False,
    ) as f:
        f.write("# Write release notes below. Lines starting with # will be ignored.\n")
        f.write("# Save and close the editor when you're done.\n")
        f.write("\n")
        notes_path = Path(f.name)

    try:
        _open_editor(notes_path)
        text = notes_path.read_text(encoding="utf-8")
        lines = [line for line in text.splitlines() if not line.startswith("#")]
        return "\n".join(lines).strip() + "\n" if any(lines) else ""
    finally:
        notes_path.unlink(missing_ok=True)


def _build_release_payload(
    tag: str,
    title: str,
    target_commitish: str,
    generate_notes: bool,
    notes_body: str | None,
) -> dict:
    """Build the JSON payload for POST /repos/{owner}/{repo}/releases."""
    data: dict = {
        "tag_name": tag,
        "name": title,
        "target_commitish": target_commitish,
    }
    if generate_notes:
        data["generate_release_notes"] = True
    else:
        data["body"] = notes_body or ""
    return data


def _run_local() -> None:
    """Build a release artifact locally without uploading to GitHub."""
    validate_release_environment(
        required_commands=("git", "cargo", "rustc", "pixi"),
        require_token=False,
    )
    repo_root = get_repo_root()
    os.chdir(repo_root)

    if has_uncommitted_changes():
        if not prompt_yn("Working tree has uncommitted changes. Continue?"):
            sys.exit(1)

    tag = prompt("Tag for the build (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    artifact = build_and_package(tag, repo_root)
    build_wheel(tag, repo_root)
    console.print(f"\n[green]Built artifact:[/green] {artifact.asset_path}")


def _run_full() -> None:
    """Build and publish a full GitHub release."""
    token = validate_release_environment(
        required_commands=("git", "cargo", "rustc", "pixi"),
    )
    validate_pypi_token()

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

    # Interactive prompts
    tag = prompt("Tag of the release (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    title = prompt("Release title")
    if not title:
        raise ReleaseError("release title cannot be empty")

    description = prompt("Docs release description (shows on changelog page)", title)
    if not description:
        raise ReleaseError("docs release description cannot be empty")

    # Release notes method
    generate_notes = prompt_yn(
        "Generate release notes automatically?", default_yes=True
    )
    notes_body: str | None = None
    if not generate_notes:
        notes_body = _get_release_notes_via_editor()

    # Build and package
    artifact = build_and_package(tag, repo_root)

    # Resolve repo slug and create client
    slug = github_repo_slug()
    client = build_github_client(token)

    # Create the GitHub release
    payload = _build_release_payload(
        tag, title, target_commitish, generate_notes, notes_body
    )
    console.print(f"Creating GitHub release [bold]{slug.full}@{tag}[/bold]...")
    release_response = github_api(
        client,
        "POST",
        f"https://api.github.com/repos/{slug.full}/releases",
        json_data=payload,
    )
    info = parse_release_response(release_response)

    # Upload asset
    replace_and_upload_asset(
        client, info.release_id, artifact.asset_name, artifact.asset_path, slug
    )

    # Fetch and write release notes for docs
    console.print("Fetching release notes...")
    body_html = fetch_release_body_html(client, info.release_id, slug)
    release_details = github_api(
        client,
        "GET",
        f"https://api.github.com/repos/{slug.full}/releases/{info.release_id}",
    )

    notes_input = ReleaseNotesInput(
        tag=tag,
        description=description,
        release_details=release_details,
        body_html=body_html,
    )
    releases_dir = repo_root / "docs" / "src" / "content" / "releases"
    generate_release_notes_file(notes_input, releases_dir)

    # Publish peppylib wheel to PyPI
    publish_wheel(tag, repo_root)

    # Final output
    release_url = info.html_url or f"https://github.com/{slug.full}/releases/tag/{tag}"
    console.print(f"\n[green]Release created:[/green] {release_url}")
    console.print(
        "[red]Do not forget to commit the new release note[/red] "
        "(to update https://forum.peppy.bot/c/peppy-os/announcements/6 "
        "and https://docs.peppy.bot/reference/changelog/)"
    )


def main() -> None:
    args = _parse_args()
    if args.local:
        run_with_error_handling(_run_local)
    else:
        run_with_error_handling(_run_full)
