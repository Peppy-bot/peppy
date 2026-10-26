"""The codec API of peppy: calls to contracts that a node was not built with.

A node gets this module when its manifest sets ``execution.codec_api: true``.
It parses a pinned contract document (:class:`Contract`), converts a message
of any format between a ``dict`` of canonical values and the wire bytes of a
generated node (:class:`MessageCodec`), and calls any service, action and
topic of a contract on one producer (:class:`ServiceClient`,
:class:`ActionClient`, :class:`TopicConsumer`).

The codec lays out each message format with the Cap'n Proto compiler. Peppy
deploys the compiler beside this module (``capnp_compiler``) for a node that
asks for the codec API; ``CAPNP_BINARY_PATH`` names another compiler.
"""

from pathlib import Path

from ._peppylib import codec as _codec  # type: ignore[import-not-found]
from ._peppylib.codec import (  # type: ignore[import-not-found]
    ActionClient,
    CodecError,
    Contract,
    ContractError,
    ConversionError,
    GoalHandle,
    GoalOutcome,
    MessageCodec,
    ServiceClient,
    TopicConsumer,
    UndeclaredFailureKindError,
)

# The file name the code generator of peppy gives the compiler.
_COMPILER = Path(__file__).with_name("capnp_compiler")
if _COMPILER.is_file():
    _codec.carry_compiler_from_file(str(_COMPILER))

__all__ = [
    "ActionClient",
    "CodecError",
    "Contract",
    "ContractError",
    "ConversionError",
    "GoalHandle",
    "GoalOutcome",
    "MessageCodec",
    "ServiceClient",
    "TopicConsumer",
    "UndeclaredFailureKindError",
]
