"""Tests for functions.docker module."""

from __future__ import annotations

import base64
import json
import os
import subprocess
from collections.abc import Mapping, Sequence
from datetime import UTC, datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import MagicMock, patch

import pytest

from functions.cli import ReleaseError
from functions.docker import (
    APT_PROBES,
    APT_SNAPSHOT_BUILD_ARG,
    APT_SNAPSHOT_SCRIPT,
    BASE_IMAGES,
    BUILDX_BUILDER_NAME,
    DOCKER_HUB_ACCOUNT,
    DOCKER_HUB_REGISTRY,
    DOCKER_PLATFORMS,
    AptSnapshot,
    BaseImage,
    _ensure_buildx_builder,
    _get_docker_hub_username,
    _get_username_from_cred_store,
    _inspect_builder_platforms,
    _require_tag,
    _run_build,
    _run_move_latest,
    _utc_now,
    build_all_base_images,
    build_and_push_base_image,
    main,
    move_latest_main,
    move_latest_to,
    probe_base_image,
    read_apt_snapshot,
    validate_docker_environment,
)

from .helpers import flag_value

SNAPSHOT = AptSnapshot("20261110T073833Z")
OTHER_SNAPSHOT = AptSnapshot("20261111T120000Z")

# The directory the base images build from, the build context of each.
BASE_IMAGES_DIR = Path(__file__).resolve().parent.parent / "base_images"


def _flag_values(cmd: Sequence[str], flag: str) -> list[str]:
    """Return the argv element following each *flag* in *cmd*."""
    return [cmd[index + 1] for index, arg in enumerate(cmd) if arg == flag]


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
    home = _write_docker_config(tmp_path, "peppybot")
    with patch("functions.docker.Path.home", return_value=home):
        assert _get_docker_hub_username() == "peppybot"


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


@pytest.mark.parametrize(
    "auth",
    [
        "not-base64!",
        "caf\u00e9",
        base64.b64encode(b"\xff\xfe:tok").decode(),
        12345,
        ["peppybot:tok"],
    ],
    ids=["bad-padding", "not-ascii", "not-utf8", "number", "list"],
)
def test_get_docker_hub_username_undecodable_auth(tmp_path: Path, auth: object) -> None:
    docker_dir = tmp_path / ".docker"
    docker_dir.mkdir()
    config = {"auths": {DOCKER_HUB_REGISTRY: {"auth": auth}}}
    (docker_dir / "config.json").write_text(json.dumps(config), encoding="utf-8")
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
    with (
        patch("functions.docker.Path.home", return_value=tmp_path),
        patch(
            "functions.docker._get_username_from_cred_store", return_value="peppybot"
        ) as mock_helper,
    ):
        assert _get_docker_hub_username() == "peppybot"
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
            stdout=json.dumps({"Username": "peppybot", "Secret": "tok"}),
        )
        assert _get_username_from_cred_store("osxkeychain") == "peppybot"

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
    with (
        patch("functions.docker.need_cmd") as mock_need,
        patch("functions.docker.subprocess.run") as mock_run,
        patch("functions.docker.Path.home", return_value=home),
        patch("functions.docker._ensure_buildx_builder"),
    ):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        validate_docker_environment()

    mock_need.assert_called_once_with("docker")
    mock_run.assert_called_once()


def test_validate_docker_environment_missing_docker() -> None:
    with (
        patch(
            "functions.docker.need_cmd",
            side_effect=ReleaseError("missing required command: docker"),
        ),
        pytest.raises(ReleaseError, match="missing required command: docker"),
    ):
        validate_docker_environment()


def test_validate_docker_environment_no_buildx() -> None:
    with (
        patch("functions.docker.need_cmd"),
        patch("functions.docker.subprocess.run") as mock_run,
    ):
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx is not available"):
            validate_docker_environment()


def test_validate_docker_environment_not_logged_in(tmp_path: Path) -> None:
    with (
        patch("functions.docker.need_cmd"),
        patch("functions.docker.subprocess.run") as mock_run,
        patch("functions.docker.Path.home", return_value=tmp_path),
    ):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        with pytest.raises(ReleaseError, match="not logged into Docker Hub"):
            validate_docker_environment()


def test_validate_docker_environment_wrong_account(tmp_path: Path) -> None:
    home = _write_docker_config(tmp_path, "wrong_user")
    with (
        patch("functions.docker.need_cmd"),
        patch("functions.docker.subprocess.run") as mock_run,
        patch("functions.docker.Path.home", return_value=home),
    ):
        mock_run.return_value = MagicMock(returncode=0)  # docker buildx version
        with pytest.raises(ReleaseError, match="logged in as 'wrong_user'"):
            validate_docker_environment()


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

    # linux/arm64/v8 is normalised to linux/arm64.
    assert platforms == {"linux/amd64", "linux/arm64"}


def test_inspect_builder_platforms_skips_an_entry_without_an_architecture() -> None:
    stdout = (
        "Name:          peppy-multiplatform\nPlatforms:     linux/amd64, unknown,\n"
    )
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0, stdout=stdout)
        platforms = _inspect_builder_platforms(BUILDX_BUILDER_NAME)

    assert platforms == {"linux/amd64"}


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
    with (
        patch("functions.docker._inspect_builder_platforms") as mock_inspect,
        patch("functions.docker.subprocess.run") as mock_run,
    ):
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
    with (
        patch("functions.docker._inspect_builder_platforms") as mock_inspect,
        patch("functions.docker.subprocess.run") as mock_run,
    ):
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


# --- AptSnapshot ---


