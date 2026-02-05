"""
peppylib - The peppyOS control library
"""

from ._version import __version__

# Re-export the Rust-implemented functions/types from the native module
from ._peppylib import sum_as_string
from ._peppylib.messaging import MessengerHandle

__all__ = ["sum_as_string", "MessengerHandle", "__version__"]
