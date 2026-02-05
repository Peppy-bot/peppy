"""
peppylib - The peppyOS control library
"""

import sys
from ._version import __version__

# Import the native module and register submodules in sys.modules
# This is required for PyO3 submodules to be importable with dot notation
from . import _peppylib

sys.modules["peppylib._peppylib.messaging"] = _peppylib.messaging
sys.modules["peppylib._peppylib.config"] = _peppylib.config
sys.modules["peppylib._peppylib.names"] = _peppylib.names

# Re-export the Rust-implemented functions/types from the native module
from ._peppylib import sum_as_string
from ._peppylib.messaging import MessengerHandle, TopicMessenger
from ._peppylib.config import QoSProfile

__all__ = ["sum_as_string", "MessengerHandle", "TopicMessenger", "QoSProfile", "__version__"]
