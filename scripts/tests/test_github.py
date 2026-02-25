"""Tests for functions.github module."""

from __future__ import annotations

import os
from pathlib import Path
from typing import Any
from unittest.mock import patch

import httpx
import pytest
import respx

from functions.cli import ReleaseError
from functions.github import (
    RepoSlug,
    build_github_client,
    delete_asset_if_exists,
    github_api,
    github_repo_slug,
    github_upload_asset,
    parse_release_response,
    replace_and_upload_asset,
)


def test_github_api_success(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/repos/owner/repo/releases/1").respond(
        json={"id": 1, "tag_name": "v0.1.0"}
    )
    result = github_api(
        github_client, "GET", "https://api.github.com/repos/owner/repo/releases/1"
    )
    assert result == {"id": 1, "tag_name": "v0.1.0"}


def test_github_api_http_error(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/repos/owner/repo/releases/tags/v999").respond(
        status_code=404, json={"message": "Not Found"}
    )
    with pytest.raises(ReleaseError, match="GitHub API request failed"):
        github_api(
            github_client,
            "GET",
            "https://api.github.com/repos/owner/repo/releases/tags/v999",
        )


def test_github_api_non_json_response(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/test").respond(
        status_code=200,
        text="<html>not json</html>",
        headers={"content-type": "text/html"},
    )
    with pytest.raises(ReleaseError, match="non-JSON response"):
        github_api(github_client, "GET", "https://api.github.com/test")


