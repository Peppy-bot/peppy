"""GitHub API client using httpx. Handles JSON validation, error reporting, asset management."""

from __future__ import annotations

import os
import random
import re
import subprocess
import time
from collections.abc import Iterator
from contextlib import contextmanager
from dataclasses import dataclass
from pathlib import Path
from threading import Event, Thread
from typing import Any

import httpx

from .cli import ReleaseError, console

_API_VERSION = "2022-11-28"
_DEFAULT_ACCEPT = "application/vnd.github+json"

_UPLOAD_TIMEOUT = 600.0
_UPLOAD_MAX_ATTEMPTS = 3
_UPLOAD_RETRY_BASE_DELAY = 5.0
_UPLOAD_CHUNK_SIZE = 1024 * 1024
_UPLOAD_PROGRESS_INTERVAL = 10.0
_RELEASES_PAGE_SIZE = 100
_MIB = 1024 * 1024
_RETRYABLE_STATUS_CODES = frozenset({502, 503, 504})

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
    none_on_404: bool = False,
) -> dict[str, Any] | list[Any] | None:
    """Make an authenticated GitHub API request.

    Validates that the response is JSON, and returns the parsed body.
    For 204/205 responses with empty body, returns an empty dict.
    When none_on_404 is True, a 404 response returns None instead of raising
    (used for "latest release" lookups, where 404 means "no release yet").
    Raises ReleaseError on other HTTP errors, non-JSON responses, or empty bodies.
    """
    headers = {"Accept": accept}
    try:
        if json_data is not None:
            response = client.request(method, url, json=json_data, headers=headers)
        else:
            response = client.request(method, url, headers=headers)

        response.raise_for_status()
    except httpx.HTTPStatusError as e:
        if none_on_404 and e.response.status_code == 404:
            return None
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


def _is_retryable(exc: Exception) -> bool:
    """Return True if the exception is transient and worth retrying."""
    if isinstance(exc, (httpx.TimeoutException, httpx.ConnectError)):
        return True
    if isinstance(exc, httpx.HTTPStatusError):
        return exc.response.status_code in _RETRYABLE_STATUS_CODES
    return False


def format_mib(size_bytes: float) -> str:
    """Format a byte count (or a byte rate) in mebibytes, for upload output."""
    return f"{size_bytes / _MIB:.1f} MiB"


class UploadProgress:
    """How much of one asset upload has gone out, for the upload's log lines.

    The upload advances the byte count as the connection takes each chunk of
    chunks(), while a reporting thread reads it on its own schedule, so the
    lines keep coming (at 0.0 MiB/s) while the connection is stalled.
    """

    def __init__(self, asset_name: str, total_bytes: int, started_at: float) -> None:
        self.asset_name = asset_name
        self.total_bytes = total_bytes
        self.sent_bytes = 0
        self._started_at = started_at
        self._reported_bytes = 0
        self._reported_at = started_at

    def chunks(self, path: Path) -> Iterator[bytes]:
        """Yield the file chunk by chunk, counting each once the connection took it."""
        with open(path, "rb") as f:
            while chunk := f.read(_UPLOAD_CHUNK_SIZE):
                yield chunk
                self.sent_bytes += len(chunk)

    def progress_line(self, now: float) -> str:
        """Describe the bytes sent so far and the rate since the previous line."""
        sent = self.sent_bytes
        elapsed = now - self._reported_at
        rate = (sent - self._reported_bytes) / elapsed if elapsed > 0 else 0.0
        self._reported_bytes, self._reported_at = sent, now
        percent = sent * 100 // self.total_bytes if self.total_bytes else 100
        return (
            f"  {self.asset_name}: {format_mib(sent)} of "
            f"{format_mib(self.total_bytes)} ({percent}%), {format_mib(rate)}/s"
        )

    def summary_line(self, now: float) -> str:
        """Describe the finished upload: how long it took and its average rate."""
        elapsed = now - self._started_at
        rate = self.total_bytes / elapsed if elapsed > 0 else 0.0
        return f"Uploaded {self.asset_name} in {elapsed:.0f}s ({format_mib(rate)}/s)."


@contextmanager
def _printing_progress(progress: UploadProgress) -> Iterator[None]:
    """Print a progress line every _UPLOAD_PROGRESS_INTERVAL seconds while the block runs."""
    stopped = Event()

    def print_until_stopped() -> None:
        while not stopped.wait(_UPLOAD_PROGRESS_INTERVAL):
            console.print(progress.progress_line(time.monotonic()))

    reporter = Thread(target=print_until_stopped, daemon=True)
    reporter.start()
    try:
        yield
    finally:
        stopped.set()
        reporter.join()