def test_apt_snapshot_at_is_the_utc_time_in_the_form_the_service_reads() -> None:
    moment = datetime(2026, 11, 10, 7, 38, 33, tzinfo=UTC)
    assert AptSnapshot.at(moment).id == "20261110T073833Z"


def test_apt_snapshot_at_converts_a_time_of_another_zone_to_utc() -> None:
    # 20:38:33 at UTC+13 is 07:38:33 UTC. A snapshot id has no fraction of a
    # second.
    moment = datetime(
        2026, 11, 10, 20, 38, 33, 999_999, tzinfo=timezone(timedelta(hours=13))
    )
    assert AptSnapshot.at(moment) == SNAPSHOT


def test_apt_snapshot_at_refuses_a_time_without_a_zone() -> None:
    with pytest.raises(ValueError, match="time zone"):
        AptSnapshot.at(datetime(2026, 11, 10, 7, 38, 33))


@pytest.mark.parametrize(
    "snapshot_id",
    [
        "",
        "latest",
        "2026-11-10T07:38:33Z",
        "20261110T073833",
        "20261110t073833z",
        "20261110T073833Z ",
        "20261110T073833Z\n",
        "20261110",
        # Fields with fewer digits than the form.
        "2026111T073833Z",
        "20261110T73833Z",
        # No such date or time.
        "20261310T073833Z",
        "20260230T073833Z",
        "20261110T250000Z",
    ],
)
def test_apt_snapshot_refuses_an_id_in_another_form(snapshot_id: str) -> None:
    with pytest.raises(ReleaseError, match="is not an Ubuntu snapshot id"):
        AptSnapshot(snapshot_id)


# --- the Dockerfiles and the snapshot script ---


@pytest.mark.parametrize("image", BASE_IMAGES, ids=lambda image: image.name)
def test_dockerfile_touches_apt_through_the_snapshot_script_alone(
    image: BaseImage,
) -> None:
    dockerfile = (BASE_IMAGES_DIR / image.dockerfile_dir / "Dockerfile").read_text(
        encoding="utf-8"
    )

    assert f"ARG {APT_SNAPSHOT_BUILD_ARG}\n" in dockerfile
    assert (
        f"RUN --mount=type=bind,source={APT_SNAPSHOT_SCRIPT},"
        f"target=/run/{APT_SNAPSHOT_SCRIPT} \\\n"
        f'    sh /run/{APT_SNAPSHOT_SCRIPT} setup "${APT_SNAPSHOT_BUILD_ARG}" '
    ) in dockerfile
    # An install outside the script would skip its checks of the image.
    assert "apt-get" not in dockerfile
    assert "apt install" not in dockerfile


def test_build_arg_and_label_names() -> None:
    # The Dockerfiles declare the argument and the containers guide names the
    # label, so neither changes with the code alone.
    assert APT_SNAPSHOT_BUILD_ARG == "APT_SNAPSHOT"
    assert APT_SNAPSHOT_SCRIPT == "apt_snapshot.sh"
    assert (BASE_IMAGES_DIR / APT_SNAPSHOT_SCRIPT).is_file()


_NOTHING_TO_UPGRADE = (
    "Reading package lists...\n"
    "Calculating upgrade...\n"
    "0 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n"
)
_UPGRADES = (
    "Inst perl-base [5.38.2-3.2ubuntu0.6] (5.38.2-3.2ubuntu0.7 "
    "Ubuntu:24.04/noble-security [amd64])\n"
    "Inst apt [2.8.3] (2.8.4ubuntu0.1 Ubuntu:24.04/noble-updates [amd64])\n"
)
_SIMULATED_UPGRADES = (
    "Reading package lists...\n"
    "The following packages will be upgraded:\n"
    "  apt perl-base\n"
    "2 upgraded, 0 newly installed, 0 to remove and 0 not upgraded.\n"
    f"{_UPGRADES}"
    "Conf perl-base (5.38.2-3.2ubuntu0.7 Ubuntu:24.04/noble-security [amd64])\n"
)

_SNAPSHOT_REPOSITORY = f"https://snapshot.ubuntu.com/ubuntu/{SNAPSHOT.id}/"
_REGULAR_MIRROR_OF_ARCHITECTURE = {
    "amd64": "http://archive.ubuntu.com/ubuntu/",
    "arm64": "http://ports.ubuntu.com/ubuntu-ports/",
}
_PROBE_MODES = ("probe-snapshot", "probe-fallback")

# Stands in for apt-get: records its arguments, and answers the two calls
# whose output the script reads from the files the test names.
_FAKE_APT_GET = """\
#!/bin/sh
echo "$*" >> "$FAKE_APT_CALLS"
case "$*" in
    "update --print-uris") cat "$FAKE_APT_INDEX_URIS" ;;
    "--simulate dist-upgrade") cat "$FAKE_APT_SIMULATION" ;;
esac
"""

# Stands in for dpkg: answers the architecture the test names.
_FAKE_DPKG = """\
#!/bin/sh
[ "$*" = "--print-architecture" ] || exit 2
echo "$FAKE_DPKG_ARCHITECTURE"
"""


def _mirror_list(snapshot_id: str, regular_mirror: str) -> str:
    """A mirror list with the snapshot *snapshot_id* first."""
    return (
        f"https://snapshot.ubuntu.com/ubuntu/{snapshot_id}/\tpriority:1\n"
        f"{regular_mirror}\tpriority:2\n"
    )


_MIRROR_LIST_OF_AMD64 = _mirror_list(SNAPSHOT.id, "http://archive.ubuntu.com/ubuntu/")


def _apt_etc_dir(tmp_path: Path) -> Path:
    return tmp_path / "etc-apt"


