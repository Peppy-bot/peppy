"""Integration tests for `peppylib.stack_list`.

Python equivalent of `crates/peppylib/tests/core_node/stack.rs`.
"""

import json

import pytest

from peppylib import stack_list
from peppylib.core_node import StackListResponse

from .common import spawn_stub_listener, start_router_and_runner, wait_until_reachable


def _sample_graph_json() -> str:
    """Matches the two-node brain→sensor graph used in the Rust stack tests."""
    brain = {
        "name": "brain",
        "tag": "0.1.0",
        "config_path": "/tmp/brain.json5",
        "artifact_path": None,
        "stage": "Ready",
        "instances": [{"instance_id": "i1", "state": "running"}],
    }
    sensor = {
        "name": "sensor",
        "tag": "0.1.0",
        "config_path": "/tmp/sensor.json5",
        "artifact_path": None,
        "stage": "Added",
        "instances": [],
    }
    graph = {
        "nodes": [brain, sensor],
        "edges": [{"from": brain, "to": sensor}],
    }
    return json.dumps(graph)


@pytest.mark.asyncio
async def test_stack_list_parses_graph_and_includes_dot_graph_when_requested(tmp_path):
    """`stack_list(..., with_dot_graph=True)` returns both graph and dot_graph."""
    graph_json = _sample_graph_json()
    response_bytes = StackListResponse(graph_json, "digraph {}").encode()

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "stack_list", response_bytes
        )
        await wait_until_reachable(node_runner.messenger(), "stack_list")

        result = await stack_list(node_runner, True, 3.0)

        await handler
    finally:
        await router.stop()

    graph = result.graph
    assert [n["name"] for n in graph["nodes"]] == ["brain", "sensor"]
    brain = next(n for n in graph["nodes"] if n["name"] == "brain")
    assert brain["stage"] == "Ready"
    assert len(brain["instances"]) == 1
    assert brain["instances"][0]["instance_id"] == "i1"
    assert brain["instances"][0]["state"] == "running"
    assert len(graph["edges"]) == 1
    assert graph["edges"][0]["from"]["name"] == "brain"
    assert graph["edges"][0]["to"]["name"] == "sensor"
    assert result.dot_graph == "digraph {}"


@pytest.mark.asyncio
async def test_stack_list_returns_none_dot_graph_when_not_requested(tmp_path):
    """`stack_list(..., with_dot_graph=False)` leaves dot_graph as None."""
    graph_json = _sample_graph_json()
    response_bytes = StackListResponse(graph_json, None).encode()

    router, node_runner, server_handle = await start_router_and_runner(tmp_path)
    try:
        handler = await spawn_stub_listener(
            server_handle, "stack_list", response_bytes
        )
        await wait_until_reachable(node_runner.messenger(), "stack_list")

        result = await stack_list(node_runner, False, 3.0)

        await handler
    finally:
        await router.stop()

    brain = next(n for n in result.graph["nodes"] if n["name"] == "brain")
    assert brain["stage"] == "Ready"
    assert brain["instances"][0]["state"] == "running"
    assert result.dot_graph is None
