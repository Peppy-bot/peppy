"""
peppylib - The peppyOS control library
"""

import sys
from ._version import __version__
from . import encoding

# Force line-buffered stdout/stderr when not connected to a TTY (e.g., when
# spawned by the daemon with piped I/O). Without this, Python defaults to full
# buffering, delaying log capture in .peppy/logs/run/.
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
sys.modules["peppylib._peppylib.core_node"] = _peppylib.core_node

# Expose as attribute for `from peppylib import messaging`
messaging = _peppylib.messaging

# Re-export the Rust-implemented functions/types from the native module
from ._peppylib.messaging import SenderTarget, ProducerRef, MessengerHandle, TopicMessenger, TopicPublisher, ZenohdInstance  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.config import QoSProfile  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.messaging.services import ServiceMessenger  # noqa: E402  # type: ignore[import-not-found]
from ._peppylib.messaging.actions import (  # noqa: E402  # type: ignore[import-not-found]
    ActionMessenger,
    ConcurrentAction,
    PendingGoal,
    GoalContext,
)
from ._peppylib.runtime import (  # noqa: E402  # type: ignore[import-not-found]
    NodeBuilder,
    StandaloneConfig,
    NodeRunner,
    CancellationToken,
)
# `info` is the one core-node helper that stays flat (a single verb-less call),
# so it and its response types live at the top level. The datastore, clock, and
# stack helpers are namespaced in their own submodules (imported below).
from ._peppylib.core_node import (  # noqa: E402  # type: ignore[import-not-found]
    ContainerInfo,
    InfoResponse,
    info,
)

# Namespaced core-node helper modules: `peppylib.datastore`, `peppylib.clock`,
# `peppylib.stack`. Imported after the sys.modules aliasing above so their
# `from ._peppylib.core_node import ...` re-exports resolve.
from . import datastore, clock, stack  # noqa: E402

__all__ = [
    "SenderTarget",
    "ProducerRef",
    "MessengerHandle",
    "TopicMessenger",
    "TopicPublisher",
    "ZenohdInstance",
    "QoSProfile",
    "ServiceMessenger",
    "ActionMessenger",
    "ConcurrentAction",
    "PendingGoal",
    "GoalContext",
    "NodeBuilder",
    "StandaloneConfig",
    "NodeRunner",
    "CancellationToken",
    "info",
    "InfoResponse",
    "ContainerInfo",
    "datastore",
    "clock",
    "stack",
    "messaging",
    "encoding",
    "__version__",
]
