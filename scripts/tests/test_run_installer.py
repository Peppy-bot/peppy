"""Tests for scripts/run_installer.sh.

Fakes of curl and sleep stand in for the network and the clock: the fake curl
answers each attempt as the test scripts it, and the fake sleep records the
waits it was asked for and returns at once.
"""

from __future__ import annotations

import os
import subprocess
from pathlib import Path

import pytest

RUN_INSTALLER = Path(__file__).resolve().parent.parent / "run_installer.sh"
INSTALLER_URL = "https://installer.example/install.sh"

# The installer the fake curl hands out when an attempt succeeds: it records
# the arguments and the PIXI_VERSION it got.
WORKING_INSTALLER = 'printf "%s %s\\n" "$PIXI_VERSION" "$*" > "$FAKE_DIR/installed"'

FAKE_CURL = """#!/usr/bin/env bash
# One line of $FAKE_DIR/answers per attempt: `fetch-fails` is a failed fetch
# of the installer, `install-fails` an installer whose own download fails, and
# `works` the working installer.
count=$(( $(cat "$FAKE_DIR/count" 2>/dev/null || echo 0) + 1 ))
echo "$count" > "$FAKE_DIR/count"
printf '%s\\n' "$*" >> "$FAKE_DIR/curl-arguments"
case "$(sed -n "${count}p" "$FAKE_DIR/answers")" in
fetch-fails) exit 22 ;;
install-fails) echo 'exit 1' ;;
*) cat "$FAKE_DIR/working-installer" ;;
esac
"""

FAKE_SLEEP = """#!/usr/bin/env bash
printf '%s\\n' "$1" >> "$FAKE_DIR/waits"
"""


def _write_executable(path: Path, content: str) -> None:
    path.write_text(content)
    path.chmod(0o755)


def _run_installer(
    tmp_path: Path, answers: list[str], *arguments: str
) -> subprocess.CompletedProcess[str]:
    """Run run_installer.sh with the fake curl answering *answers* in turn."""
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    _write_executable(fake_bin / "curl", FAKE_CURL)
    _write_executable(fake_bin / "sleep", FAKE_SLEEP)
    (tmp_path / "answers").write_text("".join(f"{answer}\n" for answer in answers))
    (tmp_path / "working-installer").write_text(WORKING_INSTALLER)
    env = {
        **os.environ,
        "PATH": f"{fake_bin}{os.pathsep}{os.environ['PATH']}",
        "FAKE_DIR": str(tmp_path),
        "PIXI_VERSION": "v0.80.0",
    }
    return subprocess.run(
        [str(RUN_INSTALLER), *arguments],
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )


def _lines(path: Path) -> list[str]:
    return path.read_text().splitlines() if path.exists() else []


def test_a_working_installer_runs_once(tmp_path: Path) -> None:
    result = _run_installer(tmp_path, ["works"], INSTALLER_URL, "--yes")

    assert result.returncode == 0, result.stderr
    assert _lines(tmp_path / "installed") == ["v0.80.0 --yes"]
    assert _lines(tmp_path / "curl-arguments") == [f"-fsSL {INSTALLER_URL}"]
    assert _lines(tmp_path / "waits") == []


@pytest.mark.parametrize("failure", ["fetch-fails", "install-fails"])
def test_a_failed_attempt_is_run_again_after_a_longer_wait(
    tmp_path: Path, failure: str
) -> None:
    result = _run_installer(tmp_path, [failure, failure, "works"], INSTALLER_URL)

    assert result.returncode == 0, result.stderr
    assert _lines(tmp_path / "installed") == ["v0.80.0 "]
    assert _lines(tmp_path / "waits") == ["30", "60"]
    assert "attempt 2 of 4" in result.stderr


def test_the_install_fails_once_the_attempts_are_spent(tmp_path: Path) -> None:
    result = _run_installer(tmp_path, ["install-fails"] * 5, INSTALLER_URL)

    assert result.returncode == 1
    assert len(_lines(tmp_path / "curl-arguments")) == 4
    assert _lines(tmp_path / "waits") == ["30", "60", "90"]
    assert f"the installer at {INSTALLER_URL} failed 4 times" in result.stderr


def test_an_installer_url_is_required(tmp_path: Path) -> None:
    result = _run_installer(tmp_path, [])

    assert result.returncode == 2
    assert "usage:" in result.stderr
    assert _lines(tmp_path / "curl-arguments") == []
