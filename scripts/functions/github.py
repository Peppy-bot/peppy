"""GitHub API client using httpx. Handles JSON validation, error reporting, asset management."""

from __future__ import annotations

import os
import re
import subprocess
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import httpx

from .cli import ReleaseError, console

_API_VERSION = "2022-11-28"
_DEFAULT_ACCEPT = "application/vnd.github+json"

_GIT_REMOTE_PATTERNS: list[tuple[str, str]] = [
    ("git@github.com:", "git@github.com:"),
    ("https://github.com/", "https://github.com/"),
    ("http://github.com/", "http://github.com/"),
    ("ssh://git@github.com/", "ssh://git@github.com/"),
]


@dataclass(frozen=True)
class RepoSlug:
    """A GitHub owner/repo pair."""

    owner: str
    repo: str

    @property
    def full(self) -> str:
        return f"{self.owner}/{self.repo}"


@dataclass(frozen=True)
class ReleaseInfo:
    """Parsed id and html_url from a GitHub release API response."""

    release_id: int
    html_url: str


def _format_response_headers(headers: httpx.Headers) -> str:
    """Filter and format response headers for error diagnostics."""
    useful = re.compile(
        r"^(content-type|x-github-request-id|x-ratelimit-remaining|x-ratelimit-reset)$",
        re.IGNORECASE,
    )
    lines = []
    for name, value in headers.items():
        if useful.match(name):
            lines.append(f"  {name}: {value}")
    return "\n".join(lines)


def build_github_client(token: str) -> httpx.Client:
    """Create a reusable httpx.Client with GitHub auth and API version headers."""
    return httpx.Client(
        headers={
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": _API_VERSION,
        },
        follow_redirects=True,
        timeout=60.0,
    )


def github_api(
    client: httpx.Client,
    method: str,
    url: str,
    *,
    json_data: dict[str, Any] | None = None,
    accept: str = _DEFAULT_ACCEPT,
) -> dict[str, Any] | list[Any]:
    """Make an authenticated GitHub API request.

    Validates that the response is JSON, and returns the parsed body.
    For 204/205 responses with empty body, returns an empty dict.
    Raises ReleaseError on HTTP errors, non-JSON responses, or empty bodies.
    """
    headers = {"Accept": accept}
    try:
        if json_data is not None:
            response = client.request(method, url, json=json_data, headers=headers)
        else:
            response = client.request(method, url, headers=headers)

        response.raise_for_status()
    except httpx.HTTPStatusError as e:
        body_preview = e.response.text[:2000] if e.response.text else "(empty)"
        resp_headers = _format_response_headers(e.response.headers)
        raise ReleaseError(
            f"GitHub API request failed: {method} {url}\n"
            f"Status: {e.response.status_code}\n"
            f"Headers:\n{resp_headers}\n"
            f"Body: {body_preview}"
        ) from e
    except httpx.RequestError as e:
        raise ReleaseError(f"GitHub API request failed: {method} {url}: {e}") from e

    if not response.text.strip():
        if response.status_code in (204, 205):
            return {}
        resp_headers = _format_response_headers(response.headers)
        raise ReleaseError(
            f"GitHub API returned an empty response ({response.status_code}) "
            f"for {method} {url}\nHeaders:\n{resp_headers}"
        )

    try:
        return response.json()
    except ValueError as e:
        resp_headers = _format_response_headers(response.headers)
        body_preview = response.text[:2000]
        raise ReleaseError(
            f"GitHub API returned a non-JSON response ({response.status_code}) "
            f"for {method} {url}\n"
            f"Headers:\n{resp_headers}\n"
            f"Body: {body_preview}"
        ) from e


def github_upload_asset(
    client: httpx.Client,
    release_id: int,
    asset_name: str,
    asset_path: Path,
    slug: RepoSlug,
) -> dict[str, Any]:
    """Upload a binary asset to a GitHub release.

    Uses the uploads.github.com endpoint with application/octet-stream.
    Returns the parsed JSON response from the upload.
    """
    upload_url = (
        f"https://uploads.github.com/repos/{slug.full}/releases/{release_id}"
        f"/assets?name={asset_name}"
    )

    with open(asset_path, "rb") as f:
        data = f.read()

    try:
        response = client.post(
            upload_url,
            content=data,
            headers={
                "Accept": _DEFAULT_ACCEPT,
                "Content-Type": "application/octet-stream",
            },
        )
        response.raise_for_status()
    except httpx.HTTPStatusError as e:
        body_preview = e.response.text[:2000] if e.response.text else "(empty)"
        raise ReleaseError(
            f"failed to upload asset '{asset_name}': "
            f"HTTP {e.response.status_code}\n{body_preview}"
        ) from e
    except httpx.RequestError as e:
        raise ReleaseError(f"failed to upload asset '{asset_name}': {e}") from e

    return response.json()


