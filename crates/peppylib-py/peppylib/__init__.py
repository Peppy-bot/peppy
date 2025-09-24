"""
peppycl - The peppyOS control library
"""

from ._version import __version__

# Re-export the Rust-implemented functions/types from the native module
from ._peppycl import sum_as_string

__all__ = ["sum_as_string", "__version__"]
