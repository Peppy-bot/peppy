"""Integration tests for functions.build_release module."""

from __future__ import annotations

import os
from pathlib import Path
from unittest.mock import MagicMock, patch

import httpx
import pytest
import respx

from functions.build import BuildArtifact
from functions.build_release import _build_release_payload, _run_full, _run_local
from functions.cli import ReleaseError


class TestBuildReleasePayload:
    def test_auto_generated_notes(self) -> None:
        payload = _build_release_payload(
            tag="v0.1.0",
            title="Release v0.1.0",
            target_commitish="main",
            generate_notes=True,
            notes_body=None,
        )
        assert payload == {
            "tag_name": "v0.1.0",
            "name": "Release v0.1.0",
            "target_commitish": "main",
            "generate_release_notes": True,
        }

    def test_manual_notes(self) -> None:
        payload = _build_release_payload(
            tag="v0.2.0",
            title="Release v0.2.0",
            target_commitish="main",
            generate_notes=False,
            notes_body="## Changes\n- Fixed bug\n",
        )
        assert payload == {
            "tag_name": "v0.2.0",
            "name": "Release v0.2.0",
            "target_commitish": "main",
            "body": "## Changes\n- Fixed bug\n",
        }

    def test_empty_notes_body(self) -> None:
        payload = _build_release_payload(
            tag="v0.3.0",
            title="Release",
            target_commitish="develop",
            generate_notes=False,
            notes_body=None,
        )
        assert payload["body"] == ""


class TestRunLocal:
    @patch("functions.build_release.build_and_package")
    @patch("functions.build_release.has_uncommitted_changes", return_value=False)
    @patch("functions.build_release.get_repo_root")
    @patch("functions.build_release.validate_release_environment", return_value="")
    @patch("functions.build_release.prompt", return_value="v0.1.0")
    def test_local_mode_builds_artifact(
        self,
        mock_prompt: MagicMock,
        mock_validate: MagicMock,
        mock_repo_root: MagicMock,
        mock_uncommitted: MagicMock,
        mock_build: MagicMock,
        tmp_path: Path,
    ) -> None:
        mock_repo_root.return_value = tmp_path
        mock_build.return_value = BuildArtifact(
            asset_name="peppy-aarch64-apple-darwin.tgz",
            asset_path=tmp_path / "dist" / "peppy-aarch64-apple-darwin.tgz",
            host_triple="aarch64-apple-darwin",
        )
        _run_local()
        mock_validate.assert_called_once_with(require_token=False)
        mock_build.assert_called_once_with("v0.1.0", tmp_path)

    @patch("functions.build_release.build_and_package")
    @patch("functions.build_release.has_uncommitted_changes", return_value=False)
    @patch("functions.build_release.get_repo_root")
    @patch("functions.build_release.validate_release_environment", return_value="")
    @patch("functions.build_release.prompt", return_value="v0.1.0")
    def test_local_mode_no_github_calls(
        self,
        mock_prompt: MagicMock,
        mock_validate: MagicMock,
        mock_repo_root: MagicMock,
        mock_uncommitted: MagicMock,
        mock_build: MagicMock,
        tmp_path: Path,
    ) -> None:
        mock_repo_root.return_value = tmp_path
        mock_build.return_value = BuildArtifact(
            asset_name="peppy-test.tgz",
            asset_path=tmp_path / "test.tgz",
            host_triple="aarch64-apple-darwin",
        )

        with patch("functions.build_release.build_github_client") as mock_client:
            _run_local()
            mock_client.assert_not_called()

    @patch("functions.build_release.has_uncommitted_changes", return_value=False)
    @patch("functions.build_release.get_repo_root")
    @patch("functions.build_release.validate_release_environment", return_value="")
    @patch("functions.build_release.prompt", return_value="")
    def test_local_mode_empty_tag_raises(
        self,
        mock_prompt: MagicMock,
        mock_validate: MagicMock,
        mock_repo_root: MagicMock,
        mock_uncommitted: MagicMock,
        tmp_path: Path,
    ) -> None:
        mock_repo_root.return_value = tmp_path
        with pytest.raises(ReleaseError, match="release tag cannot be empty"):
            _run_local()


class TestRunFull:
    @patch("functions.build_release.generate_release_notes_file")
    @patch("functions.build_release.fetch_release_body_html", return_value="<p>notes</p>")
    @patch("functions.build_release.replace_and_upload_asset")
    @patch("functions.build_release.parse_release_response")
    @patch("functions.build_release.github_api")
    @patch("functions.build_release.build_github_client")
    @patch("functions.build_release.github_repo_slug")
    @patch("functions.build_release.build_and_package")
    @patch("functions.build_release.prompt_yn")
    @patch("functions.build_release.prompt")
    @patch("functions.build_release.has_uncommitted_changes", return_value=False)
    @patch("functions.build_release.get_current_branch", return_value="main")
    @patch("functions.build_release.get_repo_root")
    @patch("functions.build_release.validate_release_environment", return_value="test-token")
    def test_full_release_flow(
        self,
        mock_validate: MagicMock,
        mock_repo_root: MagicMock,
        mock_branch: MagicMock,
        mock_uncommitted: MagicMock,
        mock_prompt: MagicMock,
        mock_prompt_yn: MagicMock,
        mock_build: MagicMock,
        mock_slug: MagicMock,
        mock_client: MagicMock,
        mock_api: MagicMock,
        mock_parse: MagicMock,
        mock_upload: MagicMock,
        mock_fetch_html: MagicMock,
        mock_gen_notes: MagicMock,
        tmp_path: Path,
    ) -> None:
        from functions.github import ReleaseInfo, RepoSlug

        mock_repo_root.return_value = tmp_path
        mock_prompt.side_effect = ["v0.1.0", "Release v0.1.0", "First release"]
        mock_prompt_yn.return_value = True  # auto-generate notes
        mock_build.return_value = BuildArtifact(
            asset_name="peppy-test.tgz",
            asset_path=tmp_path / "test.tgz",
            host_triple="aarch64-apple-darwin",
        )
        mock_slug.return_value = RepoSlug(owner="test-owner", repo="test-repo")
        mock_client.return_value = MagicMock()
        mock_api.return_value = {"id": 1, "html_url": "https://github.com/test/releases/tag/v0.1.0"}
        mock_parse.return_value = ReleaseInfo(
            release_id=1,
            html_url="https://github.com/test/releases/tag/v0.1.0",
        )

        _run_full()

        mock_validate.assert_called_once()
        mock_build.assert_called_once_with("v0.1.0", tmp_path)
        mock_upload.assert_called_once()
        mock_gen_notes.assert_called_once()
