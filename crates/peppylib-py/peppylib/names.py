"""Re-export names module from native extension."""

from ._peppylib.names import generate_name

__all__ = ["generate_name"]
