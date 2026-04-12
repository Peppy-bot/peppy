"""Tests for functions.docker module."""

from __future__ import annotations

import base64
import json
from pathlib import Path
from unittest.mock import MagicMock, call, patch

import pytest

from functions.cli import ReleaseError
from functions.docker import (
    BASE_IMAGES,
    BUILDX_BUILDER_NAME,
    DOCKER_HUB_ACCOUNT,
    DOCKER_HUB_REGISTRY,
    DOCKER_PLATFORMS,
    BaseImage,
    _ensure_buildx_builder,
    _get_docker_hub_username,
    _get_username_from_cred_store,
    _inspect_builder_platforms,
    build_all_base_images,
    build_and_push_base_image,
    validate_docker_environment,
    _run,
)


def _write_docker_config(tmp_path: Path, username: str, token: str = "tok") -> Path:
    """Write a fake ~/.docker/config.json and return its path."""
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    config_path = docker_dir / "config.json"
    auth = base64.b64encode(f"{username}:{token}".encode()).decode()
    config_path.write_text(
        json.dumps({"auths": {DOCKER_HUB_REGISTRY: {"auth": auth}}}),
        encoding="utf-8",
    )
    return tmp_path


# --- _get_docker_hub_username ---


def test_get_docker_hub_username_success(tmp_path: Path) -> None:
    home = _write_docker_config(tmp_path, "tuatini")
    with patch("functions.docker.Path.home", return_value=home):
        assert _get_docker_hub_username() == "tuatini"


def test_get_docker_hub_username_no_config(tmp_path: Path) -> None:
    with patch("functions.docker.Path.home", return_value=tmp_path):
        assert _get_docker_hub_username() is None


def test_get_docker_hub_username_no_auth_entry(tmp_path: Path) -> None:
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    (docker_dir / "config.json").write_text('{"auths": {}}', encoding="utf-8")
    with patch("functions.docker.Path.home", return_value=tmp_path):
        assert _get_docker_hub_username() is None


def test_get_docker_hub_username_invalid_json(tmp_path: Path) -> None:
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    (docker_dir / "config.json").write_text("not json", encoding="utf-8")
    with patch("functions.docker.Path.home", return_value=tmp_path):
        assert _get_docker_hub_username() is None


def test_get_docker_hub_username_creds_store_fallback(tmp_path: Path) -> None:
    """When auth entry is empty but credsStore is set, delegate to helper."""
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    config = {
        "auths": {DOCKER_HUB_REGISTRY: {}},
        "credsStore": "osxkeychain",
    }
    (docker_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
    with patch("functions.docker.Path.home", return_value=tmp_path), \
         patch("functions.docker._get_username_from_cred_store", return_value="tuatini") as mock_helper:
        assert _get_docker_hub_username() == "tuatini"
    mock_helper.assert_called_once_with("osxkeychain")


def test_get_docker_hub_username_creds_store_no_registry_entry(tmp_path: Path) -> None:
    """credsStore is set but registry not in auths -- returns None."""
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    config = {"auths": {}, "credsStore": "osxkeychain"}
    (docker_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
    with patch("functions.docker.Path.home", return_value=tmp_path):
        assert _get_docker_hub_username() is None


# --- _get_username_from_cred_store ---


def test_get_username_from_cred_store_success() -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(
            returncode=0,
            stdout=json.dumps({"Username": "tuatini", "Secret": "tok"}),
        )
        assert _get_username_from_cred_store("osxkeychain") == "tuatini"

    cmd = mock_run.call_args[0][0]
    assert cmd == ["docker-credential-osxkeychain", "get"]
    assert mock_run.call_args[1]["input"] == DOCKER_HUB_REGISTRY


def test_get_username_from_cred_store_helper_not_found() -> None:
    with patch("functions.docker.subprocess.run", side_effect=FileNotFoundError):
        assert _get_username_from_cred_store("missing") is None


def test_get_username_from_cred_store_nonzero_exit() -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1, stdout="")
        assert _get_username_from_cred_store("osxkeychain") is None


def test_get_username_from_cred_store_invalid_json() -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout="not json")
        assert _get_username_from_cred_store("osxkeychain") is None