def _mirror_list_repository(tmp_path: Path) -> str:
    """The repository URI under which apt names the files of the mirror list."""
    return f"mirror+file:{_apt_etc_dir(tmp_path) / 'peppy-ubuntu-mirrors.txt'}/"


def _index_lines(repository: str, suites: Sequence[str] = ("noble",)) -> str:
    """The lines `apt-get update --print-uris` prints for *repository*."""
    lines = []
    for suite in suites:
        for index in ("InRelease", "main/binary-amd64/Packages.gz"):
            lines.append(f"'{repository}dists/{suite}/{index}' {suite}_{index} 0 ")
    return "".join(f"{line}\n" for line in lines)


def _install_stand_in(bin_dir: Path, name: str, script: str) -> None:
    command = bin_dir / name
    command.write_text(script, encoding="utf-8")
    command.chmod(0o755)


def _run_snapshot_script(
    tmp_path: Path,
    arguments: Sequence[str],
    index_uris: str = "",
    simulation: str = _NOTHING_TO_UPGRADE,
    architecture: str = "amd64",
    mirror_list: str | None = None,
) -> tuple[subprocess.CompletedProcess[str], list[str]]:
    """Run the snapshot script with stand-ins for apt-get and dpkg first on
    PATH, and its apt configuration directory in *tmp_path*.

    *mirror_list* is the content of the mirror list file, which does not
    exist when it is None. Returns the finished process and the arguments of
    each apt-get call.
    """
    bin_dir = tmp_path / "bin"
    bin_dir.mkdir()
    _install_stand_in(bin_dir, "apt-get", _FAKE_APT_GET)
    _install_stand_in(bin_dir, "dpkg", _FAKE_DPKG)

    apt_etc_dir = _apt_etc_dir(tmp_path)
    apt_etc_dir.mkdir()
    if mirror_list is not None:
        (apt_etc_dir / "peppy-ubuntu-mirrors.txt").write_text(
            mirror_list, encoding="utf-8"
        )

    calls = tmp_path / "apt_calls"
    calls.touch()
    index_uris_file = tmp_path / "index_uris"
    index_uris_file.write_text(index_uris, encoding="utf-8")
    simulation_file = tmp_path / "simulation"
    simulation_file.write_text(simulation, encoding="utf-8")

    result = subprocess.run(
        ["sh", str(BASE_IMAGES_DIR / APT_SNAPSHOT_SCRIPT), *arguments],
        capture_output=True,
        text=True,
        check=False,
        env={
            "PATH": f"{bin_dir}{os.pathsep}{os.environ['PATH']}",
            "APT_ETC_DIR": str(apt_etc_dir),
            "FAKE_APT_CALLS": str(calls),
            "FAKE_APT_INDEX_URIS": str(index_uris_file),
            "FAKE_APT_SIMULATION": str(simulation_file),
            "FAKE_DPKG_ARCHITECTURE": architecture,
        },
    )
    return result, calls.read_text(encoding="utf-8").splitlines()


def test_probe_modes_are_the_modes_the_release_scripts_run() -> None:
    assert tuple(probe.mode for probe in APT_PROBES) == _PROBE_MODES


@pytest.mark.parametrize(
    "arguments",
    [
        [],
        ["setup"],
        ["probe-snapshot"],
        ["probe-fallback"],
        ["probe", SNAPSHOT.id],
        ["upgrade", SNAPSHOT.id],
        ["probe-snapshot", SNAPSHOT.id, "curl"],
        ["probe-fallback", SNAPSHOT.id, "curl"],
    ],
    ids=[
        "nothing",
        "setup-alone",
        "probe-snapshot-alone",
        "probe-fallback-alone",
        "probe",
        "unknown-mode",
        "probe-snapshot-package",
        "probe-fallback-package",
    ],
)
def test_snapshot_script_refuses_a_call_in_another_form(
    tmp_path: Path, arguments: list[str]
) -> None:
    result, apt_calls = _run_snapshot_script(tmp_path, arguments)

    assert result.returncode == 1
    assert result.stderr == (
        "apt_snapshot.sh: error: usage: apt_snapshot.sh setup <snapshot id> "
        "[<package>...] | apt_snapshot.sh probe-snapshot <snapshot id> | "
        "apt_snapshot.sh probe-fallback <snapshot id>\n"
    )
    assert apt_calls == []


@pytest.mark.parametrize("mode", ["setup", *_PROBE_MODES])
@pytest.mark.parametrize(
    "snapshot_id",
    ["", "latest", "2026-11-10", "20261110T073833", "x20261110T073833Z"],
)
def test_snapshot_script_refuses_an_id_in_another_form(
    tmp_path: Path, mode: str, snapshot_id: str
) -> None:
    result, apt_calls = _run_snapshot_script(tmp_path, [mode, snapshot_id])

    assert result.returncode == 1
    assert f"'{snapshot_id}' is not a snapshot id" in result.stderr
    assert apt_calls == []


@pytest.mark.parametrize("architecture", ["amd64", "arm64"])
def test_snapshot_script_probe_snapshot_passes_on_the_mirror_list(
    tmp_path: Path, architecture: str
) -> None:
    mirror_list = _mirror_list(
        SNAPSHOT.id, _REGULAR_MIRROR_OF_ARCHITECTURE[architecture]
    )
    index_uris = _index_lines(
        _mirror_list_repository(tmp_path),
        suites=("noble", "noble-updates", "noble-security"),
    )

    result, apt_calls = _run_snapshot_script(
        tmp_path,
        ["probe-snapshot", SNAPSHOT.id],
        index_uris=index_uris,
        architecture=architecture,
        mirror_list=mirror_list,
    )

    assert result.returncode == 0, result.stderr
    assert apt_calls == [
        "update --print-uris",
        "update --error-on=any",
        "--simulate dist-upgrade",
        "install -y --no-install-recommends hello",
    ]


