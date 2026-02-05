"""
Pytest configuration and shared fixtures for peppylib tests.
"""

import pytest


@pytest.fixture
def default_host() -> str:
    """Default host for messenger connections."""
    return "127.0.0.1"


@pytest.fixture
def default_port() -> int:
    """Default port for messenger connections."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    return DEFAULT_MESSAGING_PORT
