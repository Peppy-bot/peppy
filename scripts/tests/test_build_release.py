"""Integration tests for functions.build_release module."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from functions.build import BuildArtifact
from functions.build_release import (
    _build_all_targets,
    _build_release_payload,
    _confirm_release_content,
    _parse_editable,
    _prepare_release_content,
    _render_editable,
    _run_full,
    _run_local,
)
from functions.cli import ReleaseError
from functions.release_summary import ReleaseContent


def test_build_release_payload_includes_body_and_draft() -> None:
    payload = _build_release_payload(
        tag="v0.2.0",
        title="Topics API hardening",
        target_commitish="main",
        notes_body="## What's Changed\n- Fixed bug\n",
    )
    assert payload == {
        "tag_name": "v0.2.0",
        "name": "Topics API hardening",
        "target_commitish": "main",
        "draft": True,
        "body": "## What's Changed\n- Fixed bug\n",
    }


def test_build_release_payload_is_always_a_draft() -> None:
    # The release is created as a draft and only published after every asset
    # uploads, so the payload must never request a non-draft release.
    payload = _build_release_payload(
        tag="v0.3.0",
        title="Release",
        target_commitish="develop",
        notes_body="notes",
    )
    assert payload["draft"] is True


@patch("functions.build_release._build_all_targets")
@patch(
    "functions.build_release.get_targets_for_platform",
    return_value=["x86_64-unknown-linux-gnu"],
)
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
            target_triple="x86_64-unknown-linux-gnu",
        )
    ]
    _run_local()
    mock_validate.assert_called_once_with(require_token=False)
    mock_build_all.assert_called_once_with(
        "v0.1.0",
        ["x86_64-unknown-linux-gnu"],
        tmp_path,
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
    with pytest.raises(
        ReleaseError, match="full releases can only be created from macOS ARM64"
    ):
        _run_full()


@patch("functions.build_release.verify_all_releases")
@patch("functions.build_release.ensure_rust_in_vm")
@patch("functions.build_release.ensure_lima_vm")
@patch("functions.build_release.find_limactl")
@patch("functions.build_release.is_macos_arm64", return_value=True)
@patch("functions.build_release.build_and_package")
def test_build_all_targets_on_macos_builds_three(
    mock_build: MagicMock,
    mock_platform: MagicMock,
    mock_find_lima: MagicMock,
    mock_ensure_vm: MagicMock,
    mock_ensure_rust: MagicMock,
    mock_verify: MagicMock,
    tmp_path: Path,
) -> None:
    limactl = tmp_path / "limactl"
    limactl.write_bytes(b"fake")
    mock_find_lima.return_value = limactl
    mock_build.return_value = BuildArtifact(
        asset_name="peppy-test.tgz",
        asset_path=tmp_path / "test.tgz",
        target_triple="test",
    )

    targets = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ]
    artifacts = _build_all_targets("v0.1.0", targets, tmp_path)

    assert len(artifacts) == 3
    assert mock_build.call_count == 3

    # Native macOS build (no limactl kwarg)
    assert mock_build.call_args_list[0] == call(
        "v0.1.0",
        "aarch64-apple-darwin",
        tmp_path,
    )
    # Linux builds via Lima
    assert mock_build.call_args_list[1] == call(
        "v0.1.0",
        "x86_64-unknown-linux-gnu",
        tmp_path,
        limactl=limactl,
    )
    assert mock_build.call_args_list[2] == call(
        "v0.1.0",
        "aarch64-unknown-linux-gnu",
        tmp_path,
        limactl=limactl,
    )

    mock_ensure_vm.assert_called_once()
    mock_ensure_rust.assert_called_once()


def _setup_run_full_mocks(
    tmp_path: Path,
    *,
    mock_repo_root: MagicMock,
    mock_prompt: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_prepare: MagicMock,
    mock_targets: MagicMock,
    mock_build_all: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_api: MagicMock,
    mock_parse: MagicMock,
) -> None:
    """Common setup for _run_full integration tests."""
    from functions.github import ReleaseInfo, RepoSlug

    mock_repo_root.return_value = tmp_path
    # The tag is the only typed prompt; Claude-drafted content is mocked.
    mock_prompt.return_value = "v0.1.0"
    mock_prompt_yn.return_value = True
    mock_prepare.return_value = ReleaseContent(
        title="Topics API hardening",
        description="Hardened the topics API against deadlocks.",
        notes="## What's Changed\n- Fixed topics public API (#265)\n",
    )
    mock_targets.return_value = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ]
    mock_build_all.return_value = [
        BuildArtifact("peppy-a.tgz", tmp_path / "a.tgz", "aarch64-apple-darwin"),
        BuildArtifact("peppy-b.tgz", tmp_path / "b.tgz", "x86_64-unknown-linux-gnu"),
        BuildArtifact("peppy-c.tgz", tmp_path / "c.tgz", "aarch64-unknown-linux-gnu"),
    ]
    mock_slug.return_value = RepoSlug(owner="test-owner", repo="test-repo")
    mock_client.return_value = MagicMock()
    # First call: POST creates draft (GitHub assigns a temporary untagged URL).
    # Second call: GET fetches published release details (real tag URL).
    mock_api.side_effect = [
        {"id": 1, "html_url": "https://github.com/test/releases/tag/untagged-abc123"},
        {"id": 1, "html_url": "https://github.com/test/releases/tag/v0.1.0"},
    ]
    mock_parse.return_value = ReleaseInfo(
        release_id=1,
        html_url="https://github.com/test/releases/tag/untagged-abc123",
    )


@patch("functions.build_release._prepare_release_content")
@patch("functions.build_release.generate_release_notes_file")
@patch("functions.build_release.fetch_release_body_html", return_value="<p>notes</p>")
@patch("functions.build_release.publish_release")
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
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
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
    mock_publish: MagicMock,
    mock_fetch_html: MagicMock,
    mock_gen_notes: MagicMock,
    mock_prepare: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    _setup_run_full_mocks(
        tmp_path,
        mock_repo_root=mock_repo_root,
        mock_prompt=mock_prompt,
        mock_prompt_yn=mock_prompt_yn,
        mock_prepare=mock_prepare,
        mock_targets=mock_targets,
        mock_build_all=mock_build_all,
        mock_slug=mock_slug,
        mock_client=mock_client,
        mock_api=mock_api,
        mock_parse=mock_parse,
    )

    _run_full()

    mock_validate.assert_called_once()
    assert mock_upload.call_count == 3
    mock_publish.assert_called_once_with(mock_client.return_value, 1, mock_slug.return_value)
    mock_gen_notes.assert_called_once()
    # The draft is created with Claude's title and notes body.
    create_payload = mock_api.call_args_list[0].kwargs["json_data"]
    assert create_payload["name"] == "Topics API hardening"
    assert create_payload["body"] == mock_prepare.return_value.notes

    # Verify the release is created as a draft
    create_call = mock_api.call_args_list[0]
    payload = create_call.kwargs.get("json_data") or create_call[1].get("json_data")
    assert payload["draft"] is True

    # The displayed URL must be the published release URL, not the draft's untagged URL
    captured = capfd.readouterr()
    assert "releases/tag/v0.1.0" in captured.err
    assert "untagged" not in captured.err


@patch("functions.build_release._prepare_release_content")
@patch("functions.build_release.delete_release")
@patch("functions.build_release.publish_release")
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
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_cleans_up_draft_on_upload_failure(
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
    mock_publish: MagicMock,
    mock_delete_release: MagicMock,
    mock_prepare: MagicMock,
    tmp_path: Path,
) -> None:
    _setup_run_full_mocks(
        tmp_path,
        mock_repo_root=mock_repo_root,
        mock_prompt=mock_prompt,
        mock_prompt_yn=mock_prompt_yn,
        mock_prepare=mock_prepare,
        mock_targets=mock_targets,
        mock_build_all=mock_build_all,
        mock_slug=mock_slug,
        mock_client=mock_client,
        mock_api=mock_api,
        mock_parse=mock_parse,
    )
    mock_upload.side_effect = [None, ReleaseError("upload timeout"), None]

    with pytest.raises(ReleaseError, match="upload timeout"):
        _run_full()

    mock_delete_release.assert_called_once_with(
        mock_client.return_value, 1, mock_slug.return_value
    )
    mock_publish.assert_not_called()


@patch("functions.build_release._prepare_release_content")
@patch("functions.build_release.delete_release")
@patch("functions.build_release.publish_release")
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
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_warns_on_cleanup_failure(
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
    mock_publish: MagicMock,
    mock_delete_release: MagicMock,
    mock_prepare: MagicMock,
    tmp_path: Path,
) -> None:
    _setup_run_full_mocks(
        tmp_path,
        mock_repo_root=mock_repo_root,
        mock_prompt=mock_prompt,
        mock_prompt_yn=mock_prompt_yn,
        mock_prepare=mock_prepare,
        mock_targets=mock_targets,
        mock_build_all=mock_build_all,
        mock_slug=mock_slug,
        mock_client=mock_client,
        mock_api=mock_api,
        mock_parse=mock_parse,
    )
    mock_upload.side_effect = ReleaseError("upload timeout")
    mock_delete_release.side_effect = ReleaseError("cleanup failed")

    # Original error is re-raised, not the cleanup error
    with pytest.raises(ReleaseError, match="upload timeout"):
        _run_full()

    mock_delete_release.assert_called_once()
    mock_publish.assert_not_called()


# --- release content editing ---


def test_parse_editable_round_trips_rendered_content() -> None:
    content = ReleaseContent(
        title="Topics API hardening",
        description="Hardened the topics API against deadlocks.",
        notes="## What's Changed\n- Fixed topics public API (#265)",
    )
    assert _parse_editable(_render_editable(content)) == content


def test_parse_editable_strips_comment_lines() -> None:
    text = (
        "# this comment is ignored\n"
        "Title: T\n"
        "Description: D\n"
        "Notes:\n"
        "body line 1\n"
        "body line 2\n"
    )
    assert _parse_editable(text) == ReleaseContent(
        title="T", description="D", notes="body line 1\nbody line 2"
    )


def test_parse_editable_missing_label_raises() -> None:
    # No 'Description:' label.
    text = "Title: T\nNotes:\nbody\n"
    with pytest.raises(ReleaseError, match="must keep the 'Title:'"):
        _parse_editable(text)


def test_parse_editable_empty_field_raises() -> None:
    text = "Title: T\nDescription:\nNotes:\nbody\n"
    with pytest.raises(ReleaseError, match="empty title, description, or notes"):
        _parse_editable(text)


def test_confirm_release_content_accepts() -> None:
    content = ReleaseContent("T", "D", "N")
    with patch("functions.build_release.prompt_choice", return_value="y"):
        assert _confirm_release_content(content) is content


def test_confirm_release_content_aborts() -> None:
    content = ReleaseContent("T", "D", "N")
    with patch("functions.build_release.prompt_choice", return_value="a"):
        with pytest.raises(ReleaseError, match="aborted by user"):
            _confirm_release_content(content)


def test_confirm_release_content_edits_then_accepts() -> None:
    original = ReleaseContent("T", "D", "N")
    edited = ReleaseContent("T2", "D2", "N2")
    with patch(
        "functions.build_release.prompt_choice", side_effect=["e", "y"]
    ), patch(
        "functions.build_release._edit_release_content", return_value=edited
    ) as mock_edit:
        result = _confirm_release_content(original)
    assert result == edited
    mock_edit.assert_called_once_with(original)


# --- _prepare_release_content ---


@patch("functions.build_release._confirm_release_content", side_effect=lambda c: c)
@patch("functions.build_release.generate_release_content")
@patch("functions.build_release.generate_release_notes_preview")
@patch("functions.build_release.get_latest_release")
def test_prepare_release_content_uses_previous_release_tag(
    mock_latest: MagicMock,
    mock_preview: MagicMock,
    mock_generate: MagicMock,
    mock_confirm: MagicMock,
    tmp_path: Path,
) -> None:
    from functions.github import RepoSlug

    mock_latest.return_value = {"tag_name": "v0.11.1"}
    mock_preview.return_value = "## What's Changed\n- x"
    content = ReleaseContent("T", "D", "N")
    mock_generate.return_value = content
    client = MagicMock()
    slug = RepoSlug(owner="o", repo="r")

    result = _prepare_release_content(client, slug, "v0.12.0", "main", tmp_path)

    assert result == content
    mock_preview.assert_called_once_with(
        client,
        slug,
        tag_name="v0.12.0",
        target_commitish="main",
        previous_tag_name="v0.11.1",
    )
    mock_generate.assert_called_once_with("## What's Changed\n- x", "v0.12.0", tmp_path)
    mock_confirm.assert_called_once_with(content)


@patch("functions.build_release._confirm_release_content", side_effect=lambda c: c)
@patch("functions.build_release.generate_release_content")
@patch("functions.build_release.generate_release_notes_preview")
@patch("functions.build_release.get_latest_release", return_value=None)
def test_prepare_release_content_handles_no_previous_release(
    mock_latest: MagicMock,
    mock_preview: MagicMock,
    mock_generate: MagicMock,
    mock_confirm: MagicMock,
    tmp_path: Path,
) -> None:
    from functions.github import RepoSlug

    mock_preview.return_value = "## What's Changed\n- x"
    mock_generate.return_value = ReleaseContent("T", "D", "N")

    _prepare_release_content(MagicMock(), RepoSlug("o", "r"), "v0.1.0", "main", tmp_path)

    # With no prior release, previous_tag_name is None and GitHub picks the base.
    assert mock_preview.call_args.kwargs["previous_tag_name"] is None
