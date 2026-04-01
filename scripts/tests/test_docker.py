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
    DOCKER_HUB_ACCOUNT,
    DOCKER_HUB_REGISTRY,
    BaseImage,
    _get_docker_hub_username,
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


# --- validate_docker_environment ---


def test_validate_docker_environment_success(tmp_path: Path) -> None:
    home = _write_docker_config(tmp_path, DOCKER_HUB_ACCOUNT)
    with patch("functions.docker.need_cmd") as mock_need, \
         patch("functions.docker.subprocess.run") as mock_run, \
         patch("functions.docker.Path.home", return_value=home):
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
    assert cmd[:4] == ["docker", "buildx", "build", "--push"]
    assert "-t" in cmd
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
