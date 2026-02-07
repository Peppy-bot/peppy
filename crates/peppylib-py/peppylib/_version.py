try:
    from ._peppylib import __version__  # type: ignore[import-not-found]
except ImportError:
    __version__ = "0.0.1"