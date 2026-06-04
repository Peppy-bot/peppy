"""Integration tests for the `peppylib` datastore helpers
(`datastore.store` / `datastore.get` / `datastore.list` / `datastore.remove`).

Python equivalent of `crates/peppylib/tests/core_node/datastore.rs`. The stub
listeners reply with canned capnp bytes, so these assert the bindings route,
encode, and decode correctly (and that `datastore.get` folds the `found` flag
into `None`); the full round-trip semantics are covered Rust-side.
"""

import pytest

from peppylib import datastore

from .common import spawn_stub_listener, start_router_and_runner, wait_until_reachable


@pytest.mark.asyncio
async def test_datastore_store_returns_none_on_ack(tmp_path):
    """`datastore.store()` encodes the request, and resolves to `None` once the
    service acks (a timeout or decode failure would raise instead). Passing an
    `Encoding` member proves the StrEnum flows through the binding as a str."""
    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_store", datastore.DatastoreStoreResponse().encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_store")

        result = await datastore.store(
            node_runner, "greeting", b"hello", datastore.Encoding.TEXT_PLAIN, 3.0
        )

        await handler
    finally:
        await router.stop()

    assert result is None


@pytest.mark.asyncio
async def test_datastore_get_returns_stored_value(tmp_path):
    """`datastore.get()` decodes a found response into a StoredValue with the
    raw (possibly non-UTF-8) bytes, the encoding tag, and the last writer's
    instance_id preserved."""
    response = datastore.DatastoreGetResponse(
        True, b"\x00\xff\x80\xfe", "application/octet-stream", "writer_node"
    )

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_get", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_get")

        result = await datastore.get(node_runner, "blob", 3.0)

        await handler
    finally:
        await router.stop()

    assert result is not None
    assert result.value == b"\x00\xff\x80\xfe"
    assert result.encoding == "application/octet-stream"
    # The raw tag compares equal to the matching Encoding member.
    assert result.encoding == datastore.Encoding.APPLICATION_OCTET_STREAM
    assert result.last_modified_by == "writer_node"


@pytest.mark.asyncio
async def test_datastore_get_missing_returns_none(tmp_path):
    """A not-found response folds into `None`."""
    response = datastore.DatastoreGetResponse(False, b"", "", "")

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_get", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_get")

        result = await datastore.get(node_runner, "never-stored", 3.0)

        await handler
    finally:
        await router.stop()

    assert result is None


@pytest.mark.asyncio
async def test_datastore_list_returns_entries(tmp_path):
    """`datastore.list()` decodes a list response into `datastore.DatastoreEntry` objects
    exposing key, encoding, and the last writer's instance_id (no value bytes)."""
    response = datastore.DatastoreListResponse(
        [
            ("mode", "text/plain", "planner"),
            ("calibration", "application/json", "arm_node"),
        ]
    )

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_list", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_list")

        entries = await datastore.list(node_runner, 3.0)

        await handler
    finally:
        await router.stop()

    entries.sort(key=lambda e: e.key)
    assert len(entries) == 2
    assert all(isinstance(e, datastore.DatastoreEntry) for e in entries)
    assert entries[0].key == "calibration"
    assert entries[0].encoding == "application/json"
    assert entries[0].encoding == datastore.Encoding.APPLICATION_JSON
    assert entries[0].last_modified_by == "arm_node"
    assert entries[1].key == "mode"
    assert entries[1].last_modified_by == "planner"


@pytest.mark.asyncio
async def test_datastore_remove_returns_bool(tmp_path):
    """`datastore.remove()` decodes the remove response into a plain bool."""
    response = datastore.DatastoreRemoveResponse(True)

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "datastore_remove", response.encode()
        )
        await wait_until_reachable(node_runner.messenger(), "datastore_remove")

        removed = await datastore.remove(node_runner, "mode", 3.0)

        await handler
    finally:
        await router.stop()

    assert removed is True