# --- validate_docker_environment ---


def test_validate_docker_environment_success(tmp_path: Path) -> None:
    home = _write_docker_config(tmp_path, DOCKER_HUB_ACCOUNT)
    with patch("functions.docker.need_cmd") as mock_need, \
         patch("functions.docker.subprocess.run") as mock_run, \
         patch("functions.docker.Path.home", return_value=home), \
         patch("functions.docker._ensure_buildx_builder"):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        validate_docker_environment()

    mock_need.assert_called_once_with("docker")
    mock_run.assert_called_once()


def test_validate_docker_environment_missing_docker() -> None:
    with patch("functions.docker.need_cmd", side_effect=ReleaseError("missing required command: docker")):
        with pytest.raises(ReleaseError, match="missing required command: docker"):
            validate_docker_environment()


def test_validate_docker_environment_no_buildx() -> None:
    with patch("functions.docker.need_cmd"), \
         patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx is not available"):
            validate_docker_environment()


def test_validate_docker_environment_not_logged_in(tmp_path: Path) -> None:
    with patch("functions.docker.need_cmd"), \
         patch("functions.docker.subprocess.run") as mock_run, \
         patch("functions.docker.Path.home", return_value=tmp_path):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        with pytest.raises(ReleaseError, match="not logged into Docker Hub"):
            validate_docker_environment()


def test_validate_docker_environment_wrong_account(tmp_path: Path) -> None:
    home = _write_docker_config(tmp_path, "wrong_user")
    with patch("functions.docker.need_cmd"), \
         patch("functions.docker.subprocess.run") as mock_run, \
         patch("functions.docker.Path.home", return_value=home):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        with pytest.raises(ReleaseError, match="logged in as 'wrong_user'"):
            validate_docker_environment()


# --- build_and_push_base_image ---


def test_build_and_push_base_image_success(tmp_path: Path) -> None:
    image = BASE_IMAGES[0]
    (tmp_path / image.dockerfile_dir).mkdir()
    tags = ["latest", "v0.1.0"]

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        build_and_push_base_image(image, tags, tmp_path)

    mock_run.assert_called_once()
    cmd = mock_run.call_args[0][0]
    assert cmd[:3] == ["docker", "buildx", "build"]
    assert "--builder" in cmd
    assert BUILDX_BUILDER_NAME in cmd
    assert "--platform" in cmd
    platform_idx = cmd.index("--platform")
    assert cmd[platform_idx + 1] == ",".join(DOCKER_PLATFORMS)
    assert "--push" in cmd
    assert f"{image.repo}:latest" in cmd
    assert f"{image.repo}:v0.1.0" in cmd
    assert str(tmp_path / image.dockerfile_dir) == cmd[-1]


def test_build_and_push_base_image_failure(tmp_path: Path) -> None:
    image = BASE_IMAGES[0]
    (tmp_path / image.dockerfile_dir).mkdir()

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx build failed"):
            build_and_push_base_image(image, ["latest"], tmp_path)


# --- build_all_base_images ---


def test_build_all_base_images_builds_every_image(tmp_path: Path) -> None:
    for image in BASE_IMAGES:
        (tmp_path / "base_images" / image.dockerfile_dir).mkdir(parents=True)

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        build_all_base_images(tmp_path, "v0.2.0")

    assert mock_run.call_count == len(BASE_IMAGES)

    for i, image in enumerate(BASE_IMAGES):
        cmd = mock_run.call_args_list[i][0][0]
        assert f"{image.repo}:latest" in cmd
        assert f"{image.repo}:v0.2.0" in cmd


def test_build_all_base_images_stops_on_first_failure(tmp_path: Path) -> None:
    for image in BASE_IMAGES:
        (tmp_path / "base_images" / image.dockerfile_dir).mkdir(parents=True)

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx build failed"):
            build_all_base_images(tmp_path, "v0.3.0")

    assert mock_run.call_count == 1


# --- _inspect_builder_platforms ---