def test_github_api_empty_body_204(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.delete("https://api.github.com/test").respond(status_code=204, text="")
    result = github_api(github_client, "DELETE", "https://api.github.com/test")
    assert result == {}


def test_github_api_empty_body_non_204_is_error(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/test").respond(status_code=200, text="")
    with pytest.raises(ReleaseError, match="empty response"):
        github_api(github_client, "GET", "https://api.github.com/test")


def test_github_api_post_with_json_data(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    route = mock_api.post("https://api.github.com/repos/owner/repo/releases").respond(
        json={"id": 42, "html_url": "https://github.com/owner/repo/releases/tag/v1"}
    )
    result = github_api(
        github_client,
        "POST",
        "https://api.github.com/repos/owner/repo/releases",
        json_data={"tag_name": "v1.0.0", "name": "Release v1"},
    )
    assert result["id"] == 42
    assert route.called


def test_github_upload_asset_upload(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
    tmp_path: Path,
) -> None:
    asset_file = tmp_path / "test-asset.tgz"
    asset_file.write_bytes(b"fake binary content")

    mock_api.post(
        url__startswith="https://uploads.github.com/repos/owner/repo/releases/1/assets"
    ).respond(json={"id": 100, "name": "test-asset.tgz"})

    slug = RepoSlug(owner="owner", repo="repo")
    result = github_upload_asset(github_client, 1, "test-asset.tgz", asset_file, slug)
    assert result["id"] == 100


def test_github_upload_asset_upload_failure(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
    tmp_path: Path,
) -> None:
    asset_file = tmp_path / "test-asset.tgz"
    asset_file.write_bytes(b"fake")

    mock_api.post(url__startswith="https://uploads.github.com/").respond(
        status_code=422, text="Validation Failed"
    )

    slug = RepoSlug(owner="owner", repo="repo")
    with pytest.raises(ReleaseError, match="failed to upload asset"):
        github_upload_asset(github_client, 1, "test-asset.tgz", asset_file, slug)


def test_github_repo_slug_from_env() -> None:
    with patch.dict(os.environ, {"GITHUB_REPOSITORY": "myorg/myrepo"}):
        slug = github_repo_slug()
    assert slug.owner == "myorg"
    assert slug.repo == "myrepo"
    assert slug.full == "myorg/myrepo"


def test_github_repo_slug_from_ssh_remote(tmp_repo: Path) -> None:
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("GITHUB_REPOSITORY", None)
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            slug = github_repo_slug()
        finally:
            os.chdir(original_cwd)
    assert slug.owner == "test-owner"
    assert slug.repo == "test-repo"


def test_github_repo_slug_from_https_remote(tmp_repo: Path) -> None:
    import subprocess

    subprocess.run(
        [
            "git",
            "remote",
            "set-url",
            "origin",
            "https://github.com/https-owner/https-repo.git",
        ],
        cwd=tmp_repo,
        capture_output=True,
        check=True,
    )
    with patch.dict(os.environ, {}, clear=False):
        os.environ.pop("GITHUB_REPOSITORY", None)
        original_cwd = os.getcwd()
        try:
            os.chdir(tmp_repo)
            slug = github_repo_slug()
        finally:
            os.chdir(original_cwd)
    assert slug.owner == "https-owner"
    assert slug.repo == "https-repo"


def test_github_repo_slug_invalid_slug() -> None:
    with patch.dict(os.environ, {"GITHUB_REPOSITORY": "noslash"}):
        with pytest.raises(ReleaseError, match="invalid repo slug"):
            github_repo_slug()


def test_parse_release_response_valid(fake_release_response: dict[str, Any]) -> None:
    info = parse_release_response(fake_release_response)
    assert info.release_id == 12345
    assert (
        info.html_url == "https://github.com/test-owner/test-repo/releases/tag/v0.1.0"
    )


def test_parse_release_response_missing_id() -> None:
    with pytest.raises(ReleaseError, match="missing 'id'"):
        parse_release_response({"html_url": "something"})


def test_parse_release_response_api_error_message() -> None:
    with pytest.raises(ReleaseError, match="Not Found"):
        parse_release_response({"message": "Not Found"})


def test_parse_release_response_not_a_dict() -> None:
    with pytest.raises(ReleaseError, match="expected JSON object"):
        parse_release_response([1, 2, 3])  # type: ignore[arg-type]


def test_delete_asset_if_exists_found(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/repos/owner/repo/releases/1/assets").respond(
        json=[
            {"id": 100, "name": "other.tgz"},
            {"id": 200, "name": "peppy-test.tgz"},
        ]
    )
    delete_route = mock_api.delete(
        "https://api.github.com/repos/owner/repo/releases/assets/200"
    ).respond(status_code=204, text="")

    slug = RepoSlug(owner="owner", repo="repo")
    delete_asset_if_exists(github_client, 1, "peppy-test.tgz", slug)
    assert delete_route.called


def test_delete_asset_if_exists_not_found(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
) -> None:
    mock_api.get("https://api.github.com/repos/owner/repo/releases/1/assets").respond(
        json=[{"id": 100, "name": "other.tgz"}]
    )

    slug = RepoSlug(owner="owner", repo="repo")
    # Should not raise
    delete_asset_if_exists(github_client, 1, "peppy-test.tgz", slug)


def test_replace_and_upload_asset_replaces_existing(
    github_client: httpx.Client,
    mock_api: respx.MockRouter,
    tmp_path: Path,
) -> None:
    asset_file = tmp_path / "peppy-test.tgz"
    asset_file.write_bytes(b"binary content")

    # Existing asset found and deleted
    mock_api.get("https://api.github.com/repos/owner/repo/releases/1/assets").respond(
        json=[{"id": 300, "name": "peppy-test.tgz"}]
    )
    delete_route = mock_api.delete(
        "https://api.github.com/repos/owner/repo/releases/assets/300"
    ).respond(status_code=204, text="")
    upload_route = mock_api.post(
        url__startswith="https://uploads.github.com/repos/owner/repo/releases/1/assets"
    ).respond(json={"id": 400, "name": "peppy-test.tgz"})

    slug = RepoSlug(owner="owner", repo="repo")
    replace_and_upload_asset(github_client, 1, "peppy-test.tgz", asset_file, slug)
    assert delete_route.called
    assert upload_route.called


def test_build_github_client_creates_client_with_headers() -> None:
    client = build_github_client("my-token")
    assert client.headers["Authorization"] == "Bearer my-token"
    assert client.headers["X-GitHub-Api-Version"] == "2022-11-28"
    client.close()
