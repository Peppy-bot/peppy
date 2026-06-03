"""Datastore value-encoding vocabulary.

`Encoding` is the Python mirror of `peppylib::Encoding` on the Rust side: a set
of Zenoh-style content-type tags describing how a stored value's bytes should
be interpreted. The members below cover the common cases, but the set is
**open** — because `Encoding` subclasses `str`, any arbitrary tag (e.g.
``"application/cbor"``) is still accepted by `datastore_store`. The datastore
treats the tag as an opaque label and never interprets it.
"""

from __future__ import annotations

from enum import StrEnum


class Encoding(StrEnum):
    """Well-known datastore value encodings.

    Each member *is* a ``str``, so it can be passed straight to
    ``datastore_store(..., Encoding.APPLICATION_JSON, ...)`` and compares equal
    to the matching raw tag returned by ``StoredValue.encoding``
    (``stored.encoding == Encoding.APPLICATION_JSON``). Arbitrary tags outside
    this set are equally valid — pass any string.
    """

    TEXT_PLAIN = "text/plain"
    APPLICATION_JSON = "application/json"
    APPLICATION_OCTET_STREAM = "application/octet-stream"
