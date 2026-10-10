"""
peppylib - The Peppy control library
"""

import sys

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

__version__ = _peppylib.__version__

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

# Re-export the Rust-implemented functions/types from the native module, and
# the namespaced core-node helper modules: `peppylib.datastore`,
# `peppylib.clock`, `peppylib.stack`. They are imported after the sys.modules
# aliasing above so their `from ._peppylib.core_node import ...` re-exports
# resolve.
from . import clock, datastore, stack
from ._peppylib.config import (  # type: ignore[import-not-found]
    QoSProfile,
    TopicRetention,
)

# `info` is the one core-node helper that stays flat (a single verb-less call),
# so it and its response types live at the top level. The datastore, clock, and
# stack helpers are namespaced in their own submodules (imported above).
from ._peppylib.core_node import (  # type: ignore[import-not-found]
    ContainerInfo,
    InfoResponse,
    info,
)
from ._peppylib.messaging import (  # type: ignore[import-not-found]
    BoundMember,
    BoundSetSubscription,
    CopyTag,
    MessengerHandle,
    ObservationSlot,
    ObservationSlotSet,
    ObservedSource,
    ObservedSubscription,
    PeerInfo,
    PeerMember,
    PeerPublisher,
    PeerSlot,
    PeerSlotSet,
    PeerSubscription,
    ProducerRef,
    SenderTarget,
    TopicMessenger,
    TopicPublisher,
    ZenohdInstance,
)
from ._peppylib.messaging.actions import (  # type: ignore[import-not-found]
    ActionMessenger,
    ConcurrentAction,
    GoalContext,
    PendingGoal,
)
from ._peppylib.messaging.services import (  # type: ignore[import-not-found]
    ServiceMessenger,
)
from ._peppylib.runtime import (  # type: ignore[import-not-found]
    CancellationToken,
    NodeBuilder,
    NodeRunner,
    StandaloneConfig,
)

__all__ = [
    "ActionMessenger",
    "BoundMember",
    "BoundSetSubscription",
    "CancellationToken",
    "ConcurrentAction",
    "ContainerInfo",
    "CopyTag",
    "GoalContext",
    "InfoResponse",
    "MessengerHandle",
    "NodeBuilder",
    "NodeRunner",
    "ObservationSlot",
    "ObservationSlotSet",
    "ObservedSource",
    "ObservedSubscription",
    "PeerInfo",
    "PeerMember",
    "PeerPublisher",
    "PeerSlot",
    "PeerSlotSet",
    "PeerSubscription",
    "PendingGoal",
    "ProducerRef",
    "QoSProfile",
    "SenderTarget",
    "ServiceMessenger",
    "StandaloneConfig",
    "TopicMessenger",
    "TopicPublisher",
    "TopicRetention",
    "ZenohdInstance",
    "__version__",
    "clock",
    "datastore",
    "encoding",
    "info",
    "messaging",
    "stack",
]
