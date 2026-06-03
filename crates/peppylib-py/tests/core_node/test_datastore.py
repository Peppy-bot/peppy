"""Integration tests for `peppylib.datastore_store` / `peppylib.datastore_get`.

Python equivalent of `crates/peppylib/tests/core_node/datastore.rs`. The stub
listeners reply with canned capnp bytes, so these assert the bindings route,
encode, and decode correctly (and that `datastore_get` folds the `found` flag
into `None`); the full store/get round-trip semantics are covered Rust-side.
"""

import pytest

from peppylib import datastore_get, datastore_store
from peppylib.core_node import DatastoreGetResponse, DatastoreStoreResponse

from .common import spawn_stub_listener, start_router_and_runner, wait_until_reachable


@pytest.mark.asyncio
async def test_datastore_store_returns_none_on_ack(tmp_path):
    """`datastore_store()` encodes the request, and resolves to `None` once the
    service acks (a timeout or decode failure would raise instead)."""
    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_store", DatastoreStoreResponse().encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_store")

        result = await datastore_store(node_runner, "greeting", b"hello", "text/plain", 3.0)

        await handler
    finally:
        await router.stop()

    assert result is None


@pytest.mark.asyncio
async def test_datastore_get_returns_stored_value(tmp_path):
    """`datastore_get()` decodes a found response into a StoredValue with the
    raw (possibly non-UTF-8) bytes and the encoding tag preserved."""
    response = DatastoreGetResponse(True, b"\x00\xff\x80\xfe", "application/octet-stream")

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_get", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_get")

        result = await datastore_get(node_runner, "blob", 3.0)

        await handler
    finally:
        await router.stop()

    assert result is not None
    assert result.value == b"\x00\xff\x80\xfe"
    assert result.encoding == "application/octet-stream"


@pytest.mark.asyncio
async def test_datastore_get_missing_returns_none(tmp_path):
    """A not-found response folds into `None`."""
    response = DatastoreGetResponse(False, b"", "")

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_get", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_get")

        result = await datastore_get(node_runner, "never-stored", 3.0)

        await handler
    finally:
        await router.stop()

    assert result is None