def test_snapshot_script_probe_fallback_passes_on_an_index_that_upgrades_the_image(
    tmp_path: Path,
) -> None:
    # The regular mirror is newer than the snapshot, so its index can upgrade
    # packages of the image: the probe does not ask.
    result, apt_calls = _run_snapshot_script(
        tmp_path,
        ["probe-fallback", SNAPSHOT.id],
        index_uris=_index_lines(_mirror_list_repository(tmp_path)),
        simulation=_SIMULATED_UPGRADES,
        mirror_list=_MIRROR_LIST_OF_AMD64,
    )

    assert result.returncode == 0, result.stderr
    assert apt_calls == [
        "update --print-uris",
        "update --error-on=any",
        "install -y --no-install-recommends hello",
    ]


def test_snapshot_script_probe_snapshot_stops_at_a_package_the_snapshot_upgrades(
    tmp_path: Path,
) -> None:
    result, apt_calls = _run_snapshot_script(
        tmp_path,
        ["probe-snapshot", SNAPSHOT.id],
        index_uris=_index_lines(_mirror_list_repository(tmp_path)),
        simulation=_SIMULATED_UPGRADES,
        mirror_list=_MIRROR_LIST_OF_AMD64,
    )

    assert result.returncode == 1
    assert result.stderr == (
        f"apt_snapshot.sh: error: the snapshot {SNAPSHOT.id} upgrades packages "
        f"of the image:\n{_UPGRADES}"
    )
    assert apt_calls == [
        "update --print-uris",
        "update --error-on=any",
        "--simulate dist-upgrade",
    ]


@pytest.mark.parametrize("mode", _PROBE_MODES)
@pytest.mark.parametrize(
    "other_repository",
    [
        # The snapshot itself: sources without the regular mirror.
        _SNAPSHOT_REPOSITORY,
        "http://archive.ubuntu.com/ubuntu/",
        "http://ports.ubuntu.com/ubuntu-ports/",
        "mirror+file:/etc/apt/other-mirrors.txt/",
    ],
)
def test_snapshot_script_probe_stops_at_a_source_other_than_the_mirror_list(
    tmp_path: Path, mode: str, other_repository: str
) -> None:
    index_uris = _index_lines(_mirror_list_repository(tmp_path)) + _index_lines(
        other_repository
    )

    result, apt_calls = _run_snapshot_script(
        tmp_path,
        [mode, SNAPSHOT.id],
        index_uris=index_uris,
        mirror_list=_MIRROR_LIST_OF_AMD64,
    )

    assert result.returncode == 1
    assert result.stderr == (
        "apt_snapshot.sh: error: apt reads a source other than "
        f"{_mirror_list_repository(tmp_path)}:\n"
        f"  {other_repository}\n"
    )
    # It stops before a download from that source.
    assert apt_calls == ["update --print-uris"]


def test_snapshot_script_probe_names_each_other_source_once(tmp_path: Path) -> None:
    index_uris = (
        _index_lines("http://archive.ubuntu.com/ubuntu/", suites=("noble", "extra"))
        + _index_lines(_mirror_list_repository(tmp_path))
        # A repository without suites has its indexes at its root.
        + "'https://example.com/flat/./InRelease' flat_InRelease 0 \n"
    )

    result, _ = _run_snapshot_script(
        tmp_path,
        ["probe-snapshot", SNAPSHOT.id],
        index_uris=index_uris,
        mirror_list=_MIRROR_LIST_OF_AMD64,
    )

    assert result.returncode == 1
    assert result.stderr == (
        "apt_snapshot.sh: error: apt reads a source other than "
        f"{_mirror_list_repository(tmp_path)}:\n"
        "  http://archive.ubuntu.com/ubuntu/\n"
        "  https://example.com/flat/./InRelease\n"
    )


@pytest.mark.parametrize("mode", _PROBE_MODES)
def test_snapshot_script_probe_stops_when_apt_has_no_source(
    tmp_path: Path, mode: str
) -> None:
    result, apt_calls = _run_snapshot_script(
        tmp_path, [mode, SNAPSHOT.id], mirror_list=_MIRROR_LIST_OF_AMD64
    )

    assert result.returncode == 1
    assert result.stderr == "apt_snapshot.sh: error: apt has no source\n"
    assert apt_calls == ["update --print-uris"]


@pytest.mark.parametrize("mode", _PROBE_MODES)
@pytest.mark.parametrize(
    "mirror_list",
    [
        # The regular mirror first.
        (
            "http://archive.ubuntu.com/ubuntu/\tpriority:1\n"
            f"{_SNAPSHOT_REPOSITORY}\tpriority:2\n"
        ),
        _mirror_list(OTHER_SNAPSHOT.id, "http://archive.ubuntu.com/ubuntu/"),
        # The regular mirror of arm64 on amd64.
        _mirror_list(SNAPSHOT.id, "http://ports.ubuntu.com/ubuntu-ports/"),
        f"{_SNAPSHOT_REPOSITORY}\tpriority:1\n",
        "",
    ],
    ids=[
        "regular-mirror-first",
        "other-snapshot",
        "mirror-of-other-architecture",
        "no-regular-mirror",
        "empty",
    ],
)
def test_snapshot_script_probe_stops_at_a_mirror_list_in_another_form(
    tmp_path: Path, mode: str, mirror_list: str
) -> None:
    result, apt_calls = _run_snapshot_script(
        tmp_path,
        [mode, SNAPSHOT.id],
        index_uris=_index_lines(_mirror_list_repository(tmp_path)),
        mirror_list=mirror_list,
    )

    list_file = _apt_etc_dir(tmp_path) / "peppy-ubuntu-mirrors.txt"
    assert result.returncode == 1
    assert result.stderr == (
        f"apt_snapshot.sh: error: {list_file} does not name the snapshot "
        f"{SNAPSHOT.id} first and the regular mirror second:\n"
        f"{mirror_list.rstrip()}\n"
    )
    assert apt_calls == ["update --print-uris"]


