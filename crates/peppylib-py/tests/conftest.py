"""
Pytest configuration and shared fixtures for peppylib tests.
"""

import subprocess
import sys
from pathlib import Path

import pytest

CRATE_DIR = Path(__file__).resolve().parent.parent

# Generated Parameters dataclass matching the test PEPPY_CONFIG (frequency_hz: f64).
_PARAMETERS_PY = '''\
from dataclasses import dataclass


@dataclass
class Parameters:
    frequency_hz: float

    @classmethod
    def from_dict(cls, data: dict) -> "Parameters":
        return cls(
            frequency_hz=data["frequency_hz"],
        )
'''


@pytest.fixture(scope="session", autouse=True)
def _build_native_extension():
    """Rebuild the native extension before running tests."""
    subprocess.check_call(["maturin", "develop"], cwd=CRATE_DIR)


@pytest.fixture(scope="session", autouse=True)
def _peppygen_on_path(tmp_path_factory):
    """Make a peppygen.parameters module importable for the test session.

    The runtime hydrates the parameters dict into the generated Parameters
    dataclass via ``peppygen.parameters.Parameters.from_dict``, so every test
    that calls ``NodeBuilder().run(setup_fn)`` needs this module on sys.path.
    """
    root = tmp_path_factory.mktemp("peppygen_root")
    peppygen_dir = root / "peppygen"
    peppygen_dir.mkdir()
    (peppygen_dir / "__init__.py").write_text("")
    (peppygen_dir / "parameters.py").write_text(_PARAMETERS_PY)
    sys.path.insert(0, str(root))
    yield
    sys.path.remove(str(root))
    for key in [k for k in sys.modules if k == "peppygen" or k.startswith("peppygen.")]:
        del sys.modules[key]


@pytest.fixture
def default_host() -> str:
    """Default host for messenger connections."""
    return "127.0.0.1"


@pytest.fixture
def default_port() -> int:
    """Default port for messenger connections."""
    from peppylib.config import DEFAULT_MESSAGING_PORT

    return DEFAULT_MESSAGING_PORT
