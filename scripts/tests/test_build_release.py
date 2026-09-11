"""Integration tests for functions.build_release module."""

from __future__ import annotations

import shutil
from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from functions.build import BuildArtifact
from functions.build_release import (
    _build_all_targets,
    _build_release_payload,
    _commit_notes_and_align_main,
    _confirm_release_content,
    _find_open_docs_sync_pr,
    _offer_pending_upload,
    _open_docs_pr,
    _open_editor,
    _parse_editable,
    _prepare_release_content,
    _push_docs_sync_branch,
    _render_editable,
    _run_full,
    _run_local,
    _verify_docs_up_to_date,
    _verify_release_branch_state,
)
from functions.cli import ReleaseError
from functions.docs import (
    CheckResult,
    RequiredChange,
    UpdateOutcome,
    UpdateResult,
)
from functions.github import RepoSlug
from functions.pending_upload import load_pending_upload, record_pending_upload
from functions.release_summary import ReleaseChanges, ReleaseContent

DEV_COMMIT = "1111111111111111111111111111111111111111"
MAIN_COMMIT = "2222222222222222222222222222222222222222"

RELEASE_CONTENT = ReleaseContent(
    title="Topics API hardening",
    description="Hardened the topics API against deadlocks.",
    notes="## What's Changed\n- Fixed topics public API (#265)\n",
)


@pytest.fixture(autouse=True)
def _dist_dir_under_repo_root(monkeypatch: pytest.MonkeyPatch) -> None:
    """Keep the dist directory under the fake repo root whatever the host's env says."""
    monkeypatch.delenv("PEPPY_DIST_DIR", raising=False)


def _unwrapped(console_output: str) -> str:
    """Console output with rich's line wrapping undone, for phrase assertions."""
    return " ".join(console_output.split())


def _write_release_archives(dist_dir: Path) -> list[BuildArtifact]:
    """Write one small fake archive per release target and describe them."""
    dist_dir.mkdir(parents=True, exist_ok=True)
    artifacts: list[BuildArtifact] = []
    for name, triple in (
        ("a", "aarch64-apple-darwin"),
        ("b", "x86_64-unknown-linux-gnu"),
        ("c", "aarch64-unknown-linux-gnu"),
    ):
        asset_path = dist_dir / f"peppy-{name}.tgz"
        asset_path.write_bytes(f"archive {name}".encode())
        artifacts.append(BuildArtifact(f"peppy-{name}.tgz", asset_path, triple))
    return artifacts


