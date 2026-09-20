"""Re-export names module from native extension."""

from ._peppylib.names import generate_name  # type: ignore[import-not-found]

__all__ = ["generate_name"]
