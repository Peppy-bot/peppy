"""Integration tests for the `peppylib` datastore helpers
(`datastore.store` / `datastore.store_exclusive` / `datastore.get` /
`datastore.list` / `datastore.remove`).

Python equivalent of `peppy-shared/peppylib-rs/tests/core_node/datastore.rs`.
The stub listeners reply with canned capnp bytes, so these assert the bindings
route, encode, and decode correctly (and that `datastore.get` folds the
`found` flag into `None`); the full round-trip semantics are covered Rust-side.
"""

import pytest

from peppylib import datastore

from .common import call_against_stub_service


@pytest.mark.asyncio
async def test_datastore_store_returns_none_on_ack(tmp_path):
    """`datastore.store()` encodes the request, and resolves to `None` once the
    service acks (a timeout or decode failure would raise instead). Passing an
    `Encoding` member proves the StrEnum flows through the binding as a str."""
    result = await call_against_stub_service(
        tmp_path,
        "datastore_store",
        datastore.DatastoreStoreResponse().encode(),
        lambda runner: datastore.store(
            runner, "greeting", b"hello", datastore.Encoding.TEXT_PLAIN, 3.0
        ),
    )

    assert result is None


@pytest.mark.asyncio
async def test_datastore_get_returns_stored_value(tmp_path):
    """`datastore.get()` decodes a found response into a StoredValue with the
    raw (possibly non-UTF-8) bytes, the encoding tag, and the last writer's
    instance_id preserved."""
    response = datastore.DatastoreGetResponse(
        True, b"\x00\xff\x80\xfe", "application/octet-stream", "writer_node"
    )

    result = await call_against_stub_service(
        tmp_path,
        "datastore_get",
        response.encode(),
        lambda runner: datastore.get(runner, "blob", 3.0),
    )

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

    result = await call_against_stub_service(
        tmp_path,
        "datastore_get",
        response.encode(),
        lambda runner: datastore.get(runner, "never-stored", 3.0),
    )

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

    entries = await call_against_stub_service(
        tmp_path,
        "datastore_list",
        response.encode(),
        lambda runner: datastore.list(runner, 3.0),
    )

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
async def test_datastore_store_exclusive_resolves_to_stored(tmp_path):
    """`datastore.store_exclusive()` resolves to `ExclusiveStore.Stored()` when
    the core node makes the calling instance the key's owner."""
    answer = await call_against_stub_service(
        tmp_path,
        "datastore_store_exclusive",
        datastore.DatastoreStoreExclusiveResponse.stored().encode(),
        lambda runner: datastore.store_exclusive(
            runner, "arm/left/driver", b"mine", datastore.Encoding.TEXT_PLAIN, 3.0
        ),
    )

    assert isinstance(answer, datastore.ExclusiveStore.Stored)
    assert answer == datastore.ExclusiveStore.Stored(), "answers compare by value"


@pytest.mark.asyncio
async def test_datastore_store_exclusive_refused_names_the_owner(tmp_path):
    """A refusal carries the key as its owner stored it, the owner in
    `last_modified_by`, and matches as `ExclusiveStore.Refused`."""
    response = datastore.DatastoreStoreExclusiveResponse.refused(
        b"held", "text/plain", "owner_inst"
    )

    answer = await call_against_stub_service(
        tmp_path,
        "datastore_store_exclusive",
        response.encode(),
        lambda runner: datastore.store_exclusive(
            runner, "arm/left/driver", b"mine", datastore.Encoding.TEXT_PLAIN, 3.0
        ),
    )

    match answer:
        case datastore.ExclusiveStore.Refused(current=current):
            assert current.value == b"held"
            assert current.encoding == datastore.Encoding.TEXT_PLAIN
            assert current.last_modified_by == "owner_inst"
        case _:
            pytest.fail(f"expected a refusal, got {answer!r}")


@pytest.mark.asyncio
async def test_datastore_remove_returns_bool(tmp_path):
    """`datastore.remove()` decodes the remove response into a plain bool."""
    response = datastore.DatastoreRemoveResponse(True)

    removed = await call_against_stub_service(
        tmp_path,
        "datastore_remove",
        response.encode(),
        lambda runner: datastore.remove(runner, "mode", 3.0),
    )

    assert removed is True