def test_build_release_payload_includes_body_and_draft() -> None:
    payload = _build_release_payload(
        tag="v0.2.0",
        title="Topics API hardening",
        target_commitish=DEV_COMMIT,
        notes_body="## What's Changed\n- Fixed bug\n",
    )
    assert payload == {
        "tag_name": "v0.2.0",
        "name": "Topics API hardening",
        "target_commitish": DEV_COMMIT,
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


@patch("functions.build_release.build_github_client")
@patch("functions.build_release.github_repo_slug")
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=False)
def test_run_full_rejects_building_off_macos(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path

    with pytest.raises(
        ReleaseError, match="full releases can only be created from macOS ARM64"
    ):
        _run_full()

    # Only git is needed to get as far as the pending-upload offer; the build
    # tools are checked with the build host, once a build is actually due.
    assert mock_validate.call_args.kwargs["required_commands"] == ("git",)


@patch("functions.build_release.need_cmd")
@patch("functions.build_release.build_github_client")
@patch("functions.build_release.github_repo_slug")
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_requires_the_build_tools_before_building(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_need_cmd: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path

    def missing_cargo(cmd: str) -> str:
        if cmd == "cargo":
            raise ReleaseError("missing required command: cargo")
        return f"/usr/bin/{cmd}"

    mock_need_cmd.side_effect = missing_cargo

    with pytest.raises(ReleaseError, match="missing required command: cargo"):
        _run_full()


@patch("functions.build_release.stop_lima_vm")
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
    mock_stop: MagicMock,
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
    # The Lima VM must be stopped once the build completes so it frees host RAM.
    mock_stop.assert_called_once_with(limactl)


@patch("functions.build_release.stop_lima_vm")
@patch("functions.build_release.verify_all_releases")
@patch("functions.build_release.ensure_rust_in_vm")
@patch("functions.build_release.ensure_lima_vm")
@patch("functions.build_release.find_limactl")
@patch("functions.build_release.is_macos_arm64", return_value=True)
@patch("functions.build_release.build_and_package")
def test_build_all_targets_stops_vm_when_linux_build_fails(
    mock_build: MagicMock,
    mock_platform: MagicMock,
    mock_find_lima: MagicMock,
    mock_ensure_vm: MagicMock,
    mock_ensure_rust: MagicMock,
    mock_verify: MagicMock,
    mock_stop: MagicMock,
    tmp_path: Path,
) -> None:
    limactl = tmp_path / "limactl"
    mock_find_lima.return_value = limactl

    def build_side_effect(
        tag: str, triple: str, repo_root: Path, *, limactl: Path | None = None
    ) -> BuildArtifact:
        if "linux" in triple:
            raise ReleaseError("linux build blew up")
        return BuildArtifact(
            asset_name="peppy-test.tgz",
            asset_path=tmp_path / "test.tgz",
            target_triple=triple,
        )

    mock_build.side_effect = build_side_effect

    with pytest.raises(ReleaseError, match="linux build blew up"):
        _build_all_targets(
            "v0.1.0",
            ["aarch64-apple-darwin", "x86_64-unknown-linux-gnu"],
            tmp_path,
        )

    # The VM was started for the linux target, so it must be stopped even though
    # the build raised: cleanup runs in `finally`.
    mock_stop.assert_called_once_with(limactl)


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
    mock_prepare.return_value = RELEASE_CONTENT
    mock_targets.return_value = [
        "aarch64-apple-darwin",
        "x86_64-unknown-linux-gnu",
        "aarch64-unknown-linux-gnu",
    ]
    # The archives exist on disk: the pending-upload manifest hashes them.
    mock_build_all.return_value = _write_release_archives(tmp_path / "dist")
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


@patch("functions.build_release.need_cmd")
@patch("functions.build_release._verify_docs_up_to_date")
@patch("functions.build_release._commit_notes_and_align_main")
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
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_uploads_all_artifacts(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
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
    mock_align: MagicMock,
    mock_docs_gate: MagicMock,
    mock_need_cmd: MagicMock,
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
    notes_path = tmp_path / "docs" / "v0.1.0.html"
    mock_gen_notes.return_value = notes_path

    _run_full()

    mock_validate.assert_called_once()
    # The docs gate is handed the dev commit being released and runs before
    # anything is typed, drafted, or built.
    mock_docs_gate.assert_called_once_with(
        mock_client.return_value, mock_slug.return_value, DEV_COMMIT, tmp_path
    )
    assert mock_upload.call_count == 3
    mock_publish.assert_called_once_with(mock_client.return_value, 1, mock_slug.return_value)
    mock_gen_notes.assert_called_once()
    # The draft is created with Claude's title and notes body.
    create_payload = mock_api.call_args_list[0].kwargs["json_data"]
    assert create_payload["name"] == "Topics API hardening"
    assert create_payload["body"] == mock_prepare.return_value.notes

    # Verify the release is created as a draft, tagged at the exact dev commit
    # the archives were built from rather than at a branch name.
    create_call = mock_api.call_args_list[0]
    payload = create_call.kwargs.get("json_data") or create_call[1].get("json_data")
    assert payload["draft"] is True
    assert payload["target_commitish"] == DEV_COMMIT

    # The generated notes file is committed on dev and main aligned to it, only
    # after the release is published.
    mock_align.assert_called_once_with(notes_path, "v0.1.0")

    # The release is live, so there is nothing left to resume.
    assert not (tmp_path / "dist" / "pending-upload.json").exists()

    # The displayed URL must be the published release URL, not the draft's untagged URL
    captured = capfd.readouterr()
    assert "releases/tag/v0.1.0" in captured.err
    assert "untagged" not in captured.err


@patch("functions.build_release.need_cmd")
@patch("functions.build_release._verify_docs_up_to_date")
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
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_cleans_up_draft_on_upload_failure(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
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
    mock_docs_gate: MagicMock,
    mock_need_cmd: MagicMock,
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
    mock_upload.side_effect = [None, ReleaseError("upload timeout"), None]

    with pytest.raises(ReleaseError, match="upload timeout"):
        _run_full()

    mock_delete_release.assert_called_once_with(
        mock_client.return_value, 1, mock_slug.return_value
    )
    mock_publish.assert_not_called()

    # The archives survived the failure: the manifest describing them is kept
    # for the next run to resume from, and the user is told so.
    pending = load_pending_upload(tmp_path / "dist" / "pending-upload.json")
    assert pending is not None
    assert pending.tag == "v0.1.0"
    assert pending.release_commit == DEV_COMMIT
    assert pending.content == RELEASE_CONTENT
    assert pending.artifacts == mock_build_all.return_value
    output = _unwrapped(capfd.readouterr().err)
    assert "offers to upload them again" in output
    assert f"checkout of 'dev' at {DEV_COMMIT[:12]} on another machine" in output


@patch("functions.build_release.need_cmd")
@patch("functions.build_release._verify_docs_up_to_date")
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
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_warns_on_cleanup_failure(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
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
    mock_docs_gate: MagicMock,
    mock_need_cmd: MagicMock,
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


@patch("functions.build_release.need_cmd")
@patch("functions.build_release._verify_docs_up_to_date")
@patch(
    "functions.build_release._commit_notes_and_align_main",
    side_effect=ReleaseError("failed to push 'dev' to 'origin/main': rejected"),
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
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_reports_manual_steps_when_git_align_fails(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
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
    mock_align: MagicMock,
    mock_docs_gate: MagicMock,
    mock_need_cmd: MagicMock,
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
    mock_gen_notes.return_value = tmp_path / "docs" / "v0.1.0.html"

    with pytest.raises(ReleaseError) as excinfo:
        _run_full()

    # The release is already live, so the failure has to hand over the exact
    # commands that finish the git side by hand.
    message = str(excinfo.value)
    assert "failed to push 'dev' to 'origin/main'" in message
    assert "The GitHub release v0.1.0 is published" in message
    assert "git push origin dev" in message
    assert "git push origin dev:main" in message
    # The published release is never rolled back over a git failure.
    mock_publish.assert_called_once()


@patch("functions.build_release._verify_docs_up_to_date")
@patch("functions.build_release._commit_notes_and_align_main")
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
@patch("functions.build_release.has_uncommitted_changes", return_value=True)
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.need_cmd")
@patch("functions.build_release.is_macos_arm64", return_value=False)
def test_run_full_publishes_archives_copied_from_another_machine(
    mock_platform: MagicMock,
    mock_need_cmd: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
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
    mock_align: MagicMock,
    mock_docs_gate: MagicMock,
    tmp_path: Path,
    monkeypatch: pytest.MonkeyPatch,
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
    # On the macOS build machine, a run built the archives for v0.2.0 and
    # failed to publish them; the content it recorded differs from what Claude
    # would draft now.
    mac_dist = tmp_path / "mac" / "dist"
    archives = _write_release_archives(mac_dist)
    recorded_content = ReleaseContent(
        title="Recorded title", description="Recorded description", notes="- recorded"
    )
    record_pending_upload(
        mac_dist / "pending-upload.json", "v0.2.0", DEV_COMMIT, recorded_content, archives
    )

    # The dist directory is copied as-is into a staging folder on a Linux host
    # that has its own checkout of `dev` at the release commit and no build
    # tools; PEPPY_DIST_DIR points the release at that folder.
    linux_repo = tmp_path / "linux" / "repo"
    staging = tmp_path / "linux" / "staging"
    shutil.copytree(mac_dist, staging)
    linux_repo.mkdir(parents=True)
    mock_repo_root.return_value = linux_repo
    monkeypatch.setenv("PEPPY_DIST_DIR", str(staging))
    notes_path = linux_repo / "docs" / "v0.2.0.html"
    mock_gen_notes.return_value = notes_path

    _run_full()

    # The archives were built elsewhere: this host is not macOS ARM64 and is
    # never asked for cargo, rustc, or claude, only for git.
    assert mock_validate.call_args.kwargs["required_commands"] == ("git",)
    mock_need_cmd.assert_not_called()

    # The only question asked is whether to resume: no tag is typed, nothing
    # is drafted, the docs gate and the uncommitted-changes prompt are skipped
    # (the archives are already built), and nothing is rebuilt.
    mock_prompt_yn.assert_called_once()
    assert "instead of rebuilding" in mock_prompt_yn.call_args.args[0]
    mock_prompt.assert_not_called()
    mock_prepare.assert_not_called()
    mock_docs_gate.assert_not_called()
    mock_build_all.assert_not_called()

    # The draft carries the recorded content, pinned to the recorded commit.
    create_payload = mock_api.call_args_list[0].kwargs["json_data"]
    assert create_payload["tag_name"] == "v0.2.0"
    assert create_payload["name"] == "Recorded title"
    assert create_payload["body"] == "- recorded"
    assert create_payload["target_commitish"] == DEV_COMMIT

    # The copies in the staging folder are what gets uploaded, then the
    # release goes live and the notes are committed as for a fresh release.
    uploaded = [
        (call.args[2], call.args[3]) for call in mock_upload.call_args_list
    ]
    assert uploaded == [(a.asset_name, staging / a.asset_name) for a in archives]
    mock_publish.assert_called_once_with(mock_client.return_value, 1, mock_slug.return_value)
    assert mock_gen_notes.call_args.args[0].description == "Recorded description"
    mock_align.assert_called_once_with(notes_path, "v0.2.0")
    # Publishing consumes the staging folder's manifest and nothing else: the
    # archives stay, and the build machine's copy is another machine's business.
    assert not (staging / "pending-upload.json").exists()
    assert all((staging / a.asset_name).is_file() for a in archives)
    assert (mac_dist / "pending-upload.json").is_file()


# --- offering a pending upload ---


def _record_pending(tmp_path: Path, release_commit: str = DEV_COMMIT) -> Path:
    """Write a manifest for freshly built fake archives and return its path."""
    manifest_path = tmp_path / "dist" / "pending-upload.json"
    archives = _write_release_archives(tmp_path / "dist")
    record_pending_upload(manifest_path, "v0.1.0", release_commit, RELEASE_CONTENT, archives)
    return manifest_path


@patch("functions.build_release.prompt_yn")
def test_offer_pending_upload_is_silent_without_a_manifest(
    mock_prompt_yn: MagicMock, tmp_path: Path
) -> None:
    assert _offer_pending_upload(tmp_path / "dist" / "pending-upload.json", DEV_COMMIT) is None
    mock_prompt_yn.assert_not_called()


@patch("functions.build_release.prompt_yn", return_value=True)
def test_offer_pending_upload_returns_the_manifest_when_accepted(
    mock_prompt_yn: MagicMock, tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    manifest_path = _record_pending(tmp_path)

    pending = _offer_pending_upload(manifest_path, DEV_COMMIT)

    assert pending is not None
    assert pending.tag == "v0.1.0"
    assert pending.content == RELEASE_CONTENT
    # The offer names what would be published: the tag, its title, the commit,
    # and every archive.
    output = _unwrapped(capfd.readouterr().err)
    assert "v0.1.0" in output
    assert RELEASE_CONTENT.title in output
    assert DEV_COMMIT[:12] in output
    for artifact in pending.artifacts:
        assert artifact.asset_name in output
    # Accepting is the default answer: resuming is why the manifest exists.
    assert mock_prompt_yn.call_args.kwargs["default_yes"] is True
    # Publishing removes the manifest; the offer itself leaves it in place.
    assert manifest_path.exists()


@patch("functions.build_release.prompt_yn", return_value=True)
def test_offer_pending_upload_prints_every_archive_path_whole(
    mock_prompt_yn: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    # A console narrower than any archive path. Rich folds an over-long word
    # across lines, and a path split mid-word can neither be read back by this
    # suite nor copied by the user: each one has to come out on a line of its own.
    monkeypatch.setenv("COLUMNS", "40")
    manifest_path = _record_pending(tmp_path)

    pending = _offer_pending_upload(manifest_path, DEV_COMMIT)

    assert pending is not None
    printed = [line.strip() for line in capfd.readouterr().err.splitlines()]
    for artifact in pending.artifacts:
        assert len(str(artifact.asset_path)) > 40
        assert str(artifact.asset_path) in printed


@patch("functions.build_release.prompt_yn", return_value=False)
def test_offer_pending_upload_keeps_the_manifest_when_refused(
    mock_prompt_yn: MagicMock, tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    manifest_path = _record_pending(tmp_path)

    assert _offer_pending_upload(manifest_path, DEV_COMMIT) is None

    # Refusing means "not this time": the archives and the manifest stay until
    # a rebuild writes over them, so a stray "no" costs nothing.
    assert manifest_path.exists()
    assert "Starting a fresh release" in _unwrapped(capfd.readouterr().err)


@patch("functions.build_release.prompt_yn")
def test_offer_pending_upload_discards_archives_built_from_another_commit(
    mock_prompt_yn: MagicMock, tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    manifest_path = _record_pending(tmp_path, release_commit=MAIN_COMMIT)

    assert _offer_pending_upload(manifest_path, DEV_COMMIT) is None

    # `dev` moved on since the build: the tag would pin an older commit than
    # the one `main` is fast-forwarded to, so the archives are not offered.
    mock_prompt_yn.assert_not_called()
    assert not manifest_path.exists()
    output = _unwrapped(capfd.readouterr().err)
    assert f"built from {MAIN_COMMIT[:12]}" in output
    assert f"'dev' is now at {DEV_COMMIT[:12]}" in output


@patch("functions.build_release.prompt_yn")
def test_offer_pending_upload_discards_archives_that_changed_since_the_build(
    mock_prompt_yn: MagicMock, tmp_path: Path, capfd: pytest.CaptureFixture[str]
) -> None:
    manifest_path = _record_pending(tmp_path)
    # A `--local` build in between wrote a different archive over one of them
    # and another one is gone.
    (tmp_path / "dist" / "peppy-b.tgz").write_bytes(b"rebuilt locally")
    (tmp_path / "dist" / "peppy-c.tgz").unlink()

    assert _offer_pending_upload(manifest_path, DEV_COMMIT) is None

    mock_prompt_yn.assert_not_called()
    assert not manifest_path.exists()
    output = _unwrapped(capfd.readouterr().err)
    assert "peppy-b.tgz changed since it was built" in output
    assert "peppy-c.tgz is missing" in output


def test_offer_pending_upload_rejects_a_manifest_it_cannot_parse(
    tmp_path: Path,
) -> None:
    manifest_path = tmp_path / "dist" / "pending-upload.json"
    manifest_path.parent.mkdir()
    manifest_path.write_text("{not json")

    with pytest.raises(ReleaseError, match="not a valid pending-upload manifest"):
        _offer_pending_upload(manifest_path, DEV_COMMIT)


# --- release branch state ---


def _commit_resolver(commits: dict[str, str]):
    """Return a get_commit stub resolving the given revisions."""
    return lambda rev: commits[rev]


def _ancestry_resolver(ancestry: dict[tuple[str, str], bool]):
    """Return an is_ancestor stub answering the given (ancestor, descendant) pairs."""
    return lambda ancestor, descendant: ancestry[(ancestor, descendant)]


@patch("functions.build_release.get_current_branch", return_value="main")
def test_verify_release_branch_state_rejects_other_branch(
    mock_branch: MagicMock,
) -> None:
    with pytest.raises(ReleaseError, match="releases are cut from 'dev' only"):
        _verify_release_branch_state()


@patch("functions.build_release.get_current_branch", return_value=None)
def test_verify_release_branch_state_rejects_detached_head(
    mock_branch: MagicMock,
) -> None:
    with pytest.raises(ReleaseError, match="HEAD is on a detached commit"):
        _verify_release_branch_state()


@patch("functions.build_release.is_ancestor", return_value=True)
@patch("functions.build_release.get_commit")
@patch("functions.build_release.fetch_remote_branches")
@patch("functions.build_release.get_current_branch", return_value="dev")
def test_verify_release_branch_state_returns_the_dev_commit(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": DEV_COMMIT, "origin/main": MAIN_COMMIT}
    )

    assert _verify_release_branch_state() == DEV_COMMIT

    # Both remote-tracking refs are refreshed before they are compared.
    mock_fetch.assert_called_once_with("origin", ("dev", "main"))
    mock_is_ancestor.assert_called_once_with(MAIN_COMMIT, DEV_COMMIT)


@patch("functions.build_release.is_ancestor")
@patch("functions.build_release.get_commit")
@patch("functions.build_release.fetch_remote_branches")
@patch("functions.build_release.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_dev_behind_remote(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver({(DEV_COMMIT, remote_dev): True})

    with pytest.raises(ReleaseError, match="'dev' is behind origin/dev"):
        _verify_release_branch_state()


@patch("functions.build_release.is_ancestor")
@patch("functions.build_release.get_commit")
@patch("functions.build_release.fetch_remote_branches")
@patch("functions.build_release.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_unpushed_dev_commits(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(DEV_COMMIT, remote_dev): False, (remote_dev, DEV_COMMIT): True}
    )

    with pytest.raises(ReleaseError, match="commits that are not on origin/dev"):
        _verify_release_branch_state()


@patch("functions.build_release.is_ancestor")
@patch("functions.build_release.get_commit")
@patch("functions.build_release.fetch_remote_branches")
@patch("functions.build_release.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_diverged_dev(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    remote_dev = "3333333333333333333333333333333333333333"
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": remote_dev, "origin/main": MAIN_COMMIT}
    )
    mock_is_ancestor.side_effect = _ancestry_resolver(
        {(DEV_COMMIT, remote_dev): False, (remote_dev, DEV_COMMIT): False}
    )

    with pytest.raises(ReleaseError, match="'dev' and origin/dev have diverged"):
        _verify_release_branch_state()


@patch("functions.build_release.is_ancestor", return_value=False)
@patch("functions.build_release.get_commit")
@patch("functions.build_release.fetch_remote_branches")
@patch("functions.build_release.get_current_branch", return_value="dev")
def test_verify_release_branch_state_rejects_main_ahead_of_dev(
    mock_branch: MagicMock,
    mock_fetch: MagicMock,
    mock_get_commit: MagicMock,
    mock_is_ancestor: MagicMock,
) -> None:
    # main carries a commit dev does not have, so it cannot fast-forward.
    mock_get_commit.side_effect = _commit_resolver(
        {"HEAD": DEV_COMMIT, "origin/dev": DEV_COMMIT, "origin/main": MAIN_COMMIT}
    )

    with pytest.raises(ReleaseError, match="'main' cannot fast-forward to it"):
        _verify_release_branch_state()


# --- the docs freshness gate ---


_BLOCKING_CHANGE = RequiredChange(
    file="docs/x.mdx", change="document --verbose", severity="blocking"
)
_MINOR_CHANGE = RequiredChange(
    file="docs/y.mdx", change="reword the intro", severity="minor"
)


def _docs_gate(
    client: MagicMock | None = None,
    slug: MagicMock | None = None,
    repo_root: Path | None = None,
) -> None:
    """Invoke the gate with placeholder GitHub handles."""
    _verify_docs_up_to_date(
        client or MagicMock(),
        slug or RepoSlug(owner="test-owner", repo="test-repo"),
        DEV_COMMIT,
        repo_root or Path("/repo"),
    )


@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release.check_docs",
    return_value=CheckResult(changes=()),
)
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_passes_and_diffs_against_origin_main(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
) -> None:
    _docs_gate(repo_root=Path("/repo"))

    # origin/main is the last shipped commit, so it is the base the docs have
    # to cover; the head is the dev commit being released.
    mock_check.assert_called_once_with("origin/main", DEV_COMMIT)
    mock_update.assert_not_called()


@patch("functions.build_release.prompt_yn", return_value=False)
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_passes_on_minor_only_suggestions_when_declined(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_prompt_yn: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Wording-level suggestions never stop a release: they are printed, the
    # user is offered an optional pull request, and declining it leaves no
    # trace — no update run, no branch, and the release moves on.
    _docs_gate(repo_root=Path("/repo"))

    mock_prompt_yn.assert_called_once()
    assert (
        mock_find_pr.call_args.args[2] == f"auto/docs-polish-{DEV_COMMIT[:12]}"
    )
    mock_update.assert_not_called()
    err = capfd.readouterr().err
    assert "reword the intro" in err
    assert "up to date" in err


@patch("functions.build_release._open_docs_pr")
@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release.prompt_yn", return_value=True)
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.build_release.has_changes_in_paths")
def test_docs_gate_opens_an_optional_polish_pr_and_continues(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Accepting the offer opens the pull request on the side; the gate still
    # returns normally so the release flows on to the tag prompt.
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/y.mdx",
                change="reword the intro",
                status="implemented",
            ),
        ),
        summary="polished 1 file",
    )
    # Clean at the gate's dirty check, dirty after the polish update.
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/8"

    _docs_gate(repo_root=Path("/repo"))

    mock_update.assert_called_once_with(
        "origin/main", DEV_COMMIT, (_MINOR_CHANGE,)
    )
    branch = f"auto/docs-polish-{DEV_COMMIT[:12]}"
    mock_push_branch.assert_called_once_with(
        branch, Path("/repo/docs"), "docs: minor polish"
    )
    assert mock_open_pr.call_args.args[2] == branch
    assert "minor polish" in mock_open_pr.call_args.args[3]
    assert "reword the intro" in mock_open_pr.call_args.args[4]
    err = capfd.readouterr().err
    assert "pull/8" in err
    assert "does not block" in err


@patch("functions.build_release._open_docs_pr")
@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release.prompt_yn", return_value=True)
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_skips_the_polish_pr_when_the_update_changes_nothing(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_prompt_yn: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # An accepted offer whose update produces no docs edits leaves nothing to
    # open; the release just moves on — this path never hard-fails.
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/y.mdx",
                change="reword the intro",
                status="already_covered",
            ),
        ),
        summary="nothing to polish",
    )

    _docs_gate(repo_root=Path("/repo"))

    mock_push_branch.assert_not_called()
    mock_open_pr.assert_not_called()
    err = capfd.readouterr().err
    assert "no pull request to open" in err
    assert "up to date" in err


@patch("functions.build_release.prompt_yn")
@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release._find_open_docs_sync_pr",
    return_value="https://github.com/test-owner/test-repo/pull/8",
)
@patch(
    "functions.build_release.check_docs",
    return_value=CheckResult(changes=(_MINOR_CHANGE,)),
)
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_reports_an_open_polish_pr_without_prompting(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_find_pr: MagicMock,
    mock_update: MagicMock,
    mock_prompt_yn: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A polish pull request from an earlier attempt on this commit is
    # reported, not re-derived — and there is nothing to ask the user.
    _docs_gate(repo_root=Path("/repo"))

    mock_prompt_yn.assert_not_called()
    mock_update.assert_not_called()
    err = capfd.readouterr().err
    assert "pull/8" in err
    assert "up to date" in err


@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths", return_value=True)
def test_docs_gate_rejects_a_dirty_docs_tree_before_asking_claude(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
) -> None:
    # Those edits would be swept into the sync commit, so refuse up front rather
    # than after a multi-minute check.
    with pytest.raises(ReleaseError, match="'docs/' has uncommitted changes"):
        _docs_gate()

    mock_check.assert_not_called()


@patch("functions.build_release._open_docs_pr")
@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths")
def test_docs_gate_opens_a_pr_and_stops_the_release(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    mock_check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/x.mdx",
                change="document --verbose",
                status="implemented",
            ),
        ),
        summary="edited 2 files",
    )
    # Clean before the update, dirty after it: claude rewrote the docs.
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate(repo_root=Path("/repo"))

    assert excinfo.value.code == 1
    mock_update.assert_called_once_with(
        "origin/main", DEV_COMMIT, (_BLOCKING_CHANGE,)
    )
    # The branch is named after the release commit, so a retry on that commit
    # finds the pull request instead of producing a second one.
    branch = f"auto/docs-update-{DEV_COMMIT[:12]}"
    mock_push_branch.assert_called_once_with(
        branch, Path("/repo/docs"), "docs: sync with the code being released"
    )
    assert mock_open_pr.call_args.args[2] == branch
    # The user is pointed at the pull request that has to merge first.
    assert "pull/7" in capfd.readouterr().err


@patch("functions.build_release.prompt_yn")
@patch("functions.build_release._open_docs_pr")
@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths")
def test_docs_gate_feeds_only_blocking_changes_to_the_update_and_pr(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    mock_prompt_yn: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # A mixed verdict: the blocking gap drives the update and the pull
    # request; the minor one is only printed — the optional-polish offer
    # belongs to minor-only verdicts, so nothing is asked here.
    mock_check.return_value = CheckResult(
        changes=(_MINOR_CHANGE, _BLOCKING_CHANGE)
    )
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/x.mdx",
                change="document --verbose",
                status="implemented",
            ),
        ),
        summary="edited 1 file",
    )
    mock_has_changes.side_effect = [False, True]
    mock_open_pr.return_value = "https://github.com/test-owner/test-repo/pull/7"

    with pytest.raises(SystemExit):
        _docs_gate(repo_root=Path("/repo"))

    mock_update.assert_called_once_with(
        "origin/main", DEV_COMMIT, (_BLOCKING_CHANGE,)
    )
    body = mock_open_pr.call_args.args[4]
    assert "docs/x.mdx" in body
    assert "reword the intro" not in body
    mock_prompt_yn.assert_not_called()
    assert "reword the intro" in capfd.readouterr().err


@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release.update_docs")
@patch(
    "functions.build_release._find_open_docs_sync_pr",
    return_value="https://github.com/test-owner/test-repo/pull/7",
)
@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_stops_on_the_pr_an_earlier_attempt_opened(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_find_pr: MagicMock,
    mock_update: MagicMock,
    mock_push_branch: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Re-running the release on a commit whose docs pull request is still open
    # must point at that pull request, not spend another Claude run deriving a
    # branch that would then have to replace the one under review.
    mock_check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))

    with pytest.raises(SystemExit) as excinfo:
        _docs_gate()

    assert excinfo.value.code == 1
    mock_find_pr.assert_called_once()
    assert mock_find_pr.call_args.args[2] == f"auto/docs-update-{DEV_COMMIT[:12]}"
    mock_update.assert_not_called()
    mock_push_branch.assert_not_called()
    assert "pull/7" in capfd.readouterr().err


@patch("functions.build_release._open_docs_pr")
@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_continues_when_the_updater_finds_gaps_already_covered(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
    mock_open_pr: MagicMock,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # The check claimed a gap; the updater read the docs and verified each
    # reported gap is already documented. That is the check being noisy, not
    # the docs being stale, and it must not block the release.
    mock_check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/x.mdx",
                change="document --verbose",
                status="already_covered",
            ),
        ),
        summary="everything already documented",
    )

    _docs_gate(repo_root=Path("/repo"))

    mock_push_branch.assert_not_called()
    mock_open_pr.assert_not_called()
    assert "already" in capfd.readouterr().err


@patch("functions.build_release._push_docs_sync_branch")
@patch("functions.build_release._find_open_docs_sync_pr", return_value=None)
@patch("functions.build_release.update_docs")
@patch("functions.build_release.check_docs")
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_docs_gate_raises_when_the_update_claims_edits_but_changed_nothing(
    mock_has_changes: MagicMock,
    mock_check: MagicMock,
    mock_update: MagicMock,
    mock_find_pr: MagicMock,
    mock_push_branch: MagicMock,
) -> None:
    # An "implemented" claim with a clean docs tree is a malfunctioning
    # updater, not a noisy check; passing silently would make the gate
    # unfalsifiable, so the release stops with the manual route spelled out.
    mock_check.return_value = CheckResult(changes=(_BLOCKING_CHANGE,))
    mock_update.return_value = UpdateResult(
        results=(
            UpdateOutcome(
                file="docs/x.mdx",
                change="document --verbose",
                status="implemented",
            ),
        ),
        summary="edited 1 file",
    )

    with pytest.raises(ReleaseError, match="nothing changed there"):
        _docs_gate()

    mock_push_branch.assert_not_called()


@patch("functions.build_release.switch_branch")
@patch("functions.build_release.push_branch")
@patch("functions.build_release.commit_paths")
@patch("functions.build_release.switch_to_new_branch")
def test_push_docs_sync_branch_pushes_without_force_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    _push_docs_sync_branch(
        "auto/docs-update-abc",
        Path("/repo/docs"),
        "docs: sync with the code being released",
    )

    mock_switch_new.assert_called_once_with("auto/docs-update-abc")
    mock_commit.assert_called_once_with(
        [Path("/repo/docs")], "docs: sync with the code being released"
    )
    # push_branch is the plain, non-forced push: a docs branch that has already
    # been published is never overwritten.
    mock_push.assert_called_once_with(
        "origin", "auto/docs-update-abc", "auto/docs-update-abc"
    )
    mock_switch.assert_called_once_with("dev")


@patch("functions.build_release.switch_branch")
@patch("functions.build_release.push_branch", side_effect=ReleaseError("non-fast-forward"))
@patch("functions.build_release.commit_paths")
@patch("functions.build_release.switch_to_new_branch")
def test_push_docs_sync_branch_explains_a_rejected_push_and_returns_to_dev(
    mock_switch_new: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_switch: MagicMock,
) -> None:
    # A rejection means a branch was left behind by an attempt whose pull
    # request is gone; say how to clear it instead of overwriting it. And a
    # failed push must never strand the working tree on the throwaway branch.
    with pytest.raises(ReleaseError) as excinfo:
        _push_docs_sync_branch(
            "auto/docs-update-abc", Path("/repo/docs"), "docs: minor polish"
        )

    message = str(excinfo.value)
    assert "non-fast-forward" in message
    assert "git push origin --delete auto/docs-update-abc" in message
    mock_switch.assert_called_once_with("dev")


@patch("functions.build_release.github_api")
def test_open_docs_pr_creates_a_pr_against_dev(mock_api: MagicMock) -> None:
    slug = RepoSlug(owner="test-owner", repo="test-repo")
    mock_api.return_value = {
        "html_url": "https://github.com/test-owner/test-repo/pull/9"
    }

    url = _open_docs_pr(
        MagicMock(),
        slug,
        "auto/docs-update-abc",
        "docs: sync with the code being released (abc)",
        f"body mentioning docs/x.mdx at {DEV_COMMIT}",
    )

    assert url == "https://github.com/test-owner/test-repo/pull/9"
    payload = mock_api.call_args.kwargs["json_data"]
    assert payload["head"] == "auto/docs-update-abc"
    assert payload["base"] == "dev"
    assert payload["title"] == "docs: sync with the code being released (abc)"
    assert "docs/x.mdx" in payload["body"]
    assert DEV_COMMIT in payload["body"]


@patch("functions.build_release.github_api")
def test_find_open_docs_sync_pr_queries_the_branch_into_dev(
    mock_api: MagicMock,
) -> None:
    mock_api.return_value = [
        {"html_url": "https://github.com/test-owner/test-repo/pull/4"}
    ]

    url = _find_open_docs_sync_pr(
        MagicMock(),
        RepoSlug(owner="test-owner", repo="test-repo"),
        "auto/docs-update-abc",
    )

    assert url == "https://github.com/test-owner/test-repo/pull/4"
    query = mock_api.call_args.args[2]
    assert "head=test-owner:auto/docs-update-abc" in query
    assert "base=dev" in query
    assert "state=open" in query


@patch("functions.build_release.github_api", return_value=[])
def test_find_open_docs_sync_pr_returns_none_when_there_is_no_pr(
    mock_api: MagicMock,
) -> None:
    assert (
        _find_open_docs_sync_pr(
            MagicMock(),
            RepoSlug(owner="test-owner", repo="test-repo"),
            "auto/docs-update-abc",
        )
        is None
    )


@patch("functions.build_release.need_cmd")
@patch("functions.build_release._verify_docs_up_to_date")
@patch("functions.build_release.build_github_client")
@patch("functions.build_release.github_repo_slug")
@patch("functions.build_release.prompt", return_value="")
@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release._verify_release_branch_state", return_value=DEV_COMMIT)
@patch("functions.build_release.get_repo_root")
@patch(
    "functions.build_release.validate_release_environment", return_value="test-token"
)
@patch("functions.build_release.is_macos_arm64", return_value=True)
def test_run_full_skips_the_docs_gate_when_asked(
    mock_platform: MagicMock,
    mock_validate: MagicMock,
    mock_repo_root: MagicMock,
    mock_branch_state: MagicMock,
    mock_uncommitted: MagicMock,
    mock_prompt: MagicMock,
    mock_slug: MagicMock,
    mock_client: MagicMock,
    mock_docs_gate: MagicMock,
    mock_need_cmd: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    mock_repo_root.return_value = tmp_path

    # The empty tag aborts right after the gate would have run.
    with pytest.raises(ReleaseError, match="release tag cannot be empty"):
        _run_full(skip_docs_check=True)

    mock_docs_gate.assert_not_called()
    assert "skipping the docs freshness check" in capfd.readouterr().err


# --- committing the notes and aligning main ---


@patch("functions.build_release.set_branch_ref")
@patch("functions.build_release.is_branch_checked_out", return_value=False)
@patch("functions.build_release.get_commit", return_value=MAIN_COMMIT)
@patch("functions.build_release.push_branch")
@patch("functions.build_release.commit_paths")
@patch("functions.build_release.has_changes_in_paths", return_value=True)
def test_commit_notes_and_align_main_commits_pushes_and_aligns(
    mock_has_changes: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_get_commit: MagicMock,
    mock_checked_out: MagicMock,
    mock_set_ref: MagicMock,
    tmp_path: Path,
) -> None:
    notes = tmp_path / "v0.1.0.html"

    _commit_notes_and_align_main(notes, "v0.1.0")

    mock_commit.assert_called_once_with([notes], "docs: add release notes for v0.1.0")
    assert [c.args for c in mock_push.call_args_list] == [
        ("origin", "dev", "dev"),
        ("origin", "dev", "main"),
    ]
    # The local main ref follows the push, so the working tree never leaves dev.
    mock_set_ref.assert_called_once_with("main", MAIN_COMMIT)


@patch("functions.build_release.set_branch_ref")
@patch("functions.build_release.is_branch_checked_out", return_value=False)
@patch("functions.build_release.get_commit", return_value=MAIN_COMMIT)
@patch("functions.build_release.push_branch")
@patch("functions.build_release.commit_paths")
@patch("functions.build_release.has_changes_in_paths", return_value=False)
def test_commit_notes_and_align_main_skips_empty_commit(
    mock_has_changes: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_get_commit: MagicMock,
    mock_checked_out: MagicMock,
    mock_set_ref: MagicMock,
    tmp_path: Path,
) -> None:
    # Re-running a release whose notes are already committed must still push and
    # align rather than fail on an empty commit.
    _commit_notes_and_align_main(tmp_path / "v0.1.0.html", "v0.1.0")

    mock_commit.assert_not_called()
    assert mock_push.call_count == 2
    mock_set_ref.assert_called_once_with("main", MAIN_COMMIT)


@patch("functions.build_release.set_branch_ref")
@patch("functions.build_release.is_branch_checked_out", return_value=True)
@patch("functions.build_release.get_commit", return_value=MAIN_COMMIT)
@patch("functions.build_release.push_branch")
@patch("functions.build_release.commit_paths")
@patch("functions.build_release.has_changes_in_paths", return_value=True)
def test_commit_notes_and_align_main_leaves_checked_out_main_ref_alone(
    mock_has_changes: MagicMock,
    mock_commit: MagicMock,
    mock_push: MagicMock,
    mock_get_commit: MagicMock,
    mock_checked_out: MagicMock,
    mock_set_ref: MagicMock,
    tmp_path: Path,
    capfd: pytest.CaptureFixture[str],
) -> None:
    # Moving the ref under a worktree that has main checked out would leave that
    # worktree's index disagreeing with its HEAD.
    _commit_notes_and_align_main(tmp_path / "v0.1.0.html", "v0.1.0")

    assert mock_push.call_count == 2
    mock_set_ref.assert_not_called()
    assert "checked out" in capfd.readouterr().err


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


@patch("functions.build_release.subprocess.run")
def test_open_editor_splits_editor_with_flags(
    mock_run: MagicMock,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    # EDITOR may carry flags (e.g. "nano -c"); they must become separate argv
    # elements, not part of the executable name.
    monkeypatch.setenv("EDITOR", "nano -c")
    mock_run.return_value = MagicMock(returncode=0)
    notes = tmp_path / "notes.md"

    _open_editor(notes)

    argv = mock_run.call_args.args[0]
    assert argv == ["nano", "-c", str(notes)]


@patch(
    "functions.build_release.subprocess.run",
    side_effect=FileNotFoundError(2, "No such file or directory"),
)
def test_open_editor_missing_command_raises(
    mock_run: MagicMock,
    monkeypatch: pytest.MonkeyPatch,
    tmp_path: Path,
) -> None:
    monkeypatch.setenv("EDITOR", "definitely-not-an-editor")
    with pytest.raises(ReleaseError, match="editor command not found"):
        _open_editor(tmp_path / "notes.md")


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
@patch("functions.build_release.collect_release_changes")
@patch("functions.build_release.get_latest_release")
def test_prepare_release_content_uses_previous_release_tag(
    mock_latest: MagicMock,
    mock_collect: MagicMock,
    mock_generate: MagicMock,
    mock_confirm: MagicMock,
    tmp_path: Path,
) -> None:
    from functions.github import RepoSlug

    mock_latest.return_value = {"tag_name": "v0.11.1"}
    changes = ReleaseChanges(commit_subjects=("fix(x): do thing",), diffs=None)
    mock_collect.return_value = changes
    content = ReleaseContent("T", "D", "N")
    mock_generate.return_value = content
    client = MagicMock()
    slug = RepoSlug(owner="o", repo="r")

    result = _prepare_release_content(
        client, slug, "v0.12.0", DEV_COMMIT, tmp_path
    )

    assert result == content
    # Changes are gathered from the previous release tag up to the exact commit
    # being released, not via the GitHub API.
    mock_collect.assert_called_once_with("v0.11.1", DEV_COMMIT, tmp_path)
    mock_generate.assert_called_once_with(changes, "v0.12.0", tmp_path)
    mock_confirm.assert_called_once_with(content)


@patch("functions.build_release._confirm_release_content", side_effect=lambda c: c)
@patch("functions.build_release.generate_release_content")
@patch("functions.build_release.collect_release_changes")
@patch("functions.build_release.get_latest_release", return_value=None)
def test_prepare_release_content_handles_no_previous_release(
    mock_latest: MagicMock,
    mock_collect: MagicMock,
    mock_generate: MagicMock,
    mock_confirm: MagicMock,
    tmp_path: Path,
) -> None:
    from functions.github import RepoSlug

    mock_collect.return_value = ReleaseChanges(("initial commit",), diffs=None)
    mock_generate.return_value = ReleaseContent("T", "D", "N")

    _prepare_release_content(
        MagicMock(), RepoSlug("o", "r"), "v0.1.0", DEV_COMMIT, tmp_path
    )

    # With no prior release, the change collection falls back to the full history.
    mock_collect.assert_called_once_with(None, DEV_COMMIT, tmp_path)
