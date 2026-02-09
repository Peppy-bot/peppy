"""
Pytest configuration and shared fixtures for peppylib tests.
"""

import subprocess
from pathlib import Path

import pytest

CRATE_DIR = Path(__file__).resolve().parent.parent


@pytest.fixture(scope="session", autouse=True)
def _build_native_extension():
    """Rebuild the native extension before running tests."""
    subprocess.check_call(["maturin", "develop"], cwd=CRATE_DIR)


@pytest.fixture
def default_host() -> str:
    """Default host for messenger connections."""
    return "127.0.0.1"


@pytest.fixture
def default_port() -> int:
    """Default port for messenger connections."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    return DEFAULT_MESSAGING_PORT