@pytest.mark.parametrize("mode", _PROBE_MODES)
def test_snapshot_script_probe_stops_without_a_mirror_list(
    tmp_path: Path, mode: str
) -> None:
    result, apt_calls = _run_snapshot_script(
        tmp_path,
        [mode, SNAPSHOT.id],
        index_uris=_index_lines(_mirror_list_repository(tmp_path)),
    )

    list_file = _apt_etc_dir(tmp_path) / "peppy-ubuntu-mirrors.txt"
    assert result.returncode == 1
    assert result.stderr.endswith(
        f"apt_snapshot.sh: error: {list_file} cannot be read\n"
    )
    assert apt_calls == ["update --print-uris"]


@pytest.mark.parametrize("mode", _PROBE_MODES)
def test_snapshot_script_probe_stops_on_an_architecture_without_a_regular_mirror(
    tmp_path: Path, mode: str
) -> None:
    result, apt_calls = _run_snapshot_script(
        tmp_path,
        [mode, SNAPSHOT.id],
        index_uris=_index_lines(_mirror_list_repository(tmp_path)),
        architecture="riscv64",
        mirror_list=_MIRROR_LIST_OF_AMD64,
    )

    assert result.returncode == 1
    assert result.stderr == (
        "apt_snapshot.sh: error: no regular Ubuntu mirror is known for the "
        "architecture riscv64\n"
    )
    assert apt_calls == ["update --print-uris"]


# --- build_and_push_base_image ---


def test_build_and_push_base_image_builds_on_the_snapshot_under_the_tag_alone(
    tmp_path: Path,
) -> None:
    image = BASE_IMAGES[0]

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        build_and_push_base_image(image, "v0.1.0", SNAPSHOT, tmp_path)

    mock_run.assert_called_once()
    cmd = mock_run.call_args[0][0]
    assert cmd[:3] == ["docker", "buildx", "build"]
    assert flag_value(cmd, "--builder") == BUILDX_BUILDER_NAME
    assert flag_value(cmd, "--platform") == ",".join(DOCKER_PLATFORMS)
    assert "--push" in cmd
    assert _flag_values(cmd, "--build-arg") == [f"APT_SNAPSHOT={SNAPSHOT.id}"]
    assert _flag_values(cmd, "--label") == [f"bot.peppy.apt-snapshot={SNAPSHOT.id}"]
    assert flag_value(cmd, "--file") == str(
        tmp_path / image.dockerfile_dir / "Dockerfile"
    )
    # `latest` does not move with the build.
    assert _flag_values(cmd, "-t") == [f"{image.repo}:v0.1.0"]
    # The context is the directory of every image: it holds the shared script.
    assert cmd[-1] == str(tmp_path)


def test_build_and_push_base_image_failure(tmp_path: Path) -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx build failed"):
            build_and_push_base_image(BASE_IMAGES[0], "v0.1.0", SNAPSHOT, tmp_path)


# --- probe_base_image ---


def test_probe_base_image_runs_both_probes_on_each_platform(tmp_path: Path) -> None:
    image = BASE_IMAGES[0]

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        probe_base_image(image, "v0.1.0", SNAPSHOT, tmp_path)

    commands = [run.args[0] for run in mock_run.call_args_list]
    assert [(flag_value(cmd, "--platform"), cmd[-2]) for cmd in commands] == [
        (platform, mode) for platform in DOCKER_PLATFORMS for mode in _PROBE_MODES
    ]
    for cmd in commands:
        assert cmd[:3] == ["docker", "run", "--rm"]
        # The image Docker Hub serves, not a copy this host holds.
        assert flag_value(cmd, "--pull") == "always"
        assert _flag_values(cmd, "--volume") == [
            f"{tmp_path / 'apt_snapshot.sh'}:/run/apt_snapshot.sh:ro"
        ]
        assert cmd[-5:-2] == [f"{image.repo}:v0.1.0", "sh", "/run/apt_snapshot.sh"]
        assert cmd[-1] == SNAPSHOT.id

    unreachable_of_mode = {cmd[-2]: _flag_values(cmd, "--add-host") for cmd in commands}
    assert unreachable_of_mode == {
        # The regular mirror out of reach: the snapshot serves every file.
        "probe-snapshot": [
            "archive.ubuntu.com:127.0.0.1",
            "security.ubuntu.com:127.0.0.1",
            "ports.ubuntu.com:127.0.0.1",
        ],
        # The snapshot out of reach: the regular mirror serves every file.
        "probe-fallback": ["snapshot.ubuntu.com:127.0.0.1"],
    }


def test_probe_base_image_stops_at_the_first_probe_that_fails(tmp_path: Path) -> None:
    image = BASE_IMAGES[0]

    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError) as raised:
            probe_base_image(image, "v0.1.0", SNAPSHOT, tmp_path)

    assert str(raised.value) == (
        f"{image.repo}:v0.1.0 on {DOCKER_PLATFORMS[0]} does not install from "
        f"the Ubuntu snapshot {SNAPSHOT.id} alone (exit 1)"
    )
    mock_run.assert_called_once()