def github_upload_asset(
    client: httpx.Client,
    release_id: int,
    asset_name: str,
    asset_path: Path,
    slug: RepoSlug,
    *,
    timeout: float = _UPLOAD_TIMEOUT,
    max_attempts: int = _UPLOAD_MAX_ATTEMPTS,
) -> dict[str, Any]:
    """Upload a binary asset to a GitHub release with retry on transient failures.

    Uses the uploads.github.com endpoint with application/octet-stream.
    The file is streamed from disk, with a progress line every
    _UPLOAD_PROGRESS_INTERVAL seconds and a summary once GitHub accepted it.
    Retries on timeouts, connection errors, and HTTP 502/503/504.
    Cleans up partial uploads before each retry to avoid 422 conflicts.
    Returns the parsed JSON response from the upload.
    """
    upload_url = (
        f"https://uploads.github.com/repos/{slug.full}/releases/{release_id}"
        f"/assets?name={asset_name}"
    )
    total_bytes = asset_path.stat().st_size

    last_exc: Exception | None = None
    for attempt in range(max_attempts):
        if attempt > 0:
            delete_asset_if_exists(client, release_id, asset_name, slug)
            delay = _UPLOAD_RETRY_BASE_DELAY * (2 ** (attempt - 1)) + random.random()
            console.print(
                f"[yellow]Upload attempt {attempt} failed ({last_exc}), "
                f"retrying in {delay:.0f}s...[/yellow]"
            )
            time.sleep(delay)

        progress = UploadProgress(asset_name, total_bytes, time.monotonic())
        try:
            with _printing_progress(progress):
                response = client.post(
                    upload_url,
                    content=progress.chunks(asset_path),
                    headers={
                        "Accept": _DEFAULT_ACCEPT,
                        "Content-Type": "application/octet-stream",
                        # Given the length, httpx sends the streamed body as
                        # is instead of switching to chunked encoding.
                        "Content-Length": str(total_bytes),
                    },
                    timeout=httpx.Timeout(timeout),
                )
            response.raise_for_status()
            console.print(progress.summary_line(time.monotonic()))
            return response.json()
        except (httpx.HTTPStatusError, httpx.RequestError) as e:
            if not _is_retryable(e) or attempt == max_attempts - 1:
                if isinstance(e, httpx.HTTPStatusError):
                    body_preview = e.response.text[:2000] if e.response.text else "(empty)"
                    raise ReleaseError(
                        f"failed to upload asset '{asset_name}': "
                        f"HTTP {e.response.status_code}\n{body_preview}"
                    ) from e
                raise ReleaseError(
                    f"failed to upload asset '{asset_name}': {e}"
                ) from e
            last_exc = e

    # Unreachable, but satisfies type checker
    raise ReleaseError(f"failed to upload asset '{asset_name}': max retries exceeded")


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
    assets_url = (
        f"https://api.github.com/repos/{slug.full}/releases/{release_id}/assets"
    )
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
    console.print(
        f"Uploading [bold]{asset_name}[/bold] ({format_mib(asset_path.stat().st_size)})..."
    )
    github_upload_asset(client, release_id, asset_name, asset_path, slug)


def find_draft_releases(
    client: httpx.Client,
    slug: RepoSlug,
    tag: str,
) -> list[ReleaseInfo]:
    """Return every unpublished draft release of *tag*, across all release pages."""
    drafts: list[ReleaseInfo] = []
    page = 1
    while True:
        releases = github_api(
            client,
            "GET",
            f"https://api.github.com/repos/{slug.full}/releases"
            f"?per_page={_RELEASES_PAGE_SIZE}&page={page}",
        )
        if not isinstance(releases, list):
            raise ReleaseError(
                "unexpected GitHub API response for the release list (expected JSON array)"
            )
        drafts.extend(
            parse_release_response(release)
            for release in releases
            if isinstance(release, dict)
            and release.get("draft") is True
            and release.get("tag_name") == tag
        )
        if len(releases) < _RELEASES_PAGE_SIZE:
            return drafts
        page += 1


def delete_draft_release(
    client: httpx.Client,
    release_id: int,
    slug: RepoSlug,
) -> bool:
    """Delete a GitHub release by ID, only while it is still a draft.

    A publish request cut short on the client can still have gone through on
    GitHub, so the release a failure cleans up may already be live; that one
    is left alone. Returns True when the draft was deleted.
    """
    url = f"https://api.github.com/repos/{slug.full}/releases/{release_id}"
    release = github_api(client, "GET", url)
    if not isinstance(release, dict):
        raise ReleaseError(
            "unexpected GitHub API response for the release (expected JSON object)"
        )
    if release.get("draft") is not True:
        return False
    github_api(client, "DELETE", url)
    return True


def publish_release(
    client: httpx.Client,
    release_id: int,
    slug: RepoSlug,
) -> dict[str, Any] | list[Any] | None:
    """Publish a draft release by setting draft=False."""
    url = f"https://api.github.com/repos/{slug.full}/releases/{release_id}"
    return github_api(client, "PATCH", url, json_data={"draft": False})


def get_latest_release(
    client: httpx.Client,
    slug: RepoSlug,
) -> dict[str, Any] | None:
    """Return the latest published release JSON, or None if there are none.

    GitHub's releases/latest endpoint excludes drafts and prereleases and
    returns 404 when the repository has no published release yet.
    """
    result = github_api(
        client,
        "GET",
        f"https://api.github.com/repos/{slug.full}/releases/latest",
        none_on_404=True,
    )
    if result is None:
        return None
    if not isinstance(result, dict):
        raise ReleaseError(
            "unexpected GitHub API response for latest release (expected JSON object)"
        )
    return result
