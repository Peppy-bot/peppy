"""
peppylib - The peppyOS control library
"""

import sys
from ._version import __version__

# Import the native module and register submodules in sys.modules
# This is required for PyO3 submodules to be importable with dot notation
from . import _peppylib  # type: ignore[import-not-found]

sys.modules["peppylib._peppylib.messaging"] = _peppylib.messaging
sys.modules["peppylib._peppylib.config"] = _peppylib.config
sys.modules["peppylib._peppylib.names"] = _peppylib.names
sys.modules["peppylib._peppylib.runtime"] = _peppylib.runtime
sys.modules["peppylib._peppylib.services"] = _peppylib.services

# Re-export the Rust-implemented functions/types from the native module
from ._peppylib import sum_as_string  # type: ignore[import-not-found]
from ._peppylib.messaging import MessengerHandle, TopicMessenger, ZenohdInstance  # type: ignore[import-not-found]
from ._peppylib.config import QoSProfile  # type: ignore[import-not-found]
from ._peppylib.services import ServiceMessenger  # type: ignore[import-not-found]
from ._peppylib.runtime import NodeBuilder, StandaloneConfig, NodeRunner, CancellationToken  # type: ignore[import-not-found]

__all__ = [
    "sum_as_string",
    "MessengerHandle",
    "TopicMessenger",
    "ZenohdInstance",
    "QoSProfile",
    "ServiceMessenger",
    "NodeBuilder",
    "StandaloneConfig",
    "NodeRunner",
    "CancellationToken",
    "__version__",
]
