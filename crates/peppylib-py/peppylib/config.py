"""Re-export config module from native extension."""

from ._peppylib.config import DEFAULT_MESSAGING_PORT, QoSProfile

__all__ = ["DEFAULT_MESSAGING_PORT", "QoSProfile"]
