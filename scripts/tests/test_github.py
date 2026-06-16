"""Tests for functions.github module."""

from __future__ import annotations

import json
from pathlib import Path
from unittest.mock import patch

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import (
    RepoSlug,
    delete_release,
    generate_release_notes_preview,
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


# --- Upload: 502 retries then succeeds ---


@patch("functions.github.time.sleep")
@patch("functions.github.delete_asset_if_exists")
def test_upload_asset_502_retries(
    mock_delete: object,
    mock_sleep: object,
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
    asset_file: Path,
) -> None:
    mock_api.post(f"{UPLOAD_URL}?name=peppy-test.tgz").mock(
        side_effect=[
            httpx.Response(502, text="Bad Gateway"),
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


# --- delete_release ---


def test_delete_release(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.delete(f"{API_BASE}/releases/42").mock(
        return_value=httpx.Response(204)
    )

    delete_release(github_client, 42, SLUG)


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


# --- generate_release_notes_preview ---


def test_generate_release_notes_preview_returns_body_and_sends_previous_tag(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    route = mock_api.post(f"{API_BASE}/releases/generate-notes").mock(
        return_value=httpx.Response(
            200, json={"name": "v0.2.0", "body": "## What's Changed\n- x"}
        )
    )
    body = generate_release_notes_preview(
        github_client,
        SLUG,
        tag_name="v0.2.0",
        target_commitish="main",
        previous_tag_name="v0.1.0",
    )
    assert body == "## What's Changed\n- x"
    sent = json.loads(route.calls[0].request.content)
    assert sent == {
        "tag_name": "v0.2.0",
        "target_commitish": "main",
        "previous_tag_name": "v0.1.0",
    }


def test_generate_release_notes_preview_omits_previous_tag_when_none(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    route = mock_api.post(f"{API_BASE}/releases/generate-notes").mock(
        return_value=httpx.Response(200, json={"body": "notes"})
    )
    generate_release_notes_preview(
        github_client, SLUG, tag_name="v0.1.0", target_commitish="main"
    )
    sent = json.loads(route.calls[0].request.content)
    assert "previous_tag_name" not in sent


def test_generate_release_notes_preview_missing_body_raises(
    mock_api: respx.MockRouter,
    github_client: httpx.Client,
) -> None:
    mock_api.post(f"{API_BASE}/releases/generate-notes").mock(
        return_value=httpx.Response(200, json={"name": "v0.1.0"})
    )
    with pytest.raises(ReleaseError, match="missing 'body'"):
        generate_release_notes_preview(
            github_client, SLUG, tag_name="v0.1.0", target_commitish="main"
        )