def test_probe_base_image_reports_a_failed_fallback(tmp_path: Path) -> None:
    image = BASE_IMAGES[0]

    def run(cmd: Sequence[str], **_: object) -> MagicMock:
        return MagicMock(returncode=0 if cmd[-2] == "probe-snapshot" else 100)

    with (
        patch("functions.docker.subprocess.run", side_effect=run) as mock_run,
        pytest.raises(ReleaseError) as raised,
    ):
        probe_base_image(image, "v0.1.0", SNAPSHOT, tmp_path)

    assert str(raised.value) == (
        f"{image.repo}:v0.1.0 on {DOCKER_PLATFORMS[0]} does not install from "
        "the regular Ubuntu mirror when the snapshot service does not answer "
        "(exit 100)"
    )
    assert mock_run.call_count == 2


# --- build_all_base_images ---


def _is_build(cmd: Sequence[str]) -> bool:
    return list(cmd[:3]) == ["docker", "buildx", "build"]


def _is_probe(cmd: Sequence[str]) -> bool:
    return list(cmd[:2]) == ["docker", "run"]


def test_build_all_base_images_builds_every_image_on_one_snapshot_then_probes(
    tmp_path: Path,
) -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=0)
        build_all_base_images(tmp_path, "v0.2.0", SNAPSHOT)

    commands = [run.args[0] for run in mock_run.call_args_list]
    builds = commands[: len(BASE_IMAGES)]
    probes = commands[len(BASE_IMAGES) :]

    assert all(_is_build(cmd) for cmd in builds)
    assert [flag_value(cmd, "-t") for cmd in builds] == [
        f"{image.repo}:v0.2.0" for image in BASE_IMAGES
    ]
    assert {flag_value(cmd, "--build-arg") for cmd in builds} == {
        f"APT_SNAPSHOT={SNAPSHOT.id}"
    }
    assert {cmd[-1] for cmd in builds} == {str(tmp_path / "base_images")}

    # Every image is probed on every platform with every probe, once all of
    # them are pushed.
    assert all(_is_probe(cmd) for cmd in probes)
    assert [(cmd[-5], flag_value(cmd, "--platform"), cmd[-2]) for cmd in probes] == [
        (f"{image.repo}:v0.2.0", platform, mode)
        for image in BASE_IMAGES
        for platform in DOCKER_PLATFORMS
        for mode in _PROBE_MODES
    ]
    assert {cmd[-1] for cmd in probes} == {SNAPSHOT.id}

    assert not any("latest" in arg for cmd in commands for arg in cmd)


def test_build_all_base_images_stops_on_first_failure(tmp_path: Path) -> None:
    with patch("functions.docker.subprocess.run") as mock_run:
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError, match="docker buildx build failed"):
            build_all_base_images(tmp_path, "v0.3.0", SNAPSHOT)

    assert mock_run.call_count == 1


def test_build_all_base_images_stops_on_a_failed_probe(tmp_path: Path) -> None:
    def run(cmd: Sequence[str], **_: object) -> MagicMock:
        return MagicMock(returncode=0 if _is_build(cmd) else 1)

    with (
        patch("functions.docker.subprocess.run", side_effect=run) as mock_run,
        pytest.raises(ReleaseError, match="does not install from the Ubuntu snapshot"),
    ):
        build_all_base_images(tmp_path, "v0.3.0", SNAPSHOT)

    # Every build, then the one probe that failed.
    assert mock_run.call_count == len(BASE_IMAGES) + 1


# --- read_apt_snapshot ---


def _labels(snapshot_id: object) -> dict[str, object]:
    return {
        "org.opencontainers.image.version": "24.04",
        "bot.peppy.apt-snapshot": snapshot_id,
    }


def _inspected(labels_of_platform: Mapping[str, object]) -> MagicMock:
    """An `imagetools inspect` that prints an image for each platform, with
    the given value as its labels."""
    images = {
        platform: {"architecture": platform.split("/")[1], "config": {"Labels": labels}}
        for platform, labels in labels_of_platform.items()
    }
    return MagicMock(returncode=0, stdout=json.dumps(images), stderr="")


def test_read_apt_snapshot_reads_the_label_of_every_platform() -> None:
    image = BASE_IMAGES[0]
    answer = _inspected(dict.fromkeys(DOCKER_PLATFORMS, _labels(SNAPSHOT.id)))

    with patch("functions.docker.subprocess.run", return_value=answer) as mock_run:
        assert read_apt_snapshot(image, "v0.1.0") == SNAPSHOT

    mock_run.assert_called_once()
    assert mock_run.call_args[0][0] == [
        "docker",
        "buildx",
        "imagetools",
        "inspect",
        f"{image.repo}:v0.1.0",
        "--format",
        "{{json .Image}}",
    ]


def test_read_apt_snapshot_of_a_tag_docker_hub_does_not_have() -> None:
    image = BASE_IMAGES[0]
    answer = MagicMock(returncode=1, stdout="", stderr="ERROR: v9: not found\n")

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError) as raised,
    ):
        read_apt_snapshot(image, "v9")

    assert str(raised.value) == f"cannot read {image.repo}:v9: ERROR: v9: not found"


@pytest.mark.parametrize(
    ("stdout", "message"),
    [
        ("not json", "cannot read the image configuration of"),
        ("null", "is not a multi-platform image"),
        ('["linux/amd64"]', "is not a multi-platform image"),
        # The image of one platform: its configuration, not keyed by platform.
        (
            json.dumps({"architecture": "amd64", "config": {"Labels": {}}}),
            "has no linux/amd64",
        ),
    ],
    ids=["not-json", "null", "list", "one-platform"],
)
def test_read_apt_snapshot_of_an_answer_in_another_form(
    stdout: str, message: str
) -> None:
    answer = MagicMock(returncode=0, stdout=stdout, stderr="")

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError, match=message),
    ):
        read_apt_snapshot(BASE_IMAGES[0], "v0.1.0")


