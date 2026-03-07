"""Integration tests for functions.build_release module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from functions.build import BuildArtifact
from functions.build_release import (
    _build_all_targets,
    _build_release_payload,
    _run_full,
    _run_local,
)
from functions.cli import ReleaseError


def test_build_release_payload_auto_generated_notes() -> None:
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


def test_build_release_payload_manual_notes() -> None:
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


def test_build_release_payload_empty_notes_body() -> None:
    payload = _build_release_payload(
        tag="v0.3.0",
        title="Release",
        target_commitish="develop",
        generate_notes=False,
        notes_body=None,
    )
    assert payload["body"] == ""


@patch("functions.build_release._build_all_targets")
@patch("functions.build_release.get_targets_for_platform", return_value=["x86_64-unknown-linux-gnu"])
@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.validate_release_environment", return_value="")
@patch("functions.build_release.prompt", return_value="v0.1.0")
def test_run_local_on_linux_builds_native_only(
    mock_prompt: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    mock_targets: MagicMock,
    mock_build_all: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    mock_build_all.return_value = [
        BuildArtifact(
            asset_name="peppy-x86_64-unknown-linux-gnu.tgz",
            asset_path=tmp_path / "dist" / "peppy-x86_64-unknown-linux-gnu.tgz",
            host_triple="x86_64-unknown-linux-gnu",
        )
    ]
    _run_local()
    mock_validate.assert_called_once_with(require_token=False)
    mock_build_all.assert_called_once_with(
        "v0.1.0", ["x86_64-unknown-linux-gnu"], tmp_path,
    )


@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.validate_release_environment", return_value="")
@patch("functions.build_release.prompt", return_value="")
def test_run_local_mode_empty_tag_raises(
    mock_prompt: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    with pytest.raises(ReleaseError, match="release tag cannot be empty"):
        _run_local()


@patch("functions.build_release.is_macos_arm64", return_value=False)
def test_run_full_rejects_non_macos(mock_platform: MagicMock) -> None:
    with pytest.raises(ReleaseError, match="full releases can only be created from macOS ARM64"):
        _run_full()


@patch("functions.build_release.ensure_rust_in_vm")
@patch("functions.build_release.ensure_lima_vm")
@patch("functions.build_release.find_limactl")
@patch("functions.build_release.is_macos_arm64", return_value=True)
@patch("functions.build_release.build_and_package")
def test_build_all_targets_on_macos_builds_four(
    mock_build: MagicMock,
    mock_platform: MagicMock,
    mock_find_lima: MagicMock,
    mock_ensure_vm: MagicMock,
    mock_ensure_rust: MagicMock,
    tmp_path: Path,
) -> None:
    limactl = tmp_path / "limactl"
    limactl.write_bytes(b"fake")
    mock_find_lima.return_value = limactl
    mock_build.return_value = BuildArtifact(
        asset_name="peppy-test.tgz",
        asset_path=tmp_path / "test.tgz",
        host_triple="test",
    )

    targets = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "riscv64gc-unknown-linux-gnu",
    ]
    artifacts = _build_all_targets("v0.1.0", targets, tmp_path)

    assert len(artifacts) == 4
    assert mock_build.call_count == 4

    # Native macOS build (no limactl kwarg)
    assert mock_build.call_args_list[0] == call(
        "v0.1.0", "aarch64-apple-darwin", tmp_path,
    )
    # Linux builds via Lima
    assert mock_build.call_args_list[1] == call(
        "v0.1.0", "x86_64-unknown-linux-gnu", tmp_path, limactl=limactl,
    )
    assert mock_build.call_args_list[2] == call(
        "v0.1.0", "aarch64-unknown-linux-gnu", tmp_path, limactl=limactl,
    )
    assert mock_build.call_args_list[3] == call(
        "v0.1.0", "riscv64gc-unknown-linux-gnu", tmp_path, limactl=limactl,
    )

    mock_ensure_vm.assert_called_once()
    mock_ensure_rust.assert_called_once()


@patch("functions.build_release.generate_release_notes_file")
@patch("functions.build_release.fetch_release_body_html", return_value="<p>notes</p>")
@patch("functions.build_release.replace_and_upload_asset")
@patch("functions.build_release.parse_release_response")
@patch("functions.build_release.github_api")
@patch("functions.build_release.build_github_client")
@patch("functions.build_release.github_repo_slug")
@patch("functions.build_release._build_all_targets")
@patch("functions.build_release.get_targets_for_platform")
@patch("functions.build_release.prompt_yn")
@patch("functions.build_release.prompt")
@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_current_branch", return_value="main")
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.validate_release_environment", return_value="test-token")
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_uploads_all_artifacts(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch: MagicMock,
    mock_uncommitted: MagicMock,
    mock_prompt: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_targets: MagicMock,
    mock_build_all: MagicMock,
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
    mock_prompt_yn.return_value = True
    mock_targets.return_value = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
        "riscv64gc-unknown-linux-gnu",
    ]
    mock_build_all.return_value = [
        BuildArtifact("peppy-a.tgz", tmp_path / "a.tgz", "aarch64-apple-darwin"),
        BuildArtifact("peppy-b.tgz", tmp_path / "b.tgz", "x86_64-unknown-linux-gnu"),
        BuildArtifact("peppy-c.tgz", tmp_path / "c.tgz", "aarch64-unknown-linux-gnu"),
        BuildArtifact("peppy-d.tgz", tmp_path / "d.tgz", "riscv64gc-unknown-linux-gnu"),
    ]
    mock_slug.return_value = RepoSlug(owner="test-owner", repo="test-repo")
    mock_client.return_value = MagicMock()
    mock_api.return_value = {
        "id": 1,
        "html_url": "https://github.com/test/releases/tag/v0.1.0",
    }
    mock_parse.return_value = ReleaseInfo(
        release_id=1,
        html_url="https://github.com/test/releases/tag/v0.1.0",
    )

    _run_full()

    mock_validate.assert_called_once()
    assert mock_upload.call_count == 4
    mock_gen_notes.assert_called_once()
