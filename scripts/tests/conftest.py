"""Shared fixtures for release script tests."""

from __future__ import annotations

import subprocess
from pathlib import Path
from typing import Any

import httpx
import pytest
import respx


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