def test_read_apt_snapshot_of_a_tag_that_lacks_a_platform() -> None:
    image = BASE_IMAGES[0]
    answer = _inspected({"linux/amd64": _labels(SNAPSHOT.id)})

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError) as raised,
    ):
        read_apt_snapshot(image, "v0.1.0")

    assert str(raised.value) == f"{image.repo}:v0.1.0 has no linux/arm64"


@pytest.mark.parametrize(
    "labels",
    [None, {}, {"org.opencontainers.image.version": "24.04"}, _labels(None)],
    ids=["null-labels", "no-labels", "other-labels", "null-snapshot"],
)
def test_read_apt_snapshot_of_an_image_without_the_label(labels: object) -> None:
    image = BASE_IMAGES[0]
    answer = _inspected(
        {"linux/amd64": _labels(SNAPSHOT.id), "linux/arm64": labels},
    )

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError) as raised,
    ):
        read_apt_snapshot(image, "v0.1.0")

    assert str(raised.value) == (
        f"{image.repo}:v0.1.0 on linux/arm64 has no bot.peppy.apt-snapshot "
        "label: it does not read an Ubuntu snapshot"
    )


def test_read_apt_snapshot_of_an_image_without_a_configuration() -> None:
    images = {platform: {"config": None} for platform in DOCKER_PLATFORMS}
    answer = MagicMock(returncode=0, stdout=json.dumps(images), stderr="")

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError, match="has no bot.peppy.apt-snapshot label"),
    ):
        read_apt_snapshot(BASE_IMAGES[0], "v0.1.0")


def test_read_apt_snapshot_of_a_label_that_is_not_a_snapshot_id() -> None:
    answer = _inspected(dict.fromkeys(DOCKER_PLATFORMS, _labels("yesterday")))

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError, match="'yesterday' is not an Ubuntu snapshot id"),
    ):
        read_apt_snapshot(BASE_IMAGES[0], "v0.1.0")


def test_read_apt_snapshot_of_platforms_on_different_snapshots() -> None:
    image = BASE_IMAGES[0]
    answer = _inspected(
        {
            "linux/amd64": _labels(OTHER_SNAPSHOT.id),
            "linux/arm64": _labels(SNAPSHOT.id),
        }
    )

    with (
        patch("functions.docker.subprocess.run", return_value=answer),
        pytest.raises(ReleaseError) as raised,
    ):
        read_apt_snapshot(image, "v0.1.0")

    assert str(raised.value) == (
        f"the platforms of {image.repo}:v0.1.0 read different Ubuntu snapshots: "
        f"{SNAPSHOT.id}, {OTHER_SNAPSHOT.id}"
    )


# --- move_latest_to ---


def test_move_latest_to_moves_latest_of_every_image() -> None:
    with (
        patch("functions.docker.read_apt_snapshot", return_value=SNAPSHOT) as mock_read,
        patch("functions.docker.subprocess.run") as mock_run,
    ):
        mock_run.return_value = MagicMock(returncode=0)
        move_latest_to("v0.4.0")

    assert [read.args for read in mock_read.call_args_list] == [
        (image, "v0.4.0") for image in BASE_IMAGES
    ]
    assert [run.args[0] for run in mock_run.call_args_list] == [
        [
            "docker",
            "buildx",
            "imagetools",
            "create",
            "--tag",
            f"{image.repo}:latest",
            f"{image.repo}:v0.4.0",
        ]
        for image in BASE_IMAGES
    ]


def test_move_latest_to_moves_nothing_when_the_images_read_different_snapshots() -> (
    None
):
    snapshots = [SNAPSHOT, *[OTHER_SNAPSHOT] * (len(BASE_IMAGES) - 1)]

    with (
        patch("functions.docker.read_apt_snapshot", side_effect=snapshots),
        patch("functions.docker.subprocess.run") as mock_run,
        pytest.raises(ReleaseError) as raised,
    ):
        move_latest_to("v0.4.0")

    message = str(raised.value)
    assert "the base images under v0.4.0 read different Ubuntu snapshots" in message
    assert f"{BASE_IMAGES[0].repo}:v0.4.0 reads {SNAPSHOT.id}" in message
    assert f"{BASE_IMAGES[-1].repo}:v0.4.0 reads {OTHER_SNAPSHOT.id}" in message
    mock_run.assert_not_called()


def test_move_latest_to_moves_nothing_when_an_image_cannot_be_read() -> None:
    # The last image has no such tag: `latest` of the first one stays too.
    answers = [*[SNAPSHOT] * (len(BASE_IMAGES) - 1), ReleaseError("cannot read")]

    with (
        patch("functions.docker.read_apt_snapshot", side_effect=answers),
        patch("functions.docker.subprocess.run") as mock_run,
        pytest.raises(ReleaseError, match="cannot read"),
    ):
        move_latest_to("v0.4.0")

    mock_run.assert_not_called()


def test_move_latest_to_reports_a_move_that_fails() -> None:
    image = BASE_IMAGES[0]

    with (
        patch("functions.docker.read_apt_snapshot", return_value=SNAPSHOT),
        patch("functions.docker.subprocess.run") as mock_run,
    ):
        mock_run.return_value = MagicMock(returncode=1)
        with pytest.raises(ReleaseError) as raised:
            move_latest_to("v0.4.0")

    assert str(raised.value) == (
        f"cannot move {image.repo}:latest to v0.4.0 (exit 1). Run this again: "
        "`latest` of the base images must name one build."
    )
    mock_run.assert_called_once()


