"""Re-export config module from native extension."""

from ._peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile  # type: ignore[import-not-found]

__all__ = ["DEFAULT_MESSAGING_PORT", "QoSProfile"]
