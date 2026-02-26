"""Build peppy for the current host and upload it to an existing GitHub Release.

Requires:
  - GITHUB_PEPPY_RELEASE_TOKEN env var (repo-scoped token)
  - UV_PUBLISH_TOKEN env var (PyPI API token)
  - git, cargo, rustc, pixi on PATH

Usage:
  add-release-build

Then provide the release tag (example: v0.0.1). The script will upload an archive named like:
  peppy-x86_64-unknown-linux-gnu.tgz
"""

from __future__ import annotations

import os
import sys

from .build import build_and_package, publish_wheel
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
from .repo import (
    checkout,
    get_head_commit,
    get_repo_root,
    get_tag_commit,
    has_uncommitted_changes,
)


def _run() -> None:
    token = validate_release_environment(
        required_commands=("git", "cargo", "rustc", "pixi"),
    )
    validate_pypi_token()

    repo_root = get_repo_root()
    os.chdir(repo_root)

    if has_uncommitted_changes():
        if not prompt_yn("Working tree has uncommitted changes. Continue?"):
            sys.exit(1)

    tag = prompt("Release tag to update (example: v0.0.1)")
    if not tag:
        raise ReleaseError("release tag cannot be empty")

    # Resolve repo and fetch existing release
    slug = github_repo_slug()
    client = build_github_client(token)

    release_response = github_api(
        client,
        "GET",
        f"https://api.github.com/repos/{slug.full}/releases/tags/{tag}",
    )
    info = parse_release_response(release_response)

    # Verify HEAD matches tag commit
    tag_commit = get_tag_commit(tag)
    head_commit = get_head_commit()
    if tag_commit != head_commit:
        console.print(
            f"Current HEAD ({head_commit}) does not match tag '{tag}' ({tag_commit})."
        )
        if prompt_yn(f"Checkout '{tag}' before building?", default_yes=True):
            checkout(tag)
        else:
            sys.exit(1)

    # Build and package
    artifact = build_and_package(tag, repo_root)

    # Upload asset
    replace_and_upload_asset(
        client, info.release_id, artifact.asset_name, artifact.asset_path, slug
    )

    # Publish peppylib wheel to PyPI
    publish_wheel(tag, repo_root)

    # Final output
    release_url = info.html_url or f"https://github.com/{slug.full}/releases/tag/{tag}"
    console.print(f"\n[green]Release updated:[/green] {release_url}")


def main() -> None:
    run_with_error_handling(_run)
