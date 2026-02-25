"""Integration tests for functions.add_release_build module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.add_release_build import _run
from functions.build import BuildArtifact
from functions.cli import ReleaseError
from functions.github import ReleaseInfo, RepoSlug


@patch("functions.add_release_build.replace_and_upload_asset")
@patch("functions.add_release_build.build_and_package")
@patch("functions.add_release_build.get_head_commit", return_value="abc123")
@patch("functions.add_release_build.get_tag_commit", return_value="abc123")
@patch("functions.add_release_build.parse_release_response")
@patch("functions.add_release_build.github_api")
@patch("functions.add_release_build.build_github_client")
@patch("functions.add_release_build.github_repo_slug")
@patch("functions.add_release_build.prompt", return_value="v0.1.0")
@patch("functions.add_release_build.has_uncommitted_changes", return_value=False)
@patch("functions.add_release_build.get_repo_root")
@patch("functions.add_release_build.validate_release_environment", return_value="test-token")
def test_run_add_release_build_happy_path(
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    mock_prompt: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_api: MagicMock,
    mock_parse: MagicMock,
    mock_tag_commit: MagicMock,
    mock_head_commit: MagicMock,
    mock_build: MagicMock,
    mock_upload: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_slug.return_value = RepoSlug(owner="test-owner", repo="test-repo")
    mock_client.return_value = MagicMock()
    mock_api.return_value = {"id": 1, "html_url": "https://example.com"}
    mock_parse.return_value = ReleaseInfo(release_id=1, html_url="https://example.com")
    mock_build.return_value = BuildArtifact(
        asset_name="peppy-test.tgz",
        asset_path=tmp_path / "test.tgz",
        host_triple="aarch64-apple-darwin",
    )

    _run()

    mock_validate.assert_called_once()
    mock_build.assert_called_once_with("v0.1.0", tmp_path)
    mock_upload.assert_called_once()


@patch("functions.add_release_build.replace_and_upload_asset")
@patch("functions.add_release_build.build_and_package")
@patch("functions.add_release_build.checkout")
@patch("functions.add_release_build.get_head_commit", return_value="different123")
@patch("functions.add_release_build.get_tag_commit", return_value="abc123")
@patch("functions.add_release_build.parse_release_response")
@patch("functions.add_release_build.github_api")
@patch("functions.add_release_build.build_github_client")
@patch("functions.add_release_build.github_repo_slug")
@patch("functions.add_release_build.prompt_yn", return_value=True)
@patch("functions.add_release_build.prompt", return_value="v0.1.0")
@patch("functions.add_release_build.has_uncommitted_changes", return_value=False)
@patch("functions.add_release_build.get_repo_root")
@patch("functions.add_release_build.validate_release_environment", return_value="test-token")
def test_run_add_release_build_tag_mismatch_checkout(
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    mock_prompt: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_api: MagicMock,
    mock_parse: MagicMock,
    mock_tag_commit: MagicMock,
    mock_head_commit: MagicMock,
    mock_checkout: MagicMock,
    mock_build: MagicMock,
    mock_upload: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_slug.return_value = RepoSlug(owner="owner", repo="repo")
    mock_client.return_value = MagicMock()
    mock_api.return_value = {"id": 1, "html_url": "https://example.com"}
    mock_parse.return_value = ReleaseInfo(release_id=1, html_url="https://example.com")
    mock_build.return_value = BuildArtifact(
        asset_name="peppy-test.tgz",
        asset_path=tmp_path / "test.tgz",
        host_triple="aarch64-apple-darwin",
    )

    _run()

    mock_checkout.assert_called_once_with("v0.1.0")
    mock_build.assert_called_once()


@patch("functions.add_release_build.has_uncommitted_changes", return_value=False)
@patch("functions.add_release_build.get_repo_root")
@patch("functions.add_release_build.validate_release_environment", return_value="token")
@patch("functions.add_release_build.prompt", return_value="")
def test_run_add_release_build_empty_tag_raises(
    mock_prompt: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    with pytest.raises(ReleaseError, match="release tag cannot be empty"):
        _run()


@patch("functions.add_release_build.get_head_commit", return_value="different123")
@patch("functions.add_release_build.get_tag_commit", return_value="abc123")
@patch("functions.add_release_build.parse_release_response")
@patch("functions.add_release_build.github_api")
@patch("functions.add_release_build.build_github_client")
@patch("functions.add_release_build.github_repo_slug")
@patch("functions.add_release_build.prompt_yn", return_value=False)
@patch("functions.add_release_build.prompt", return_value="v0.1.0")
@patch("functions.add_release_build.has_uncommitted_changes", return_value=False)
@patch("functions.add_release_build.get_repo_root")
@patch("functions.add_release_build.validate_release_environment", return_value="token")
def test_run_add_release_build_tag_mismatch_decline_exits(
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    mock_prompt: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_api: MagicMock,
    mock_parse: MagicMock,
    mock_tag_commit: MagicMock,
    mock_head_commit: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_slug.return_value = RepoSlug(owner="owner", repo="repo")
    mock_client.return_value = MagicMock()
    mock_api.return_value = {"id": 1, "html_url": "https://example.com"}
    mock_parse.return_value = ReleaseInfo(release_id=1, html_url="https://example.com")

    with pytest.raises(SystemExit) as exc_info:
        _run()
    assert exc_info.value.code == 1
