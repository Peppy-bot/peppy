"""Datastore helpers: share small values across nodes through the core node's
in-memory key/value store.

This module is the Python face of `peppylib::datastore`. It exposes the four
operations (`store`, `get`, `list`, `remove`), the value types (`StoredValue`,
`DatastoreEntry`), and the `Encoding` vocabulary.

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
    DatastoreStoreResponse,
    StoredValue,
    datastore_get as get,
    datastore_list as list,
    datastore_remove as remove,
    datastore_store as store,
)


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
    "store",
    "get",
    "list",
    "remove",
    "Encoding",
    "StoredValue",
    "DatastoreEntry",
    "DatastoreStoreResponse",
    "DatastoreGetResponse",
    "DatastoreListResponse",
    "DatastoreRemoveResponse",
]
