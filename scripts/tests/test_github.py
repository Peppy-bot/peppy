"""Tests for functions.github module."""

from __future__ import annotations

import re
from pathlib import Path
from unittest.mock import patch

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import (
    RepoSlug,
    UploadProgress,
    delete_draft_release,
    find_draft_releases,
    find_published_release,
    get_latest_release,
    github_api,
    github_upload_asset,
    publish_release,
)

SLUG = RepoSlug(owner="test-owner", repo="test-repo")
UPLOAD_URL = (
    "https://uploads.github.com/repos/test-owner/test-repo/releases/1/assets"
)
API_BASE = "https://api.github.com/repos/test-owner/test-repo"


@pytest.fixture()
def asset_file(tmp_path: Path) -> Path:
    p = tmp_path / "peppy-test.tgz"
    p.write_bytes(b"fake-archive-data")
    return p


# --- Upload: happy path ---


def test_upload_asset_success(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        return_value=httpx.Response(201, json={"id": 99, "name": "peppy-test.tgz"})
    )

    result = github_upload_asset(
        github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=1
    )
    assert result["id"] == 99


# --- Upload: streamed with progress ---


class _TicksThenStops:
    """Stands in for the reporter's Event: *ticks* intervals elapse,
    then the upload is over, whatever the host's speed."""

    ticks = 0

    def __init__(self) -> None:
        self._remaining = self.ticks

    def wait(self, timeout: float) -> bool:
        if self._remaining == 0:
            return True
        self._remaining -= 1
        return False

    def set(self) -> None:
        pass


def test_upload_asset_streams_the_whole_file_with_its_length(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    tmp_path: Path,
) -> None:
    # Larger than one chunk, so the body goes out in several pieces.
    data = bytes(range(256)) * 10_000
    asset = tmp_path / "peppy-big.tgz"
    asset.write_bytes(data)
    received: list[httpx.Request] = []

    def accept(request: httpx.Request) -> httpx.Response:
        request.read()
        received.append(request)
        return httpx.Response(201, json={"id": 99})

    mock_api.post(f"{UPLOAD_URL}?name=peppy-big.tgz").mock(side_effect=accept)

    github_upload_asset(github_client, 1, "peppy-big.tgz", asset, SLUG, max_attempts=1)

    (request,) = received
    assert request.content == data
    assert request.headers["Content-Length"] == str(len(data))
    assert "Transfer-Encoding" not in request.headers


