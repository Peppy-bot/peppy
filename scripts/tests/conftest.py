"""Shared fixtures for release script tests."""

from __future__ import annotations

import shutil
import subprocess
from pathlib import Path
from typing import Any

import httpx
import pytest
import respx

from .lima_helpers import build_release_archives, get_native_linux_targets


def pytest_configure(config: pytest.Config) -> None:
    """Register custom markers and ensure Lima cross-arch guest agents are available."""
    config.addinivalue_line(
        "markers",
        "cross_arch: marks tests as cross-architecture (may be slow under QEMU emulation)",
    )
    _ensure_lima_guest_agents()


def _ensure_lima_guest_agents() -> None:
    """Copy Lima additional guest agent binaries into the pixi Lima share dir.

    Lima requires architecture-specific guest agent binaries for cross-arch
    VMs (e.g. x86_64 on aarch64).  The main ``lima`` package only ships the
    native agent; additional agents come from ``lima-additional-guestagents``
    (installed via Homebrew).  This function copies any missing agents from
    the Homebrew installation into pixi's Lima share directory.
    """
    limactl = shutil.which("limactl")
    if not limactl:
        return

    pixi_share = Path(limactl).resolve().parent.parent / "share" / "lima"
    if not pixi_share.is_dir():
        return

    brew_share = Path("/opt/homebrew/share/lima")
    if not brew_share.is_dir():
        return

    for agent in brew_share.glob("lima-guestagent.*"):
        dest = pixi_share / agent.name
        if not dest.exists():
            shutil.copy2(agent, dest)


@pytest.fixture(scope="session", autouse=True)
def _build_release_archives(request: pytest.FixtureRequest) -> None:
    """Build release archives needed by the collected tests.

    Only builds cross-arch targets when ``test_install`` is collected
    (the only module with cross-arch VM configs).  This avoids spawning
    a slow QEMU x86_64 VM when running container tests alone.
    """
    needs_cross_arch = any(
        item.module.__name__.endswith(".test_install")
        for item in request.session.items
    )
    if needs_cross_arch:
        build_release_archives()
    else:
        build_release_archives(targets=get_native_linux_targets())


@pytest.fixture()
def mock_api() -> respx.MockRouter:
    """A respx mock router for httpx requests."""
    with respx.mock(assert_all_called=False) as router:
        yield router


@pytest.fixture()
def github_client(mock_api: respx.MockRouter) -> httpx.Client:
    """An httpx.Client that routes through the respx mock."""
    return httpx.Client(
        headers={
            "Authorization": "Bearer test-token",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        follow_redirects=True,
        timeout=10.0,
    )


@pytest.fixture()
def fake_release_response() -> dict[str, Any]:
    """A dict matching a GitHub release API response."""
    return {
        "id": 12345,
        "html_url": "https://github.com/test-owner/test-repo/releases/tag/v0.1.0",
        "tag_name": "v0.1.0",
        "name": "v0.1.0",
        "body": "Release notes content",
        "published_at": "2025-06-15T10:00:00Z",
        "created_at": "2025-06-15T09:00:00Z",
        "assets": [],
    }


@pytest.fixture()
def tmp_repo(tmp_path: Path) -> Path:
    """A temporary directory with a git repo initialized and a fake remote."""
    subprocess.run(["git", "init"], cwd=tmp_path, capture_output=True, check=True)
    subprocess.run(
        ["git", "remote", "add", "origin", "git@github.com:test-owner/test-repo.git"],
        cwd=tmp_path,
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "config", "user.email", "test@test.com"],
        cwd=tmp_path,
        capture_output=True,
        check=True,
    )
    subprocess.run(
        ["git", "config", "user.name", "Test User"],
        cwd=tmp_path,
        capture_output=True,
        check=True,
    )
    # Create initial commit so HEAD exists
    (tmp_path / "README.md").write_text("test")
    subprocess.run(["git", "add", "."], cwd=tmp_path, capture_output=True, check=True)
    subprocess.run(
        ["git", "commit", "-m", "initial"],
        cwd=tmp_path,
        capture_output=True,
        check=True,
    )
    return tmp_path
