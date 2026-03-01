"""
peppylib - The peppyOS control library
"""

import os
import sys

# When deployed, the native extension lives in a platform-specific subpackage
# (e.g. macos_aarch64/_peppylib.abi3.so). Pre-load it into sys.modules so that
# subsequent `from . import _peppylib` and `from ._peppylib import ...` resolve.
# In local dev mode (maturin develop), the subdir doesn't exist and the standard
# flat import on line 24 handles it instead.
import platform as _platform

_os_tag = {"Darwin": "macos", "Linux": "linux"}.get(_platform.system())
if _os_tag:
    _arch = _platform.machine().replace("arm64", "aarch64")
    _platform_pkg = f"{_os_tag}_{_arch}"
    if os.path.isdir(os.path.join(os.path.dirname(__file__), _platform_pkg)):
        import importlib

        _peppylib = importlib.import_module(
            f".{_platform_pkg}._peppylib", package=__package__
        )
        sys.modules[f"{__package__}._peppylib"] = _peppylib

from ._version import __version__
from . import encoding

# Force line-buffered stdout/stderr when not connected to a TTY (e.g., when
# spawned by the daemon with piped I/O). Without this, Python defaults to full
# buffering, delaying log capture in .peppy/logs/start/.
if hasattr(sys.stdout, "reconfigure") and not sys.stdout.isatty():
    sys.stdout.reconfigure(line_buffering=True)
if hasattr(sys.stderr, "reconfigure") and not sys.stderr.isatty():
    sys.stderr.reconfigure(line_buffering=True)

# Import the native module and register submodules in sys.modules
# This is required for PyO3 submodules to be importable with dot notation
from . import _peppylib  # type: ignore[import-not-found]

# Public module aliases
sys.modules["peppylib.messaging"] = _peppylib.messaging
sys.modules["peppylib.messaging.services"] = _peppylib.messaging.services
sys.modules["peppylib.messaging.actions"] = _peppylib.messaging.actions

# Internal/native module aliases
sys.modules["peppylib._peppylib.messaging"] = _peppylib.messaging
sys.modules["peppylib._peppylib.config"] = _peppylib.config
sys.modules["peppylib._peppylib.names"] = _peppylib.names
sys.modules["peppylib._peppylib.runtime"] = _peppylib.runtime
sys.modules["peppylib._peppylib.messaging.services"] = _peppylib.messaging.services
sys.modules["peppylib._peppylib.messaging.actions"] = _peppylib.messaging.actions
sys.modules["peppylib._peppylib.services"] = _peppylib.services

# Expose as attribute for `from peppylib import messaging`
messaging = _peppylib.messaging

# Re-export the Rust-implemented functions/types from the native module
from ._peppylib.messaging import MessengerHandle, TopicMessenger, ZenohdInstance  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.config import QoSProfile  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.messaging.services import ServiceMessenger  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.messaging.actions import ActionMessenger  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.runtime import (  # noqa: E402  # type: ignore[import-not-found]
    NodeBuilder,
    StandaloneConfig,
    NodeRunner,
    CancellationToken,
)

__all__ = [
    "MessengerHandle",
    "TopicMessenger",
    "ZenohdInstance",
    "QoSProfile",
    "ServiceMessenger",
    "ActionMessenger",
    "NodeBuilder",
    "StandaloneConfig",
    "NodeRunner",
    "CancellationToken",
    "messaging",
    "encoding",
    "__version__",
]