def github_repo_slug() -> RepoSlug:
    """Determine the GitHub owner/repo from GITHUB_REPOSITORY env var or git remote.

    Resolution order:
    1. GITHUB_REPOSITORY env var (if set and non-empty)
    2. git remote 'origin' URL

    Raises ReleaseError if the slug cannot be determined or is invalid.
    """
    env_slug = os.environ.get("GITHUB_REPOSITORY", "").strip()
    if env_slug:
        return _parse_slug(env_slug)

    result = subprocess.run(
        ["git", "config", "--get", "remote.origin.url"],
        capture_output=True,
        text=True,
    )
    remote_url = result.stdout.strip()
    if not remote_url:
        raise ReleaseError(
            "could not determine repo "
            "(set GITHUB_REPOSITORY=owner/repo or configure git remote 'origin')"
        )

    for prefix, match_prefix in _GIT_REMOTE_PATTERNS:
        if remote_url.startswith(match_prefix):
            slug_str = remote_url[len(prefix) :]
            slug_str = slug_str.removesuffix(".git")
            return _parse_slug(slug_str)

    raise ReleaseError(f"unsupported remote url (expected github.com): {remote_url}")


def _parse_slug(slug_str: str) -> RepoSlug:
    """Parse an 'owner/repo' string into a RepoSlug.

    Raises ReleaseError if the slug is invalid (missing /, owner == repo after split).
    """
    slug_str = slug_str.strip("/")
    if "/" not in slug_str:
        raise ReleaseError(f"invalid repo slug (expected owner/repo): {slug_str}")

    owner, _, repo = slug_str.partition("/")
    if not owner or not repo or owner == repo:
        raise ReleaseError(f"invalid repo slug: {slug_str}")

    return RepoSlug(owner=owner, repo=repo)


def parse_release_response(response: dict[str, Any]) -> ReleaseInfo:
    """Extract release id and html_url from a GitHub release API response.

    Raises ReleaseError if the response is not a dict or the 'id' field is missing.
    """
    if not isinstance(response, dict):
        raise ReleaseError("unexpected GitHub API response (expected JSON object)")

    release_id = response.get("id")
    if release_id is None:
        msg = response.get("message") or "missing 'id' in response"
        raise ReleaseError(f"unexpected GitHub API response: {msg}")

    html_url = response.get("html_url", "")
    return ReleaseInfo(release_id=int(release_id), html_url=html_url)


def delete_asset_if_exists(
    client: httpx.Client,
    release_id: int,
    asset_name: str,
    slug: RepoSlug,
) -> None:
    """Delete a release asset by name if it exists.

    Lists all assets for the release, finds one matching asset_name,
    and DELETEs it. No-op if the asset doesn't exist.
    """
    assets_url = f"https://api.github.com/repos/{slug.full}/releases/{release_id}/assets"
    assets = github_api(client, "GET", assets_url)

    if not isinstance(assets, list):
        return

    for asset in assets:
        if isinstance(asset, dict) and asset.get("name") == asset_name:
            asset_id = asset.get("id")
            if asset_id is not None:
                delete_url = f"https://api.github.com/repos/{slug.full}/releases/assets/{asset_id}"
                github_api(client, "DELETE", delete_url)
            break


def replace_and_upload_asset(
    client: httpx.Client,
    release_id: int,
    asset_name: str,
    asset_path: Path,
    slug: RepoSlug,
) -> None:
    """Delete an existing asset (if any) and upload a new one."""
    delete_asset_if_exists(client, release_id, asset_name, slug)
    console.print(f"Uploading [bold]{asset_name}[/bold]...")
    github_upload_asset(client, release_id, asset_name, asset_path, slug)