# --- the tag ---


def test_require_tag_prompts_when_no_tag_is_given() -> None:
    with patch("functions.docker.prompt", return_value="v1.0.0") as mock_prompt:
        assert _require_tag(None) == "v1.0.0"
    mock_prompt.assert_called_once()


@pytest.mark.usefixtures("no_prompts")
def test_require_tag_takes_the_tag_it_is_given() -> None:
    assert _require_tag("v1.0.0") == "v1.0.0"


def test_require_tag_refuses_an_empty_answer() -> None:
    with (
        patch("functions.docker.prompt", return_value=""),
        pytest.raises(ReleaseError, match="tag cannot be empty"),
    ):
        _require_tag(None)


@pytest.mark.usefixtures("no_prompts")
@pytest.mark.parametrize(
    ("tag", "message"),
    [("", "tag cannot be empty"), ("latest", "'latest' is not a tag of its own")],
)
def test_require_tag_refuses_a_tag_that_names_no_build(tag: str, message: str) -> None:
    with pytest.raises(ReleaseError, match=message):
        _require_tag(tag)


# --- the entry points ---


def test_utc_now_is_a_time_in_utc() -> None:
    assert _utc_now().utcoffset() == timedelta(0)


@pytest.mark.usefixtures("no_prompts")
def test_run_build_gives_one_snapshot_to_every_image_and_platform() -> None:
    # 09:38:33 at UTC+1 is 08:38:33 UTC, and the snapshot is of one hour
    # before: the service can still change the snapshot of a later time.
    start = datetime(2026, 11, 10, 9, 38, 33, tzinfo=timezone(timedelta(hours=1)))

    with (
        patch("functions.docker.validate_docker_environment") as mock_validate,
        patch("functions.docker._utc_now", return_value=start) as mock_now,
        patch("functions.docker.subprocess.run") as mock_run,
    ):
        mock_run.return_value = MagicMock(returncode=0)
        _run_build("v1.0.0")

    mock_validate.assert_called_once_with()
    # One time for the whole build.
    mock_now.assert_called_once_with()

    commands = [run.args[0] for run in mock_run.call_args_list]
    builds = [cmd for cmd in commands if _is_build(cmd)]
    probes = [cmd for cmd in commands if _is_probe(cmd)]
    assert len(builds) == len(BASE_IMAGES)
    assert len(probes) == len(BASE_IMAGES) * len(DOCKER_PLATFORMS) * len(APT_PROBES)
    # One build of an image covers every platform.
    assert {flag_value(cmd, "--platform") for cmd in builds} == {
        ",".join(DOCKER_PLATFORMS)
    }
    assert {flag_value(cmd, "--build-arg") for cmd in builds} == {
        "APT_SNAPSHOT=20261110T073833Z"
    }
    assert {flag_value(cmd, "--label") for cmd in builds} == {
        "bot.peppy.apt-snapshot=20261110T073833Z"
    }
    assert {cmd[-1] for cmd in probes} == {"20261110T073833Z"}


def test_run_build_refuses_a_tag_before_it_builds() -> None:
    with (
        patch("functions.docker.validate_docker_environment"),
        patch("functions.docker.build_all_base_images") as mock_build,
        pytest.raises(ReleaseError, match="'latest' is not a tag of its own"),
    ):
        _run_build("latest")

    mock_build.assert_not_called()


def test_run_build_prompts_for_the_tag() -> None:
    with (
        patch("functions.docker.validate_docker_environment"),
        patch("functions.docker.prompt", return_value="v1.0.0"),
        patch(
            "functions.docker._utc_now", return_value=datetime(2026, 1, 2, tzinfo=UTC)
        ),
        patch("functions.docker.build_all_base_images") as mock_build,
    ):
        _run_build(None)

    mock_build.assert_called_once()
    _, tag, snapshot = mock_build.call_args[0]
    assert tag == "v1.0.0"
    assert snapshot == AptSnapshot("20260101T230000Z")


@pytest.mark.usefixtures("no_prompts")
def test_run_move_latest_moves_latest_to_the_tag() -> None:
    with (
        patch("functions.docker.validate_docker_environment") as mock_validate,
        patch("functions.docker.move_latest_to") as mock_move,
    ):
        _run_move_latest("v1.0.0")

    mock_validate.assert_called_once_with()
    mock_move.assert_called_once_with("v1.0.0")


def test_run_move_latest_refuses_latest_itself() -> None:
    with (
        patch("functions.docker.validate_docker_environment"),
        patch("functions.docker.move_latest_to") as mock_move,
        pytest.raises(ReleaseError, match="'latest' is not a tag of its own"),
    ):
        _run_move_latest("latest")

    mock_move.assert_not_called()


@pytest.mark.parametrize("tag", [None, "v1.0.0"])
def test_main_builds_with_the_tag_it_is_given(tag: str | None) -> None:
    with patch("functions.docker._run_build") as mock_run_build:
        main(tag)
    mock_run_build.assert_called_once_with(tag)


@pytest.mark.parametrize("tag", [None, "v1.0.0"])
def test_move_latest_main_moves_to_the_tag_it_is_given(tag: str | None) -> None:
    with patch("functions.docker._run_move_latest") as mock_run_move_latest:
        move_latest_main(tag)
    mock_run_move_latest.assert_called_once_with(tag)


def test_main_exits_with_1_on_a_release_error() -> None:
    with (
        patch("functions.docker._run_build", side_effect=ReleaseError("no")),
        pytest.raises(SystemExit) as exited,
    ):
        main("v1.0.0")
    assert exited.value.code == 1
