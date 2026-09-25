"""Tests for functions.build_release."""

from __future__ import annotations

from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from functions.build import BuildArtifact
from functions.build_release import _build_all_targets, _run_local, main
from functions.cli import ReleaseError


@patch("functions.build_release._build_all_targets")
@patch(
    "functions.build_release.get_targets_for_platform",
    return_value=["x86_64-unknown-linux-gnu"],
)
@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.need_cmd")
@patch("functions.build_release.prompt", return_value="v0.1.0")
def test_run_local_on_linux_builds_native_only(
    mock_prompt: MagicMock,
    mock_need_cmd: MagicMock,
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
    # A local build publishes nothing, so it needs the build tools and no token.
    assert [c.args[0] for c in mock_need_cmd.call_args_list] == ["git", "cargo", "rustc"]
    mock_build_all.assert_called_once_with(
        "v0.1.0",
        ["x86_64-unknown-linux-gnu"],
        tmp_path,
    )


@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.need_cmd")
@patch("functions.build_release.prompt", return_value="")
def test_run_local_mode_empty_tag_raises(
    mock_prompt: MagicMock,
    mock_need_cmd: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    with pytest.raises(ReleaseError, match="release tag cannot be empty"):
        _run_local()


@patch("functions.build_release._build_all_targets", return_value=[])
@patch(
    "functions.build_release.get_targets_for_platform",
    return_value=["x86_64-unknown-linux-gnu"],
)
@patch("functions.build_release.has_uncommitted_changes", return_value=False)
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.need_cmd")
@patch("functions.build_release.prompt")
def test_run_local_with_a_tag_builds_it_without_prompting(
    mock_prompt: MagicMock,
    mock_need_cmd: MagicMock,
    mock_repo_root: MagicMock,
    mock_uncommitted: MagicMock,
    mock_targets: MagicMock,
    mock_build_all: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    _run_local("test")
    mock_prompt.assert_not_called()
    mock_build_all.assert_called_once_with(
        "test",
        ["x86_64-unknown-linux-gnu"],
        tmp_path,
    )


@patch("functions.build_release.prompt")
@patch("functions.build_release.get_repo_root")
@patch("functions.build_release.need_cmd")
def test_run_local_with_an_empty_tag_raises_without_prompting(
    mock_need_cmd: MagicMock,
    mock_repo_root: MagicMock,
    mock_prompt: MagicMock,
    tmp_path: Path,
) -> None:
    mock_repo_root.return_value = tmp_path
    with patch(
        "functions.build_release.has_uncommitted_changes", return_value=False
    ), pytest.raises(ReleaseError, match="release tag cannot be empty"):
        _run_local("")
    mock_prompt.assert_not_called()



# --- the command line ---


@patch("functions.build_release._run_local")
def test_main_passes_the_tag_to_the_local_build(mock_run_local: MagicMock) -> None:
    with patch("sys.argv", ["build-release", "--local", "--tag", "test"]):
        main()
    mock_run_local.assert_called_once_with("test")


@patch("functions.build_release._run_local")
def test_main_prompts_for_the_local_tag_without_the_option(
    mock_run_local: MagicMock,
) -> None:
    with patch("sys.argv", ["build-release", "--local"]):
        main()
    mock_run_local.assert_called_once_with(None)


def test_tag_without_local_is_rejected(capsys: pytest.CaptureFixture[str]) -> None:
    with patch(
        "sys.argv", ["build-release", "--base-images", "--tag", "test"]
    ), pytest.raises(SystemExit) as exited:
        main()
    assert exited.value.code == 2
    assert "--tag applies to --local only" in capsys.readouterr().err



@pytest.mark.parametrize(
    "argv",
    [
        ["build-release"],
        # --tag alone names no mode either.
        ["build-release", "--tag", "v0.3.0"],
    ],
)
def test_main_refuses_to_run_without_a_mode(
    argv: list[str], capsys: pytest.CaptureFixture[str]
) -> None:
    # Releases publish from the workflow alone; without --local or
    # --base-images there is nothing this script may do.
    with patch("sys.argv", argv), patch(
        "functions.build_release._run_local"
    ) as mock_run_local, patch(
        "functions.build_release.build_base_images_main"
    ) as mock_base_images, pytest.raises(SystemExit) as exited:
        main()

    assert exited.value.code == 2
    err = " ".join(capsys.readouterr().err.split())
    assert 'releases publish only from the "Parallel release" workflow' in err
    assert (
        "Pass --local to build the release archives on this host without "
        "publishing them"
    ) in err
    mock_run_local.assert_not_called()
    mock_base_images.assert_not_called()


def test_main_refuses_both_modes_at_once(capsys: pytest.CaptureFixture[str]) -> None:
    with patch("sys.argv", ["build-release", "--local", "--base-images"]), patch(
        "functions.build_release._run_local"
    ) as mock_run_local, pytest.raises(SystemExit) as exited:
        main()

    assert exited.value.code == 2
    assert "not allowed with argument" in capsys.readouterr().err
    mock_run_local.assert_not_called()


@patch("functions.build_release.build_base_images_main")
@patch("functions.build_release._run_local")
def test_main_builds_the_base_images(
    mock_run_local: MagicMock, mock_base_images: MagicMock
) -> None:
    with patch("sys.argv", ["build-release", "--base-images"]):
        main()

    mock_base_images.assert_called_once_with()
    mock_run_local.assert_not_called()


# --- building every target ---


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