INSPECT_OUTPUT_BOTH_PLATFORMS = """\
Name:          peppy-multiplatform
Driver:        docker-container
Nodes:
Name:          peppy-multiplatform0
Platforms:     linux/amd64, linux/arm64/v8
"""

INSPECT_OUTPUT_AMD64_ONLY = """\
Name:          peppy-multiplatform
Driver:        docker-container
Nodes:
Name:          peppy-multiplatform0
Platforms:     linux/amd64
"""


def test_inspect_builder_platforms_parses_both() -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(
            returncode=0, stdout=INSPECT_OUTPUT_BOTH_PLATFORMS
        )
        platforms = _inspect_builder_platforms(BUILDX_BUILDER_NAME)

    assert platforms == {"linux/amd64", "linux/arm64"}


def test_inspect_builder_platforms_strips_qualifiers() -> None:
    """linux/arm64/v8 should be normalised to linux/arm64."""
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(
            returncode=0, stdout=INSPECT_OUTPUT_BOTH_PLATFORMS
        )
        platforms = _inspect_builder_platforms(BUILDX_BUILDER_NAME)

    assert "linux/arm64" in platforms


def test_inspect_builder_platforms_returns_empty_on_missing_builder() -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1, stdout="", stderr="")
        platforms = _inspect_builder_platforms("nonexistent")

    assert platforms == set()


# --- _ensure_buildx_builder ---


def test_ensure_buildx_builder_exists_with_platforms() -> None:
    """Builder already exists with both platforms -- no creation call."""
    with patch("functions.docker._inspect_builder_platforms") as mock_inspect:
        mock_inspect.return_value = {"linux/amd64", "linux/arm64"}
        _ensure_buildx_builder()

    mock_inspect.assert_called_once_with(BUILDX_BUILDER_NAME)


def test_ensure_buildx_builder_creates_when_missing() -> None:
    """Builder doesn't exist, creation succeeds, re-inspect shows platforms."""
    with patch("functions.docker._inspect_builder_platforms") as mock_inspect, \
         patch("functions.docker.subprocess.run") as mock_run:
        mock_inspect.side_effect = [
            set(),  # first inspect: builder doesn't exist
            {"linux/amd64", "linux/arm64"},  # re-inspect after creation
        ]
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx create

        _ensure_buildx_builder()

    assert mock_inspect.call_count == 2
    cmd = mock_run.call_args[0][0]
    assert "create" in cmd
    assert "--driver" in cmd
    assert "docker-container" in cmd
    assert "--bootstrap" in cmd


def test_ensure_buildx_builder_create_failure() -> None:
    """Builder creation fails -- raises ReleaseError."""
    with patch("functions.docker._inspect_builder_platforms") as mock_inspect, \
         patch("functions.docker.subprocess.run") as mock_run:
        mock_inspect.return_value = set()
        mock_run.return_value = MagicMock(returncode=1)

        with pytest.raises(ReleaseError, match="failed to create buildx builder"):
            _ensure_buildx_builder()


def test_ensure_buildx_builder_missing_qemu() -> None:
    """Builder exists but missing arm64 platform -- raises with QEMU instructions."""
    with patch("functions.docker._inspect_builder_platforms") as mock_inspect:
        mock_inspect.return_value = {"linux/amd64"}  # arm64 missing

        with pytest.raises(ReleaseError, match="missing platforms.*linux/arm64"):
            _ensure_buildx_builder()


# --- _run (interactive entry point) ---


def test_run_empty_tag_raises() -> None:
    with patch("functions.docker.validate_docker_environment"), \
         patch("functions.docker.prompt", return_value=""):
        with pytest.raises(ReleaseError, match="tag cannot be empty"):
            _run()


def test_run_calls_build_all_with_tag() -> None:
    with patch("functions.docker.validate_docker_environment") as mock_validate, \
         patch("functions.docker.prompt", return_value="v1.0.0"), \
         patch("functions.docker.build_all_base_images") as mock_build:
        _run()

    mock_validate.assert_called_once()
    mock_build.assert_called_once()
    _, tag = mock_build.call_args[0]
    assert tag == "v1.0.0"
