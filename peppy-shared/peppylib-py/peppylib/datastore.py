"""Datastore helpers: share small values across nodes through the core node's
in-memory key/value store.

This module is the Python face of `peppylib::datastore`. It exposes the five
operations (`store`, `store_exclusive`, `get`, `list`, `remove`), the value
types (`StoredValue`, `DatastoreEntry`), the answer of an exclusive store
(`ExclusiveStore`), and the `Encoding` vocabulary. The `Datastore*Response`
classes encode a core node's answers, for test stubs that stand in for it.

`Encoding` is the Python mirror of `peppylib::datastore::Encoding`: a set of
Zenoh-style content-type tags describing how a stored value's bytes should be
interpreted. The members below cover the common cases, but the set is **open**:
because `Encoding` subclasses `str`, any arbitrary tag (e.g. ``"application/cbor"``)
is still accepted by `store`. The datastore treats the tag as an opaque label and
never interprets it.
"""

from __future__ import annotations

from enum import StrEnum

from ._peppylib.core_node import (  # type: ignore[import-not-found]
    DatastoreEntry,
    DatastoreGetResponse,
    DatastoreListResponse,
    DatastoreRemoveResponse,
    DatastoreStoreExclusiveResponse,
    DatastoreStoreResponse,
    ExclusiveStore,
    StoredValue,
)
from ._peppylib.core_node import datastore_get as get
from ._peppylib.core_node import datastore_list as list
from ._peppylib.core_node import datastore_remove as remove
from ._peppylib.core_node import datastore_store as store
from ._peppylib.core_node import datastore_store_exclusive as store_exclusive


class Encoding(StrEnum):
    """Well-known datastore value encodings.

    Each member *is* a ``str``, so it can be passed straight to
    ``store(..., Encoding.APPLICATION_JSON, ...)`` and compares equal to the
    matching raw tag returned by ``StoredValue.encoding``
    (``stored.encoding == Encoding.APPLICATION_JSON``). Arbitrary tags outside
    this set are equally valid: pass any string.
    """

    TEXT_PLAIN = "text/plain"
    APPLICATION_JSON = "application/json"
    APPLICATION_OCTET_STREAM = "application/octet-stream"


__all__ = [
    "DatastoreEntry",
    "DatastoreGetResponse",
    "DatastoreListResponse",
    "DatastoreRemoveResponse",
    "DatastoreStoreExclusiveResponse",
    "DatastoreStoreResponse",
    "Encoding",
    "ExclusiveStore",
    "StoredValue",
    "get",
    "list",
    "remove",
    "store",
    "store_exclusive",
]
