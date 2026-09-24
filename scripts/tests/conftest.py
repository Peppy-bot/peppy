"""Shared fixtures for release script tests."""

from __future__ import annotations

from typing import Any

import httpx
import pytest
import respx

from .install_helpers import DISPOSABLE_HOST_VARIABLE, is_disposable_host


def pytest_configure(config: pytest.Config) -> None:
    """Register the suite's custom markers."""
    config.addinivalue_line(
        "markers",
        "install: marks tests that run install.sh on this machine (minutes each, "
        "vs milliseconds for the mocked suite)",
    )


def pytest_collection_modifyitems(
    config: pytest.Config, items: list[pytest.Item]
) -> None:
    """Skip the install tests on a machine not declared disposable.

    The install tests change the machine they run on and install the release
    archive of its platform, so they run only where the environment says the
    machine is discarded after the run. Everywhere else, a developer's laptop
    included, they skip.
    """
    if is_disposable_host():
        return
    skip_install = pytest.mark.skip(
        reason=f"install.sh tests change this machine; they run only where "
        f"{DISPOSABLE_HOST_VARIABLE}=1 declares it disposable"
    )
    for item in items:
        if item.get_closest_marker("install"):
            item.add_marker(skip_install)


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
