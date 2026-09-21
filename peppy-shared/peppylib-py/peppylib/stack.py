"""Stack inspection helpers: read the live node graph from the core node.

This module is the Python face of `peppylib::stack`. `list` polls the core
node's ``STACK_LIST`` service and returns a `StackList` (the node graph, the
copies the coordinator runs, its configured shutdown grace, and the serving
daemon's identity: core-node name, instance id, and hostname).
"""

from __future__ import annotations

from ._peppylib.core_node import (  # type: ignore[import-not-found]
    StackList,
    StackListResponse,
    stack_list as list,
)

__all__ = ["list", "StackList", "StackListResponse"]