@patch("functions.github.Event", type("Ticks", (_TicksThenStops,), {"ticks": 2}))
@patch("functions.github.time.monotonic", side_effect=[100.0, 105.0, 110.0, 125.0])
def test_upload_asset_prints_progress_lines_then_a_summary(
    mock_monotonic: object,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    asset = tmp_path / "peppy-big.tgz"
    asset.write_bytes(b"x" * (5 * 1024 * 1024))
    mock_api.post(f"{UPLOAD_URL}?name=peppy-big.tgz").mock(
        return_value=httpx.Response(201, json={"id": 99})
    )

    github_upload_asset(github_client, 1, "peppy-big.tgz", asset, SLUG, max_attempts=1)

    lines = capfd.readouterr().err.rstrip().splitlines()
    # One line per elapsed interval (their byte counts depend on how far the
    # upload got meanwhile), then the summary over the whole 25s.
    assert len(lines) == 3
    progress = re.compile(
        r"^  peppy-big\.tgz: \d+\.\d MiB of 5\.0 MiB \(\d+%\), \d+\.\d MiB/s$"
    )
    assert all(progress.match(line) for line in lines[:2])
    assert lines[2] == "Uploaded peppy-big.tgz in 25s (0.2 MiB/s)."


def test_upload_progress_counts_each_chunk_the_connection_took(tmp_path: Path) -> None:
    asset = tmp_path / "peppy-big.tgz"
    asset.write_bytes(b"x" * (2 * 1024 * 1024 + 10))
    progress = UploadProgress("peppy-big.tgz", 2 * 1024 * 1024 + 10, started_at=0.0)

    chunks = progress.chunks(asset)
    assert len(next(chunks)) == 1024 * 1024
    # A chunk only counts once the connection asks for the next one.
    assert progress.sent_bytes == 0
    next(chunks)
    assert progress.sent_bytes == 1024 * 1024
    assert len(next(chunks)) == 10
    assert list(chunks) == []
    assert progress.sent_bytes == 2 * 1024 * 1024 + 10


def test_upload_progress_line_reports_the_rate_since_the_previous_line() -> None:
    progress = UploadProgress("peppy.tgz", 100 * 1024 * 1024, started_at=0.0)

    progress.sent_bytes = 40 * 1024 * 1024
    assert progress.progress_line(10.0) == "  peppy.tgz: 40.0 MiB of 100.0 MiB (40%), 4.0 MiB/s"
    progress.sent_bytes = 50 * 1024 * 1024
    assert progress.progress_line(20.0) == "  peppy.tgz: 50.0 MiB of 100.0 MiB (50%), 1.0 MiB/s"
    # A stalled connection shows as lines that stop moving.
    assert progress.progress_line(30.0) == "  peppy.tgz: 50.0 MiB of 100.0 MiB (50%), 0.0 MiB/s"


def test_upload_progress_handles_an_empty_file_and_no_elapsed_time() -> None:
    progress = UploadProgress("empty.tgz", 0, started_at=5.0)

    assert progress.progress_line(5.0) == "  empty.tgz: 0.0 MiB of 0.0 MiB (100%), 0.0 MiB/s"
    assert progress.summary_line(5.0) == "Uploaded empty.tgz in 0s (0.0 MiB/s)."


def test_upload_progress_summary_reports_the_average_rate() -> None:
    progress = UploadProgress("peppy.tgz", 240 * 1024 * 1024, started_at=100.0)

    assert progress.summary_line(120.0) == "Uploaded peppy.tgz in 20s (12.0 MiB/s)."


# --- Upload: retry on timeout ---


@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_timeout_retries_and_succeeds(
    mock_delete: object,
    mock_sleep: object,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=[
            httpx.ReadTimeout("read timed out"),
            httpx.Response(201, json={"id": 99}),
        ]
    )

    result = github_upload_asset(
        github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=2
    )
    assert result["id"] == 99


# --- Upload: retry a connection that died mid-upload ---


@pytest.mark.parametrize(
    "transport_error",
    [
        httpx.ReadError("[Errno 54] Connection reset by peer"),
        httpx.WriteError("[Errno 32] Broken pipe"),
        httpx.ConnectError("connection refused"),
        httpx.RemoteProtocolError("server disconnected without sending a response"),
    ],
    ids=["read-reset", "write-broken-pipe", "connect-refused", "server-hung-up"],
)
@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_retries_a_dead_connection(
    mock_delete: object,
    mock_sleep: object,
    transport_error: httpx.RequestError,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=[transport_error, httpx.Response(201, json={"id": 99})]
    )

    result = github_upload_asset(
        github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=2
    )
    assert result["id"] == 99


# --- Upload: a request this client malformed is not retried ---


def test_upload_asset_does_not_retry_a_local_protocol_error(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    route = mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=httpx.LocalProtocolError("illegal header value")
    )

    with pytest.raises(ReleaseError, match="failed to upload asset"):
        github_upload_asset(
            github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=3
        )

    assert route.call_count == 1


# --- Upload: all retries exhausted ---


@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_retries_exhausted(
    mock_delete: object,
    mock_sleep: object,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=httpx.ReadTimeout("read timed out")
    )

    with pytest.raises(ReleaseError, match="failed to upload asset"):
        github_upload_asset(
            github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=3
        )


# --- Upload: non-retryable error fails immediately ---


def test_upload_asset_non_retryable_error_no_retry(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    route = mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )

    with pytest.raises(ReleaseError, match="HTTP 404"):
        github_upload_asset(
            github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=3
        )

    assert route.call_count == 1


# --- Upload: a server-side status retries then succeeds ---


@pytest.mark.parametrize("status", [500, 502, 503, 504])
@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_server_error_retries(
    mock_delete: object,
    mock_sleep: object,
    status: int,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=[
            httpx.Response(status, text="server error"),
            httpx.Response(201, json={"id": 99}),
        ]
    )

    result = github_upload_asset(
        github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=2
    )
    assert result["id"] == 99


# --- Upload: cleans up partial upload before retry ---


@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_cleans_partial_before_retry(
    mock_delete_asset: object,
    mock_sleep: object,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=[
            httpx.ReadTimeout("timed out"),
            httpx.Response(201, json={"id": 99}),
        ]
    )

    github_upload_asset(
        github_client, 1, "peppy-test.tgz", asset_file, SLUG, max_attempts=2
    )

    mock_delete_asset.assert_called_once_with(
        github_client, 1, "peppy-test.tgz", SLUG
    )


# --- delete_draft_release ---


def test_delete_draft_release_deletes_a_draft(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/42").mock(
        return_value=httpx.Response(200, json={"id": 42, "draft": True})
    )
    delete = mock_api.delete(f"{API_BASE}/releases/42").mock(
        return_value=httpx.Response(204)
    )

    assert delete_draft_release(github_client, 42, SLUG) is True
    assert delete.call_count == 1


def test_delete_draft_release_leaves_a_published_release(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/42").mock(
        return_value=httpx.Response(200, json={"id": 42, "draft": False})
    )
    delete = mock_api.delete(f"{API_BASE}/releases/42").mock(
        return_value=httpx.Response(204)
    )

    assert delete_draft_release(github_client, 42, SLUG) is False
    assert delete.call_count == 0


# --- find_draft_releases ---


def _release(release_id: int, tag: str, *, draft: bool) -> dict:
    return {
        "id": release_id,
        "tag_name": tag,
        "draft": draft,
        "html_url": f"https://github.com/test-owner/test-repo/releases/tag/untagged-{release_id}",
    }


def test_find_draft_releases_keeps_only_drafts_of_the_tag(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases", params={"per_page": "100", "page": "1"}).mock(
        return_value=httpx.Response(
            200,
            json=[
                _release(3, "v0.2.0", draft=True),
                _release(2, "v0.1.0", draft=True),
                _release(1, "v0.1.0", draft=False),
            ],
        )
    )

    drafts = find_draft_releases(github_client, SLUG, "v0.1.0")

    assert [d.release_id for d in drafts] == [2]
    assert drafts[0].html_url.endswith("untagged-2")


def test_find_draft_releases_reads_every_page(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    full_page = [_release(1000 - i, "v0.0.1", draft=False) for i in range(99)]
    full_page.append(_release(900, "v0.1.0", draft=True))
    mock_api.get(f"{API_BASE}/releases", params={"per_page": "100", "page": "1"}).mock(
        return_value=httpx.Response(200, json=full_page)
    )
    mock_api.get(f"{API_BASE}/releases", params={"per_page": "100", "page": "2"}).mock(
        return_value=httpx.Response(200, json=[_release(5, "v0.1.0", draft=True)])
    )

    drafts = find_draft_releases(github_client, SLUG, "v0.1.0")

    assert [d.release_id for d in drafts] == [900, 5]


def test_find_draft_releases_rejects_a_non_list_response(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases", params={"per_page": "100", "page": "1"}).mock(
        return_value=httpx.Response(200, json={"message": "Moved"})
    )

    with pytest.raises(ReleaseError, match="expected JSON array"):
        find_draft_releases(github_client, SLUG, "v0.1.0")


# --- find_published_release ---


def test_find_published_release_returns_the_release_of_the_tag(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/tags/v0.1.0").mock(
        return_value=httpx.Response(
            200,
            json=_release(4, "v0.1.0", draft=False) | {"html_url": "https://t/v0.1.0"},
        )
    )

    release = find_published_release(github_client, SLUG, "v0.1.0")

    assert release is not None
    assert (release.release_id, release.html_url) == (4, "https://t/v0.1.0")


def test_find_published_release_is_none_without_one(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/tags/v0.1.0").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )

    assert find_published_release(github_client, SLUG, "v0.1.0") is None


def test_find_published_release_never_takes_a_draft_for_one(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/tags/v0.1.0").mock(
        return_value=httpx.Response(200, json=_release(4, "v0.1.0", draft=True))
    )

    assert find_published_release(github_client, SLUG, "v0.1.0") is None


def test_find_published_release_rejects_a_non_object_response(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/tags/v0.1.0").mock(
        return_value=httpx.Response(200, json=[])
    )

    with pytest.raises(ReleaseError, match="expected JSON object"):
        find_published_release(github_client, SLUG, "v0.1.0")


# --- publish_release ---


def test_publish_release(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    fake_release_response: dict,
) -> None:
    fake_release_response["draft"] = False
    mock_api.patch(f"{API_BASE}/releases/12345").mock(
        return_value=httpx.Response(200, json=fake_release_response)
    )

    result = publish_release(github_client, 12345, SLUG)
    assert isinstance(result, dict)
    assert result["draft"] is False


# --- github_api none_on_404 ---


def test_github_api_returns_none_on_404_when_opted_in(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/latest").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )
    result = github_api(
        github_client,
        "GET",
        f"{API_BASE}/releases/latest",
        none_on_404=True,
    )
    assert result is None


def test_github_api_still_raises_on_404_by_default(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/missing").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )
    with pytest.raises(ReleaseError, match="Status: 404"):
        github_api(github_client, "GET", f"{API_BASE}/missing")


# --- get_latest_release ---


def test_get_latest_release_returns_dict(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    fake_release_response: dict,
) -> None:
    mock_api.get(f"{API_BASE}/releases/latest").mock(
        return_value=httpx.Response(200, json=fake_release_response)
    )
    result = get_latest_release(github_client, SLUG)
    assert result is not None
    assert result["tag_name"] == "v0.1.0"


def test_get_latest_release_returns_none_when_no_releases(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.get(f"{API_BASE}/releases/latest").mock(
        return_value=httpx.Response(404, json={"message": "Not Found"})
    )
    assert get_latest_release(github_client, SLUG) is None
